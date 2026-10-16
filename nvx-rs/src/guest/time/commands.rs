//! `nvx-time` command implementations and the restore packet transaction.

use std::ffi::{CStr, CString};
use std::io::Write;

use super::checks::{self, BootLog, Checks};
use super::daemon::{self, Promoter, RestoreSignal};
use super::decoders::{self, GENERATION_ID_SIZE, PACKET_HEADER_SIZE, RestorePacket};
use super::state::TimeState;
use super::system::{
    self, COMMAND_LINE_LIMIT, PORTB_GENERATION_ID_AVAILABLE, PORTB_PACKET_AVAILABLE,
    PORTB_TIME_WINDOW, PRIVATE_FILE_MODE, PortIo, PowerOffStatus, RESTORE_PATH, RESTORE_TEMP_PATH,
    Runtime, SELECT_GENERATION_ID, SELECT_PACKET, SNAPSHOT_ACKNOWLEDGE, SampleError, clock_ns,
    cpu_time_ns, current_frequency, initial_sample, portb_read, set_frequency, state_apply,
    state_load, state_store, step_clock, suppression_restore, suppression_save,
};
use super::{DEFERRED_START_NS, MAX_CPUS, NSEC_PER_USEC, Phase, USAGE};
use crate::guest::parse::parse_decimal;
use crate::guest::status::CommandError;
use crate::guest::sys::{self, Errno, FileDescriptor, ForkResult, Mode, OpenFlags, Port, kernel};

/// Restore flag requiring a snapshot acknowledgement.
const PACKET_ACK_REQUIRED: u8 = 0x04;
/// Restore flag indicating downtime came from host UTC.
const PACKET_DOWNTIME_UTC: u8 = 0x01;
/// Restore flag indicating memory-target ranges.
const PACKET_MEMORY_TARGET: u8 = 0x02;
/// Maximum accepted restore pairing uncertainty.
const REPAIR_BOUND_NS: i64 = 1_000_000;
/// Samples that repair takes when the packet's pairing is too uncertain.
const REPAIR_SAMPLE_ATTEMPTS: usize = 2;
/// The capture requests that `capture` accepts, and writes to the snapshot port: 0 or 1.
const CAPTURE_REQUESTS: std::ops::RangeInclusive<u64> = 0..=1;
/// Longest CPU list that `restore-finish` records, as the C program's 128-byte buffer held.
const NEW_CPUS_LIMIT: usize = 127;
/// The kernel command-line token that makes every command report-only.
const REPORT_ONLY_TOKEN: &[u8] = b"nvx_time_abi=report-only";

/// Writes one command diagnostic assembled from raw byte slices.
fn report(err: &mut dyn Write, parts: &[&[u8]]) {
    sys::report(err, parts);
}

/// Prints one usage line and fails with the usage status.
fn usage(err: &mut dyn Write, line: &[u8]) -> Result<(), CommandError> {
    report(err, &[line]);
    Err(CommandError::Usage)
}

/// Reports that the process may not access the time ABI's ports, and fails.
fn ports_unavailable(err: &mut dyn Write, error: Errno) -> CommandError {
    report(
        err,
        &[b"nvx-time: ioperm: ", error.desc().as_bytes(), b"\n"],
    );
    CommandError::Operation
}

/// Loads the persisted generation into shared runtime state.
fn load_generation(runtime: &Runtime<'_>) {
    if let Ok(state) = state_load() {
        runtime.set_generation(state.generation as u32);
    }
}

/// Accepted initial sample retained by the deferred boot checker.
struct BootSample {
    /// Guest-to-host offset in nanoseconds.
    theta: i64,
    /// Pairing uncertainty in nanoseconds.
    epsilon: i64,
    /// Monotonic acceptance time.
    accepted_ns: i64,
}

/// Opens the kernel log at its end, so that the violation watcher reads only later records.
fn open_kernel_log(runtime: &Runtime<'_>, checks: &mut Checks) -> Option<FileDescriptor> {
    let flags = OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK | OpenFlags::O_CLOEXEC;
    let log = sys::open(kernel::KERNEL_LOG_DEVICE, flags, Mode::empty()).and_then(|log| {
        log.seek_end()?;
        Ok(log)
    });
    match log {
        Ok(log) => Some(log),
        Err(error) => {
            checks.fail(
                runtime,
                "C4",
                format!(
                    "open {}: {}",
                    kernel::KERNEL_LOG_DEVICE.to_string_lossy(),
                    error.desc()
                ),
            );
            None
        }
    }
}

/// Runs the deferred boot checks from the monotonic `start_ns`, at idle priority until they end
/// or `IDLE_BOUND_NS` passes, and returns them with the kernel log that the daemon watches.
fn deferred_boot_checks(
    runtime: &Runtime<'_>,
    start_ns: i64,
    c12_failures: i32,
) -> (Checks, Option<FileDescriptor>) {
    let _promoter = Promoter::start(start_ns);
    let _idle = sys::IdlePriority::enter();
    sys::sleep_until_monotonic(start_ns);
    let mut checks = match Checks::load(Phase::Boot) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Boot);
            checks.fail(runtime, "C1", "cannot read the CPU sets");
            sys::exit_immediately(1);
        }
    };
    checks.failures = c12_failures;
    // The buffer is allocated before the checks start their threads, as the C program did.
    let log_size = sys::kernel::kernel_log_size();
    let mut log_buffer = Vec::new();
    let buffer = match log_size {
        Ok(size) if size > 0 && log_buffer.try_reserve_exact(size).is_ok() => {
            log_buffer.resize(size, 0);
            Some(log_buffer.as_mut_slice())
        }
        _ => None,
    };
    let kmsg = open_kernel_log(runtime, &mut checks);
    let log = kmsg.is_some().then_some(BootLog {
        // A size that `klogctl` could not report is its failure result, -1.
        size: log_size.map_or(-1, |size| size as i64),
        buffer,
    });
    checks::run_boot_checks(runtime, &mut checks, log);
    (checks, kmsg)
}

/// Runs the deferred boot checks and then becomes the time daemon.
fn boot_checker(
    runtime: &Runtime<'_>,
    sample: &BootSample,
    c12_ns: i64,
    c12_cpu_ns: i64,
    c12_failures: i32,
) -> ! {
    let start = clock_ns(sys::Clock::Monotonic).wrapping_add(DEFERRED_START_NS);
    let cpu_start = cpu_time_ns();
    runtime.set_async_output(true);
    let detached = {
        let flags = OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC;
        let null = sys::open(kernel::NULL_DEVICE, flags, Mode::empty()).ok();
        sys::detach(null.as_ref())
    };
    if detached.is_err() {
        sys::exit_immediately(1);
    }
    let (mut checks, kmsg) = deferred_boot_checks(runtime, start, c12_failures);
    let daemon = kmsg.and_then(
        |kmsg| match daemon::become_daemon(kmsg, sample.accepted_ns) {
            Ok(daemon) => Some(daemon),
            Err(error) => {
                checks.fail(
                    runtime,
                    "C10",
                    format!("cannot start the time daemon: {}", error.desc()),
                );
                None
            }
        },
    );
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        b"ok",
        c12_ns.wrapping_add(clock_ns(sys::Clock::Monotonic).wrapping_sub(start)) / NSEC_PER_USEC,
        c12_cpu_ns.wrapping_add(cpu_time_ns().wrapping_sub(cpu_start)) / NSEC_PER_USEC,
        checks.tsc_hz,
        checks.lapic_hz,
    ) {
        checks.fail(
            runtime,
            "C10",
            format!("cannot publish the time state: {}", error.desc()),
        );
    }
    if let Some(daemon) = daemon {
        daemon::daemon_loop(runtime, daemon);
    }
    sys::exit_immediately(1)
}

/// Initializes time state, forks deferred boot checks, and returns promptly.
fn cmd_boot(runtime: &Runtime<'_>) -> Result<(), CommandError> {
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    let mut sample = BootSample {
        theta: 0,
        epsilon: 0,
        accepted_ns: 0,
    };
    runtime.set_generation(0);
    system::make_runtime_directories();
    let mut checks = Checks::empty(Phase::Boot);
    match initial_sample(runtime) {
        Ok(initial) => {
            sample.theta = initial.theta;
            sample.epsilon = initial.epsilon;
            sample.accepted_ns = clock_ns(sys::Clock::Monotonic);
        }
        Err(detail) => {
            checks.fail(runtime, "C12", detail);
        }
    }
    let accepted = u64::from(sample.accepted_ns != 0);
    let mut state = TimeState {
        synchronized: accepted,
        samples: accepted,
        ..TimeState::default()
    };
    if sample.accepted_ns != 0 {
        state.offset_ns = sample.theta;
        state.uncertainty_ns = sample.epsilon;
    }
    state.frequency_ppb = decoders::frequency_to_ppb(current_frequency(runtime));
    if let Err(error) = state_store(&state) {
        checks.fail(
            runtime,
            "C10",
            format!("cannot publish the time state: {}", error.desc()),
        );
        return Err(CommandError::Operation);
    }
    let c12_cpu = cpu_time_ns().wrapping_sub(cpu_start);
    match sys::fork() {
        Ok(ForkResult::Child) => boot_checker(
            runtime,
            &sample,
            clock_ns(sys::Clock::Monotonic).wrapping_sub(start),
            c12_cpu,
            checks.failures,
        ),
        Ok(ForkResult::Parent(_)) => Ok(()),
        Err(error) => {
            checks.fail(
                runtime,
                "C10",
                format!("cannot start the boot checks: {}", error.desc()),
            );
            Err(CommandError::Operation)
        }
    }
}

/// Parses `none`, empty, or CPU-list syntax for newly online CPUs.
fn parse_new_cpus(text: &[u8]) -> Option<([bool; MAX_CPUS], usize)> {
    if text.is_empty() || text == b"none" {
        Some(([false; MAX_CPUS], 0))
    } else {
        decoders::parse_cpu_list(text)
    }
}

/// Runs explicit capture or restore conformance checks.
fn cmd_check(
    runtime: &Runtime<'_>,
    arguments: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    let mut phase = None;
    let mut new_cpus = [false; MAX_CPUS];
    let mut new_count = Some(0);
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index] {
            b"--phase" if index + 1 < arguments.len() => {
                index += 1;
                phase = match arguments[index] {
                    b"capture" => Some(Phase::Capture),
                    b"restore" => Some(Phase::Restore),
                    _ => None,
                };
            }
            b"--new-cpus" if index + 1 < arguments.len() => {
                index += 1;
                // As in the C program, a later valid list replaces an earlier error.
                match parse_new_cpus(arguments[index]) {
                    Some((cpus, count)) => {
                        new_cpus = cpus;
                        new_count = Some(count);
                    }
                    None => new_count = None,
                }
            }
            _ => new_count = None,
        }
        index += 1;
    }
    let (Some(phase), Some(new_count)) = (phase, new_count) else {
        return usage(
            err,
            b"usage: nvx-time check --phase capture|restore [--new-cpus LIST]\n",
        );
    };
    load_generation(runtime);
    let mut checks = match Checks::load(phase) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(phase);
            checks.fail(runtime, "C6", "cannot read the CPU sets");
            return Err(CommandError::Operation);
        }
    };
    if phase == Phase::Capture {
        checks::run_capture_checks(runtime, &mut checks);
    } else {
        checks::run_restore_checks(runtime, &mut checks, &new_cpus, new_count);
    }
    let _ = checks::record_check(
        runtime,
        &checks,
        b"ok",
        clock_ns(sys::Clock::Monotonic).wrapping_sub(start) / NSEC_PER_USEC,
        cpu_time_ns().wrapping_sub(cpu_start) / NSEC_PER_USEC,
        0,
        0,
    );
    if checks.failures == 0 {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

/// Preparation steps 1 to 4: waits for the boot checks, which start the daemon that C10
/// requires, runs the capture checks, and suppresses the stall detectors before the snapshot.
fn cmd_pre_capture(runtime: &Runtime<'_>, err: &mut dyn Write) -> Result<(), CommandError> {
    let (state, completed) = checks::wait_for_checks(true);
    if !completed {
        let mut checks = Checks::empty(Phase::Capture);
        checks.fail(runtime, "C10", "the boot checks are still pending");
        return Err(CommandError::Operation);
    }
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    runtime.set_generation(state.generation as u32);
    let mut checks = match Checks::load(Phase::Capture) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Capture);
            checks.fail(runtime, "C6", "cannot read the CPU sets");
            return Err(CommandError::Operation);
        }
    };
    checks::run_capture_checks(runtime, &mut checks);
    let elapsed = clock_ns(sys::Clock::Monotonic).wrapping_sub(start);
    let cpu = cpu_time_ns().wrapping_sub(cpu_start);
    if let Err(error) = suppression_save() {
        let reason = if error == Errno::EEXIST {
            "a capture is already pending"
        } else {
            let _ = suppression_restore(false);
            error.desc()
        };
        report(
            err,
            &[
                b"nvx-time: cannot suppress the stall detectors: ",
                reason.as_bytes(),
                b"\n",
            ],
        );
        return Err(CommandError::Operation);
    }
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        b"ok",
        elapsed / NSEC_PER_USEC,
        cpu / NSEC_PER_USEC,
        0,
        0,
    ) {
        checks.fail(
            runtime,
            "C10",
            format!("cannot record the capture check: {}", error.desc()),
        );
    }
    Ok(())
}

/// Cancels capture by restoring saved stall-detector settings.
fn cmd_cancel_capture(err: &mut dyn Write) -> Result<(), CommandError> {
    suppression_restore(false).map_err(|detail| {
        report(err, &[b"nvx-time: ", &detail, b"\n"]);
        CommandError::Operation
    })
}

/// Closes and removes the restored entropy file, of a capture that restored no packet.
fn discard_entropy(file: FileDescriptor, path: &CStr) {
    let _ = file.close();
    let _ = sys::unlink(path);
}

/// Applies one successful restore packet to persistent state.
fn apply_repair(
    state: &mut TimeState,
    packet: &RestorePacket,
    theta: i64,
    epsilon: i64,
    step_realtime_ns: i64,
    frequency: i64,
    elapsed_us: i64,
) {
    state.generation = u64::from(packet.generation);
    state.discontinuities = state.discontinuities.wrapping_add(1);
    state.last_discontinuity = b"restore".to_vec();
    state.last_step_ns = theta;
    state.last_step_realtime_ns = step_realtime_ns;
    state.last_downtime_ns = packet.downtime_ns;
    state.last_downtime_source = if packet.flags & PACKET_DOWNTIME_UTC != 0 {
        b"utc".to_vec()
    } else {
        b"monotonic".to_vec()
    };
    state.synchronized = 1;
    state.offset_ns = theta;
    state.uncertainty_ns = epsilon;
    state.frequency_ppb = decoders::frequency_to_ppb(frequency);
    state.samples = state.samples.wrapping_add(1);
    state.last_sample_error = b"none".to_vec();
    // Repair step 3 marks this restore's check pending, so `nvx-time status` waits for the
    // daemon's step 7. Until then, `restore_elapsed_us` is the readiness path up to the clock
    // step, at `last_step_realtime_ns`; `restore_cpu_us` counts only step 7, which records it.
    let restore = &mut state.checks[Phase::Restore.check_index().unwrap_or_default()];
    restore.status = b"pending".to_vec();
    restore.cpus = 0;
    restore.elapsed_us = elapsed_us;
    restore.cpu_us = 0;
    restore.generation = state.generation;
    restore.failures = -1;
}

/// Emits a fatal repair event and fails in report-only mode.
fn repair_failed(runtime: &Runtime<'_>, code: &[u8], detail: &[u8]) -> Result<(), CommandError> {
    runtime.fail_fatal(
        PowerOffStatus::Repair,
        code,
        b"repair",
        Phase::Restore,
        detail,
    );
    Err(CommandError::Operation)
}

/// What a restore transaction writes, and where.
struct RepairTarget<'a> {
    /// The VM generation ID before the snapshot, which the packet must replace.
    previous_id: &'a [u8],
    /// The path of the restored entropy file.
    entropy_path: &'a CStr,
    /// The restored entropy file.
    entropy_file: FileDescriptor,
    /// Whether to hand repair steps 6 and 7 to the daemon.
    finish: bool,
    /// The daemon that would run step 7.
    daemon_pid: Option<sys::ProcessId>,
}

/// Repair steps 2 and 3: reads the packet with four-byte reads, validates it, steps and slews
/// the clock, and records the restore, after `cmd_capture` read the status, step 1.
fn repair_restore(
    runtime: &Runtime<'_>,
    ports: &dyn PortIo,
    target: RepairTarget<'_>,
    start_ns: i64,
    out: &mut dyn Write,
) -> Result<(), CommandError> {
    let t0 = clock_ns(sys::Clock::Realtime);
    ports.outb(Port::PortbStatus, SELECT_PACKET);
    let t1 = clock_ns(sys::Clock::Realtime);
    let mut header = [0_u8; PACKET_HEADER_SIZE];
    portb_read(ports, Port::PortbData, &mut header);
    let mut packet = match RestorePacket::parse_header(&header) {
        Ok(packet) => packet,
        Err(detail) => return repair_failed(runtime, b"G_REPAIR_PACKET", detail.as_bytes()),
    };
    if packet.generation == runtime.generation() {
        discard_entropy(target.entropy_file, target.entropy_path);
        let _ = out.write_all(b"0\n");
        return Ok(());
    }
    if u64::from(packet.generation) != u64::from(runtime.generation()) + 1 {
        return repair_failed(
            runtime,
            b"G_REPAIR_GENERATION",
            format!(
                "restore packet generation {} does not follow recorded generation {}",
                packet.generation,
                runtime.generation()
            )
            .as_bytes(),
        );
    }
    let mut body = vec![0_u8; packet.body_size()];
    portb_read(ports, Port::PortbData, &mut body);
    if let Err(detail) = packet.parse_body(&body) {
        return repair_failed(runtime, b"G_REPAIR_PACKET", detail.as_bytes());
    }
    if packet.entropy[..GENERATION_ID_SIZE] == *target.previous_id {
        return repair_failed(
            runtime,
            b"G_REPAIR_GENERATION",
            b"the VM generation ID did not change",
        );
    }
    runtime.set_generation(packet.generation);
    let (mut theta, mut epsilon) = decoders::pair_clocks(t0, t1, packet.utc_ns);
    if epsilon > REPAIR_BOUND_NS {
        match system::best_sample(
            runtime,
            REPAIR_SAMPLE_ATTEMPTS,
            REPAIR_BOUND_NS,
            packet.generation,
        ) {
            Err(SampleError::Generation(detail)) => {
                return repair_failed(runtime, b"G_REPAIR_GENERATION", &detail);
            }
            Ok((_, sample)) if sample.epsilon < epsilon => {
                theta = sample.theta;
                epsilon = sample.epsilon;
            }
            _ => {}
        }
        if epsilon > REPAIR_BOUND_NS {
            return repair_failed(
                runtime,
                b"G_REPAIR_SAMPLE",
                format!("UTC pairing uncertainty {epsilon} ns exceeds 1 ms").as_bytes(),
            );
        }
    }
    let frequency = decoders::restore_frequency(packet.rate_deviation);
    if let Err(error) = step_clock(runtime, theta).and_then(|()| set_frequency(runtime, frequency))
    {
        return repair_failed(
            runtime,
            b"G_REPAIR_CLOCK",
            format!("adjtimex: {}", error.desc()).as_bytes(),
        );
    }
    let step_realtime_ns = clock_ns(sys::Clock::Realtime);
    let elapsed_us = clock_ns(sys::Clock::Monotonic).wrapping_sub(start_ns) / NSEC_PER_USEC;
    let finishing = target.finish
        && target.daemon_pid.is_some()
        && packet.online_vp_count == 0
        && packet.range_count == 0
        && packet.flags & PACKET_MEMORY_TARGET == 0;
    let entropy = &packet.entropy;
    if let Err(detail) = system::write_entropy(target.entropy_file, target.entropy_path, entropy) {
        return repair_failed(runtime, b"G_REPAIR_PACKET", &detail);
    }
    if let Err(error) = state_apply(|state| {
        apply_repair(
            state,
            &packet,
            theta,
            epsilon,
            step_realtime_ns,
            frequency,
            elapsed_us,
        );
    }) {
        return repair_failed(
            runtime,
            b"G_REPAIR_CLOCK",
            format!("record the restore: {}", error.desc()).as_bytes(),
        );
    }
    let finished = finishing.then(|| {
        let signal = daemon::signal_restore(
            runtime,
            target.daemon_pid,
            packet.flags & PACKET_ACK_REQUIRED != 0,
            start_ns,
        );
        if signal == RestoreSignal::Failed {
            let mut checks = Checks::empty(Phase::Restore);
            checks.fail(runtime, "C10", "the time daemon is not running");
        }
        signal
    });
    let handed_off = finished.is_some_and(|signal| signal != RestoreSignal::NoDaemon);
    let mut line = format!(
        "{} {} {} {} {}",
        if handed_off { 2 } else { 1 },
        packet.flags,
        packet.online_vp_count,
        packet.range_count,
        decoders::format_hex(&packet.entropy[..GENERATION_ID_SIZE])
    );
    for (start, length) in &packet.ranges {
        line.push_str(&format!(" {start} {length}"));
    }
    line.push('\n');
    let _ = out.write_all(line.as_bytes());
    Ok(())
}

/// Parses a capture request, 0 or 1, as plain decimal.
fn parse_capture_request(text: &[u8]) -> Option<u8> {
    parse_decimal(text)
        .filter(|request| CAPTURE_REQUESTS.contains(request))
        .map(|request| request as u8)
}

/// Starts a capture or performs restore repair from the selected packet.
fn cmd_capture(
    runtime: &Runtime<'_>,
    arguments: &[&[u8]],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    const LINE: &[u8] = b"usage: nvx-time capture 0|1 GENERATION_ID ENTROPY_FILE [--finish]\n";
    let (request, previous_id, entropy_path, finish) = match arguments {
        [request, id, path] => (request, id, path, false),
        [request, id, path, b"--finish"] => (request, id, path, true),
        _ => return usage(err, LINE),
    };
    let Some(request) = parse_capture_request(request) else {
        return usage(err, LINE);
    };
    let Some(previous_id) = decoders::parse_hex(previous_id, GENERATION_ID_SIZE) else {
        return usage(err, LINE);
    };
    let Ok(entropy_path) = CString::new(*entropy_path) else {
        return usage(err, LINE);
    };
    if state_load().is_err() {
        report(err, &[b"nvx-time: the time state is unavailable\n"]);
        return Err(CommandError::Operation);
    }
    load_generation(runtime);
    let ports = runtime
        .ports()
        .map_err(|error| ports_unavailable(err, error))?;
    let daemon_pid = if finish { checks::daemon_pid() } else { None };
    let flags = OpenFlags::O_WRONLY
        | OpenFlags::O_CREAT
        | OpenFlags::O_TRUNC
        | OpenFlags::O_NOFOLLOW
        | OpenFlags::O_CLOEXEC;
    let entropy_file = match sys::open(&entropy_path, flags, PRIVATE_FILE_MODE) {
        Ok(file) => file,
        Err(error) => {
            report(
                err,
                &[
                    b"nvx-time: open ",
                    entropy_path.to_bytes(),
                    b": ",
                    error.desc().as_bytes(),
                    b"\n",
                ],
            );
            return Err(CommandError::Operation);
        }
    };
    let _lock = match system::lock_portb() {
        Ok(lock) => lock,
        Err(detail) => {
            report(err, &[b"nvx-time: ", &detail, b"\n"]);
            discard_entropy(entropy_file, &entropy_path);
            return Err(CommandError::Operation);
        }
    };
    ports.outb(Port::Snapshot, request);
    let start = clock_ns(sys::Clock::Monotonic);
    if ports.inb(Port::PortbStatus) & PORTB_PACKET_AVAILABLE == 0 {
        discard_entropy(entropy_file, &entropy_path);
        let _ = out.write_all(b"0\n");
        return Ok(());
    }
    let target = RepairTarget {
        previous_id: &previous_id,
        entropy_path: &entropy_path,
        entropy_file,
        finish,
        daemon_pid,
    };
    repair_restore(runtime, ports, target, start, out)
}

/// Records new CPUs and signals or inlines deferred restore completion.
fn cmd_restore_finish(
    runtime: &Runtime<'_>,
    arguments: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut acknowledge = false;
    let mut new_list = b"none".as_slice();
    let mut new_cpus = [false; MAX_CPUS];
    let mut new_count = Some(0);
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index] {
            b"--ack" => acknowledge = true,
            b"--new-cpus" if index + 1 < arguments.len() => {
                index += 1;
                new_list = arguments[index];
                // As in the C program, a later valid list replaces an earlier error.
                match parse_new_cpus(new_list) {
                    Some((cpus, count)) => {
                        new_cpus = cpus;
                        new_count = Some(count);
                    }
                    None => new_count = None,
                }
            }
            _ => new_count = None,
        }
        index += 1;
    }
    let Some(new_count) = new_count.filter(|_| new_list.len() <= NEW_CPUS_LIMIT) else {
        return usage(
            err,
            b"usage: nvx-time restore-finish [--ack] [--new-cpus LIST]\n",
        );
    };
    let Ok(state) = state_load() else {
        return repair_failed(runtime, b"G_REPAIR_CLOCK", b"the time state is unreadable");
    };
    runtime.set_generation(state.generation as u32);
    let cpus = if new_count > 0 { new_list } else { b"none" };
    let mut record = format!("generation={}\nnew_cpus=", state.generation).into_bytes();
    record.extend_from_slice(cpus);
    record.push(b'\n');
    if let Err(error) = system::replace_file(RESTORE_PATH, RESTORE_TEMP_PATH, &record) {
        return repair_failed(
            runtime,
            b"G_REPAIR_CLOCK",
            format!("record the restore: {}", error.desc()).as_bytes(),
        );
    }
    let ports = if acknowledge {
        Some(
            runtime
                .ports()
                .map_err(|error| ports_unavailable(err, error))?,
        )
    } else {
        None
    };
    let start = clock_ns(sys::Clock::Monotonic)
        .wrapping_sub(clock_ns(sys::Clock::Realtime).wrapping_sub(state.last_step_realtime_ns))
        .wrapping_sub(
            state.checks[Phase::Restore.check_index().unwrap_or_default()].elapsed_us
                * NSEC_PER_USEC,
        );
    match daemon::signal_restore(runtime, checks::daemon_pid(), acknowledge, start) {
        RestoreSignal::Queued => return Ok(()),
        RestoreSignal::NoDaemon if runtime.report_only() => {}
        RestoreSignal::NoDaemon | RestoreSignal::Failed => {
            let mut checks = Checks::empty(Phase::Restore);
            checks.fail(runtime, "C10", "the time daemon is not running");
            return Err(CommandError::Operation);
        }
    }
    if let Some(ports) = ports {
        ports.outb(Port::Snapshot, SNAPSHOT_ACKNOWLEDGE);
    }
    let timing = daemon::inline_restore_timing(start);
    let _ = daemon::restore_work(runtime, &new_cpus, new_count, true, &timing);
    Ok(())
}

/// Prints persisted status after waiting for pending checks.
fn cmd_status(runtime: &Runtime<'_>, out: &mut dyn Write) -> Result<(), CommandError> {
    let (state, completed) = checks::wait_for_checks(false);
    let (text, ok) = state.format_status(runtime.report_only());
    let _ = out.write_all(&text);
    if completed && ok {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

/// Prints one raw host time sample and its derived pairing.
fn cmd_sample(
    runtime: &Runtime<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let ports = runtime
        .ports()
        .map_err(|error| ports_unavailable(err, error))?;
    let status = ports.inb(Port::PortbStatus);
    if status & PORTB_TIME_WINDOW == 0 {
        report(
            err,
            &[format!("nvx-time: portb status 0x{status:02x} has no time window\n").as_bytes()],
        );
        return Err(CommandError::Operation);
    }
    let sample = {
        let _lock = system::lock_portb().map_err(|detail| {
            report(err, &[b"nvx-time: ", &detail, b"\n"]);
            CommandError::Operation
        })?;
        system::take_sample(ports)
    };
    let sample = sample.map_err(|detail| {
        report(err, &[b"nvx-time: ", detail.as_bytes(), b"\n"]);
        CommandError::Operation
    })?;
    let (theta, epsilon) = decoders::pair_clocks(sample.t0, sample.t1, sample.utc_ns);
    let line = format!(
        "flags=0x{:02x} generation={} utc_ns={} t0={} t1={} theta_ns={} epsilon_ns={}\n",
        sample.flags, sample.generation, sample.utc_ns, sample.t0, sample.t1, theta, epsilon
    );
    let _ = out.write_all(line.as_bytes());
    Ok(())
}

/// Reads and prints the generation identifier from the port window.
fn cmd_generation_id(
    runtime: &Runtime<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let ports = runtime
        .ports()
        .map_err(|error| ports_unavailable(err, error))?;
    if ports.inb(Port::PortbStatus) & PORTB_GENERATION_ID_AVAILABLE == 0 {
        report(err, &[b"nvx-time: the VM generation ID is unavailable\n"]);
        return Err(CommandError::Operation);
    }
    let mut id = [0_u8; GENERATION_ID_SIZE];
    {
        let _lock = system::lock_portb().map_err(|detail| {
            report(err, &[b"nvx-time: ", &detail, b"\n"]);
            CommandError::Operation
        })?;
        ports.outb(Port::PortbStatus, SELECT_GENERATION_ID);
        portb_read(ports, Port::PortbData, &mut id);
    }
    let mut text = decoders::format_hex(&id).into_bytes();
    text.push(b'\n');
    let _ = out.write_all(&text);
    Ok(())
}

/// Returns whether the kernel command line enables report-only mode.
fn cmdline_requests_report_only() -> bool {
    let Ok(text) = system::read_text(kernel::COMMAND_LINE, COMMAND_LINE_LIMIT) else {
        return false;
    };
    text.split(|byte| matches!(byte, b' ' | b'\t' | b'\n'))
        .any(|token| token == REPORT_ONLY_TOKEN)
}

/// Dispatches a production `nvx-time` command.
pub(super) fn run(
    arguments: &[&[u8]],
    report_only: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let ops = system::RealOps::new();
    let runtime = Runtime::new(&ops, report_only || cmdline_requests_report_only());
    sys::reset_child_signal();
    let Some((&command, rest)) = arguments.split_first() else {
        return usage(err, USAGE);
    };
    match (command, rest) {
        (b"boot", []) => cmd_boot(&runtime),
        (b"check", arguments) => cmd_check(&runtime, arguments, err),
        (b"pre-capture", []) => cmd_pre_capture(&runtime, err),
        (b"capture", arguments) => cmd_capture(&runtime, arguments, out, err),
        (b"cancel-capture", []) => cmd_cancel_capture(err),
        (b"restore-finish", arguments) => cmd_restore_finish(&runtime, arguments, err),
        (b"status", []) => cmd_status(&runtime, out),
        (b"sample", []) => cmd_sample(&runtime, out, err),
        (b"generation-id", []) => cmd_generation_id(&runtime, out, err),
        (b"exhaustive", []) => checks::exhaustive(&runtime, out),
        _ => usage(err, USAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_cpu_parser_keeps_none_and_ranges_distinct() {
        assert_eq!(parse_new_cpus(b"none").unwrap().1, 0);
        assert_eq!(parse_new_cpus(b"").unwrap().1, 0);
        assert_eq!(parse_new_cpus(b"1-3,5").unwrap().1, 4);
        assert!(parse_new_cpus(b"64").is_none());
    }

    #[test]
    fn capture_requests_are_plain_decimal_zero_or_one() {
        for (text, expected) in [
            (b"0".as_slice(), Some(0)),
            (b"1", Some(1)),
            (b"01", Some(1)),
            (b"2", None),
            (b"", None),
            (b"+1", None),
            (b"-0", None),
            (b" 1", None),
            (b"1x", None),
            (b"999999999999999999999999999999999999", None),
        ] {
            assert_eq!(parse_capture_request(text), expected, "{text:?}");
        }
    }
}
