//! The `nvx-time test` compatibility interface: the C program's test hooks, which exercise its
//! decoders, formats, and process structure without the hypervisor.
//!
//! A hook prints a decoded failure as its output and succeeds; it fails with the usage status
//! only when its own command line is malformed. Its numeric arguments keep the C program's
//! `strtoll` and `strtoull` reading, except the CPUID values of `exhaustive-leaf`, which are
//! plain decimal or `0x`-prefixed hexadecimal, as other guest programs read their arguments.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Condvar, Mutex, mpsc};

use super::daemon::{self, Promoter};
use super::decoders::{
    self, GENERATION_ID_SIZE, PACKET_HEADER_SIZE, PACKET_MAX_SIZE, RestorePacket, SAMPLE_SIZE,
    TimeSample,
};
use super::events::{DETAIL_MAX, escape_text, format_event};
use super::state::TimeState;
use super::system::{clock_ns, cpu_time_ns};
use super::{DEFERRED_START_NS, IDLE_BOUND_NS, MAX_CPUS, NSEC_PER_MSEC, NSEC_PER_SEC, Phase};
use crate::guest::status::CommandError;
use crate::guest::sys::{self, ChildStatus, ClockAdjustment, Errno, ForkResult};

/// Maximum compatibility-test input file size.
const TEXT_MAX: usize = 65_536;
/// Maximum size of a state, status, or restore-record fixture, as the C program's buffer.
const RECORD_MAX: usize = 4096;
/// How long after its start the `promote` hook's first promoter promotes the threads.
const PROMOTION_DELAY_NS: i64 = 50 * NSEC_PER_MSEC;
/// How long after its start the `promote` hook's second promoter would promote the threads,
/// long enough that stopping it first cancels the promotion.
const CANCELLED_PROMOTION_DELAY_NS: i64 = 10 * NSEC_PER_SEC;

/// Runs a named test hook. The C interface exits 0 for a decoded failure and 2 only when the
/// hook's invocation itself is malformed.
pub(super) fn run(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    let Some((&name, arguments)) = arguments.split_first() else {
        return Err(CommandError::Usage);
    };
    match name {
        b"packet" => test_packet(arguments, out),
        b"sample" => test_sample(arguments, out),
        b"pair" => test_pair(arguments, out),
        b"kmsg" => test_lines(arguments, LineMode::Kmsg, out),
        b"boot-log" => test_lines(arguments, LineMode::BootLog, out),
        b"syslog" => test_lines(arguments, LineMode::Syslog, out),
        b"console" => test_console(arguments, out),
        b"exhaustive-leaf" => test_exhaustive_leaf(arguments, out),
        b"exhaustive-report" => test_exhaustive_report(arguments, out),
        b"exhaustive-summary" => test_exhaustive_summary(arguments, out),
        b"timer-list" => test_timer_list(arguments, out),
        b"correction" => test_correction(arguments, out),
        b"frequency" => test_frequency(arguments, out),
        b"event" => test_event(arguments, out),
        b"state" => test_state(arguments, out),
        b"status" => test_status(arguments, out),
        b"restore-record" => test_restore_record(arguments, out),
        b"deferred-restore-cpu" => test_deferred_restore_cpu(arguments, out),
        b"deferral" if arguments.is_empty() => {
            let _ = writeln!(
                out,
                "deferred_start_ms={} idle_bound_ms={}",
                DEFERRED_START_NS / NSEC_PER_MSEC,
                IDLE_BOUND_NS / NSEC_PER_MSEC
            );
            Ok(())
        }
        b"parallel" => test_parallel(arguments, out),
        b"promote" if arguments.is_empty() => test_promote(out),
        b"idle-priority" if arguments.is_empty() => test_idle_priority(out),
        b"cpuinfo" => test_cpuinfo(arguments, out),
        b"cmdline" => test_cmdline(arguments, out),
        b"clocksource" => test_clocksource(arguments, out),
        _ => Err(CommandError::Usage),
    }
}

/// Checks that a hook received exactly `count` arguments.
fn expect(arguments: &[&[u8]], count: usize) -> Result<(), CommandError> {
    if arguments.len() == count {
        Ok(())
    } else {
        Err(CommandError::Usage)
    }
}

/// Reads at most `size` bytes of a test fixture, and either extends them with zero bytes to
/// `size`, or drops the last byte of a fixture that fills `size`, as C's NUL did.
fn read_file(path: &[u8], size: usize, zero_fill: bool) -> Result<Vec<u8>, CommandError> {
    let file = fs::File::open(OsStr::from_bytes(path)).map_err(|_| CommandError::Usage)?;
    let mut bytes = Vec::with_capacity(size);
    file.take(size as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CommandError::Usage)?;
    if zero_fill {
        bytes.resize(size, 0);
    } else if bytes.len() == size {
        bytes.truncate(size.saturating_sub(1));
    }
    Ok(bytes)
}

/// Decodes and prints one synthetic restore packet.
fn test_packet(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 3)?;
    let bytes = read_file(arguments[0], PACKET_MAX_SIZE, true)?;
    let previous =
        decoders::parse_hex(arguments[2], GENERATION_ID_SIZE).ok_or(CommandError::Usage)?;
    let recorded = decoders::parse_u64_lossy(arguments[1]);
    let mut packet = match RestorePacket::parse_header(&bytes) {
        Ok(packet) => packet,
        Err(detail) => {
            let _ = writeln!(out, "error G_REPAIR_PACKET {detail}");
            return Ok(());
        }
    };
    if u64::from(packet.generation) == recorded {
        let _ = writeln!(out, "stale");
        return Ok(());
    }
    if u64::from(packet.generation) != recorded.wrapping_add(1) {
        let _ = writeln!(
            out,
            "error G_REPAIR_GENERATION generation {}",
            packet.generation
        );
        return Ok(());
    }
    if let Err(detail) = packet.parse_body(&bytes[PACKET_HEADER_SIZE..]) {
        let _ = writeln!(out, "error G_REPAIR_PACKET {detail}");
        return Ok(());
    }
    if packet.entropy[..GENERATION_ID_SIZE] == previous {
        let _ = writeln!(out, "error G_REPAIR_GENERATION generation ID unchanged");
        return Ok(());
    }
    let _ = write!(
        out,
        "ok flags={} online={} ranges={} generation={} rate_deviation={} \
         frequency={} downtime_ns={} utc_ns={} generation_id={}",
        packet.flags,
        packet.online_vp_count,
        packet.range_count,
        packet.generation,
        packet.rate_deviation,
        decoders::restore_frequency(packet.rate_deviation),
        packet.downtime_ns,
        packet.utc_ns,
        decoders::format_hex(&packet.entropy[..GENERATION_ID_SIZE])
    );
    for (start, length) in packet.ranges {
        let _ = write!(out, " range={start}:{length}");
    }
    let _ = writeln!(out);
    Ok(())
}

/// Decodes and prints one synthetic time sample.
fn test_sample(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    let bytes = read_file(arguments[0], SAMPLE_SIZE, true)?;
    match TimeSample::parse(&bytes) {
        Err(detail) => {
            let _ = writeln!(out, "error {detail}");
        }
        Ok(sample) if u64::from(sample.generation) != decoders::parse_u64_lossy(arguments[1]) => {
            let _ = writeln!(out, "error generation {}", sample.generation);
        }
        Ok(sample) => {
            let _ = writeln!(
                out,
                "ok flags={} generation={} utc_ns={}",
                sample.flags, sample.generation, sample.utc_ns
            );
        }
    }
    Ok(())
}

/// Computes and prints a synthetic clock pairing.
fn test_pair(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 3)?;
    let (theta, epsilon) = decoders::pair_clocks(
        decoders::parse_i64_lossy(arguments[0]),
        decoders::parse_i64_lossy(arguments[1]),
        decoders::parse_u64_lossy(arguments[2]),
    );
    let _ = writeln!(out, "theta_ns={theta} epsilon_ns={epsilon}");
    Ok(())
}

#[derive(Clone, Copy)]
/// Line-oriented decoder selected by the compatibility test.
enum LineMode {
    /// Match runtime kernel-message watch rules.
    Kmsg,
    /// Scan boot-log evidence.
    BootLog,
    /// Strip one syslog record's metadata.
    Syslog,
}

/// Runs one line-oriented decoder over a fixture.
fn test_lines(
    arguments: &[&[u8]],
    mode: LineMode,
    out: &mut dyn Write,
) -> Result<(), CommandError> {
    expect(arguments, 1)?;
    let text = read_file(arguments[0], TEXT_MAX, false)?;
    if matches!(mode, LineMode::Kmsg) {
        let text = text.split(|byte| *byte == 0).next().unwrap_or_default();
        for line in text
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let code = decoders::match_watch_rule(line).unwrap_or("none");
            let _ = writeln!(out, "{code}");
        }
        return Ok(());
    }
    let log = decoders::scan_boot_log(&text, matches!(mode, LineMode::Syslog));
    match log.verdict() {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(&[b"fail ".as_slice(), &detail, b"\n"].concat());
        }
    }
    if !log.wx.is_empty() {
        let _ = out.write_all(&[b"wx ".as_slice(), &log.wx, b"\n"].concat());
    }
    Ok(())
}

/// Reproduces synchronous or asynchronous console byte output.
fn test_console(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    if arguments[1] == b"async" {
        let _ = out.write_all(b"\n");
    }
    let _ = out.write_all(arguments[0]);
    Ok(())
}

/// Parses a CPUID value as plain decimal or `0x`-prefixed hexadecimal, as `parse_u64` reads
/// the guest programs' arguments, and narrows it with C's `uint32_t` cast. Any other text is
/// zero, as `strtoul` returned for text without digits.
fn cpuid_value(text: &[u8]) -> u32 {
    CString::new(text)
        .ok()
        .and_then(|text| super::super::parse::parse_u64(&text))
        .unwrap_or(0) as u32
}

/// Validates one synthetic exhaustive CPUID leaf.
fn test_exhaustive_leaf(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 9)?;
    let mut value = [0; 4];
    let mut out_of_range = [0; 4];
    for index in 0..4 {
        value[index] = cpuid_value(arguments[index + 1]);
        out_of_range[index] = cpuid_value(arguments[index + 5]);
    }
    match decoders::exhaustive_leaf_ok(cpuid_value(arguments[0]), value, out_of_range) {
        Ok(()) => {
            let _ = writeln!(out, "pass");
        }
        Err(detail) => {
            let _ = writeln!(out, "fail {detail}");
        }
    }
    Ok(())
}

/// Formats one exhaustive subcheck report.
fn test_exhaustive_report(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 4)?;
    let passed = arguments[2] == b"pass";
    let escaped = escape_text(arguments[3], DETAIL_MAX - 1);
    let _ = write!(
        out,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 check={} cpu={} status={} detail=\"",
        String::from_utf8_lossy(arguments[0]),
        decoders::parse_i64_lossy(arguments[1]),
        if passed { "pass" } else { "fail" }
    );
    let _ = out.write_all(&escaped);
    let _ = writeln!(out, "\"");
    let _ = writeln!(out, "failures={}", usize::from(!passed));
    Ok(())
}

/// Formats one exhaustive final summary.
fn test_exhaustive_summary(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    let online = decoders::parse_i64_lossy(arguments[0]);
    let failures = decoders::parse_i64_lossy(arguments[1]);
    let status = if failures == 0 { "ok" } else { "fail" };
    let _ = writeln!(
        out,
        "NVX-TIME-ABI-EXHAUSTIVE: v=1 status={status} cpus={online} failures={failures}"
    );
    let _ = writeln!(out, "exit={}", i32::from(failures != 0));
    Ok(())
}

/// Parses a CPU list that must name at least one CPU.
fn selected_cpus(text: &[u8]) -> Result<[bool; MAX_CPUS], CommandError> {
    decoders::parse_cpu_list(text)
        .filter(|(_, count)| *count > 0)
        .map(|(cpus, _)| cpus)
        .ok_or(CommandError::Usage)
}

/// Validates a synthetic `/proc/timer_list` fixture.
fn test_timer_list(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    let text = read_file(arguments[0], TEXT_MAX, false)?;
    let cpus = selected_cpus(arguments[1])?;
    match decoders::timer_list_verdict(&text, &cpus) {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(&[b"fail ".as_slice(), &detail, b"\n"].concat());
        }
    }
    Ok(())
}

/// Plans and prints one step or slew correction, with the `adjtimex` modes and status that the
/// system layer would apply.
fn test_correction(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 3)?;
    let correction = decoders::plan_correction(
        decoders::parse_i64_lossy(arguments[0]),
        decoders::parse_i64_lossy(arguments[1]),
        arguments[2] == b"fast",
    );
    let modes = correction.modes();
    match correction {
        ClockAdjustment::Step {
            seconds,
            nanoseconds,
        } => {
            let _ = writeln!(
                out,
                "step modes=0x{modes:x} sec={seconds} nsec={nanoseconds}"
            );
        }
        ClockAdjustment::Slew {
            offset_ns,
            time_constant,
            maximum_error_us,
            estimated_error_us,
        } => {
            let _ = writeln!(
                out,
                "slew modes=0x{modes:x} offset={offset_ns} status=0x{:x} \
                 constant={time_constant} maxerror={maximum_error_us} \
                 esterror={estimated_error_us}",
                correction.status()
            );
        }
        ClockAdjustment::Query | ClockAdjustment::Frequency { .. } => {}
    }
    Ok(())
}

/// Converts one frequency value to parts per billion.
fn test_frequency(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 1)?;
    let frequency = decoders::restore_frequency(decoders::parse_i64_lossy(arguments[0]) as i32);
    let _ = writeln!(
        out,
        "frequency={frequency} ppb={}",
        decoders::frequency_to_ppb(frequency)
    );
    Ok(())
}

/// Formats one synthetic time-ABI violation event.
fn test_event(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 6)?;
    let phase = match arguments[2] {
        b"boot" => Phase::Boot,
        b"capture" => Phase::Capture,
        b"restore" => Phase::Restore,
        _ => Phase::Runtime,
    };
    let line = format_event(
        false,
        arguments[0],
        arguments[1],
        phase,
        decoders::parse_u64_lossy(arguments[3]) as u32,
        decoders::parse_i64_lossy(arguments[4]),
        arguments[5],
    );
    let _ = out.write_all(&line);
    Ok(())
}

/// Parses and re-formats one state fixture.
fn test_state(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 1)?;
    let text = read_file(arguments[0], RECORD_MAX, false)?;
    match TimeState::parse(&text) {
        Some(state) => {
            let _ = out.write_all(&state.format());
        }
        None => {
            let _ = writeln!(out, "error");
        }
    }
    Ok(())
}

/// Formats status from one state fixture.
fn test_status(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    let text = read_file(arguments[0], RECORD_MAX, false)?;
    let state = TimeState::parse(&text).ok_or(CommandError::Usage)?;
    let (text, ok) = state.format_status(arguments[1] == b"report-only");
    let _ = out.write_all(&text);
    let _ = writeln!(out, "ok={}", u8::from(ok));
    Ok(())
}

/// Parses and prints one private restore record.
fn test_restore_record(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 1)?;
    let text = read_file(arguments[0], RECORD_MAX, false)?;
    let Some((generation, cpus, count)) = decoders::parse_restore_record(&text) else {
        let _ = writeln!(out, "error");
        return Ok(());
    };
    let _ = write!(out, "generation={generation} new_cpus=");
    let mut separator = "";
    for (cpu, selected) in cpus.iter().copied().enumerate() {
        if selected {
            let _ = write!(out, "{separator}{cpu}");
            separator = ",";
        }
    }
    if count == 0 {
        let _ = write!(out, "none");
    }
    let _ = writeln!(out, " count={count}");
    Ok(())
}

/// Formats a report-only conformance failure event.
fn failure_event(id: &str, detail: &[u8]) -> Vec<u8> {
    let code = format!("G_CONFORMANCE_{id}");
    format_event(
        true,
        code.as_bytes(),
        b"conformance",
        Phase::Boot,
        0,
        clock_ns(sys::Clock::Boottime),
        detail,
    )
}

/// Validates and summarizes a synthetic cpuinfo fixture.
fn test_cpuinfo(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    let text = read_file(arguments[0], TEXT_MAX, false)?;
    let selected = selected_cpus(arguments[1])?;
    let (cpuinfo, verdict) = decoders::check_cpuinfo(&text, &selected, true);
    match verdict {
        Ok(()) => {
            let _ = write!(out, "ok");
            for cpu in (0..MAX_CPUS).filter(|&cpu| cpuinfo.known[cpu]) {
                let _ = write!(out, " cpu{cpu}_khz={}", cpuinfo.khz[cpu]);
            }
            let _ = writeln!(out);
        }
        Err(detail) => {
            let _ = out.write_all(&failure_event("C5", detail.as_bytes()));
        }
    }
    Ok(())
}

/// Validates a synthetic kernel command line.
fn test_cmdline(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 1)?;
    match decoders::check_cmdline(arguments[0]) {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(&failure_event("C9", &detail));
        }
    }
    Ok(())
}

/// Validates synthetic current and available clocksources.
fn test_clocksource(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 2)?;
    match decoders::check_clocksource(arguments[0], arguments[1]) {
        Ok(()) => {
            let _ = writeln!(out, "ok");
        }
        Err(detail) => {
            let _ = out.write_all(&failure_event("C6", &detail));
        }
    }
    Ok(())
}

/// Exercises deterministic ordering of parallel check reports.
fn test_parallel(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 1)?;
    let selected = selected_cpus(arguments[0])?;
    let mut failures = 0;
    for side in 1..=3 {
        for index in 1..=side {
            let detail = format!("side job failure {index}");
            let _ = out.write_all(&failure_event("C4", detail.as_bytes()));
            failures += 1;
        }
    }
    let _ = out.write_all(&failure_event("C9", b"foreground failure"));
    failures += 1;
    for (cpu, selected) in selected.iter().copied().enumerate() {
        if !selected {
            continue;
        }
        let pinned = std::thread::spawn(move || sys::pin_current_thread(cpu))
            .join()
            .unwrap_or(Err(Errno::EINVAL));
        let Err(error) = pinned else {
            continue;
        };
        let detail = format!("cannot run on cpu {cpu}: {}", error.desc());
        let _ = out.write_all(&failure_event("C1", detail.as_bytes()));
        failures += 1;
        break;
    }
    let _ = writeln!(out, "failures={failures}");
    Ok(())
}

/// A test thread that reports its thread ID and waits until it is told to end.
struct WaitingThread {
    /// Whether the thread may end, and the condition that announces it.
    done: Arc<(Mutex<bool>, Condvar)>,
    /// The running thread.
    thread: std::thread::JoinHandle<()>,
}

impl WaitingThread {
    /// Starts the thread and returns it with its thread ID.
    fn start() -> Result<(Self, sys::ProcessId), CommandError> {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let wait = Arc::clone(&done);
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = sender.send(sys::current_thread());
            let (lock, changed) = &*wait;
            if let Ok(done) = lock.lock() {
                let _done = changed.wait_while(done, |done| !*done);
            }
        });
        let id = receiver.recv().map_err(|_| CommandError::Operation)?;
        Ok((Self { done, thread }, id))
    }

    /// Tells the thread to end, and joins it.
    fn end(self) -> Result<(), CommandError> {
        {
            let (lock, changed) = &*self.done;
            if let Ok(mut done) = lock.lock() {
                *done = true;
                changed.notify_all();
            }
        }
        self.thread.join().map_err(|_| CommandError::Operation)
    }
}

/// Starts deferred work whose promoter promotes it after `delay_ns`, in the monotonic `now`.
fn start_promoter(now: i64, delay_ns: i64) -> Result<Promoter, CommandError> {
    Promoter::start(now.wrapping_sub(IDLE_BOUND_NS).wrapping_add(delay_ns))
        .ok_or(CommandError::Operation)
}

/// Exercises the promoter: one that promotes an idle caller and the thread that it started,
/// and one that its deferred work stops before its deadline.
fn test_promote(out: &mut dyn Write) -> Result<(), CommandError> {
    let (caller, thread) = {
        let promoter = start_promoter(clock_ns(sys::Clock::Monotonic), PROMOTION_DELAY_NS)?;
        let _idle = sys::IdlePriority::enter();
        let (waiting, id) = WaitingThread::start()?;
        promoter.finish();
        let policies = (
            sys::scheduling_policy(None),
            sys::scheduling_policy(Some(id)),
        );
        waiting.end()?;
        policies
    };
    let _ = writeln!(out, "promoted caller={caller} thread={thread}");

    let start = clock_ns(sys::Clock::Monotonic);
    let promoter = start_promoter(start, CANCELLED_PROMOTION_DELAY_NS)?;
    let _idle = sys::IdlePriority::enter();
    promoter.stop();
    let _ = writeln!(
        out,
        "stopped caller={} stop_ms={}",
        sys::scheduling_policy(None),
        clock_ns(sys::Clock::Monotonic).wrapping_sub(start) / NSEC_PER_MSEC
    );
    Ok(())
}

/// Exercises direct idle-to-normal scheduler transitions.
fn test_idle_priority(out: &mut dyn Write) -> Result<(), CommandError> {
    let idle = {
        let _idle = sys::IdlePriority::enter();
        sys::scheduling_policy(None)
    };
    let _ = writeln!(out, "idle={idle} normal={}", sys::scheduling_policy(None));
    Ok(())
}

/// Spends `milliseconds` of this process's CPU time.
fn burn_cpu(milliseconds: i64) {
    let deadline = cpu_time_ns().wrapping_add(milliseconds.wrapping_mul(NSEC_PER_MSEC));
    while cpu_time_ns() < deadline {
        std::hint::spin_loop();
    }
}

/// Forks a repair step 7 worker as the daemon does, after `PRE_MS` of preparation, with a fork
/// that costs the parent `FORK_MS`; the worker spends `CHILD_MS` and prints its preparation
/// share and step 7's CPU time.
fn test_deferred_restore_cpu(arguments: &[&[u8]], out: &mut dyn Write) -> Result<(), CommandError> {
    expect(arguments, 3)?;
    let [preparation_ms, fork_ms, child_ms] =
        [arguments[0], arguments[1], arguments[2]].map(decoders::parse_i64_lossy);
    let mut timing = daemon::empty_restore_timing();
    let start = cpu_time_ns();
    burn_cpu(preparation_ms);
    match daemon::fork_deferred_restore(start, &mut timing, || burn_cpu(fork_ms)) {
        Ok(ForkResult::Child) => {
            burn_cpu(child_ms);
            let line = format!(
                "prior_ms={} total_ms={}\n",
                timing.prior_cpu_ns / NSEC_PER_MSEC,
                daemon::deferred_restore_cpu_ns(&timing) / NSEC_PER_MSEC
            );
            let _ = out.write_all(line.as_bytes());
            sys::exit_immediately(0)
        }
        Ok(ForkResult::Parent(process)) => match sys::wait(process) {
            Some(ChildStatus::Exited(0)) => Ok(()),
            _ => Err(CommandError::Operation),
        },
        Err(_) => Err(CommandError::Operation),
    }
}

#[cfg(test)]
mod tests {
    //! Focused regressions for the conversions of the test hooks.

    use super::*;

    #[test]
    fn cpuid_values_are_decimal_or_hexadecimal_and_narrow_like_c() {
        assert_eq!(cpuid_value(b"0x40000006"), 0x4000_0006);
        assert_eq!(cpuid_value(b"7"), 7);
        assert_eq!(cpuid_value(b"4294967297"), 1);
        assert_eq!(cpuid_value(b"-1"), 0);
        assert_eq!(cpuid_value(b"x"), 0);
    }

    #[test]
    fn malformed_hooks_fail_with_the_usage_status() {
        let mut out = Vec::new();
        assert_eq!(run(&[], &mut out), Err(CommandError::Usage));
        assert_eq!(run(&[b"unknown"], &mut out), Err(CommandError::Usage));
        assert_eq!(run(&[b"pair", b"1"], &mut out), Err(CommandError::Usage));
        assert_eq!(
            run(&[b"deferral", b"x"], &mut out),
            Err(CommandError::Usage)
        );
        assert!(out.is_empty());
        assert_eq!(run(&[b"pair", b"1000", b"3000", b"5000"], &mut out), Ok(()));
        assert_eq!(out, b"theta_ns=3000 epsilon_ns=1000\n");
    }

    #[test]
    fn corrections_print_the_kernel_modes() {
        let mut out = Vec::new();
        run(&[b"correction", b"128000000", b"10", b"fast"], &mut out).unwrap();
        run(&[b"correction", b"-5", b"10", b"slow"], &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "step modes=0x2100 sec=0 nsec=128000000\n\
             slew modes=0x203d offset=-5 status=0x2001 constant=6 maxerror=1 esterror=1\n"
        );
    }
}
