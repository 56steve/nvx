//! `nvx-time` command implementations and the restore packet transaction.

use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::io::Write;

use super::checks::{self, BootLog, Checks};
use super::cpu_mask::CpuMask;
use super::daemon::{self, Promoter};
use super::decoders::{
    self, GENERATION_ID_SIZE, PACKET_HEADER_SIZE, PacketError, PacketHeader, RestorePacket,
};
use super::events::{CheckId, ViolationCode};
use super::state::{
    CheckState, CheckStatus, Discontinuity, DowntimeSource, SampleRejection, TimeState,
};
use super::system::{
    self, PORTB_GENERATION_ID_AVAILABLE, PORTB_PACKET_AVAILABLE, PORTB_TIME_WINDOW,
    PRIVATE_FILE_MODE, RESTORE_PATH, RESTORE_TEMP_PATH, Runtime, SELECT_GENERATION_ID,
    SELECT_PACKET, SampleError, clock_ns, cpu_time_ns, current_frequency, fork_process,
    initial_sample, portb_read, set_frequency, state_apply, state_load, state_store, step_clock,
    suppression_restore, suppression_save,
};
use super::{DEFERRED_START_NS, NSEC_PER_USEC, Phase, USAGE};
use crate::guest::parse::parse_decimal;
use crate::guest::status::CommandError;
use crate::guest::sys::{
    self, Errno, FileDescriptor, ForkResult, Mode, OpenFlags, Port, PortAccess, PortError, kernel,
};

/// Maximum accepted restore pairing uncertainty.
const REPAIR_BOUND_NS: i64 = 1_000_000;
/// Samples that repair takes when the packet's pairing is too uncertain.
const REPAIR_SAMPLE_ATTEMPTS: usize = 2;
/// The capture requests that `capture` accepts, and writes to the snapshot port: 0 or 1.
const CAPTURE_REQUESTS: std::ops::RangeInclusive<u64> = 0..=1;
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

/// Reports why the thread could not take, or give up, its permission for the time ABI's ports,
/// such as `nvx-time: ioperm: Operation not permitted`, and fails.
fn port_failure(err: &mut dyn Write, error: PortError) -> CommandError {
    report(err, &[format!("nvx-time: {error}\n").as_bytes()]);
    CommandError::Operation
}

/// Loads the persisted generation into shared runtime state.
fn load_generation(runtime: &Runtime<'_>) {
    if let Ok(state) = state_load() {
        runtime.set_generation(state.generation as u32);
    }
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
                CheckId::C4,
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
    c12_failures: usize,
) -> (Checks, Option<FileDescriptor>) {
    let _promoter = Promoter::start(start_ns);
    let _idle = sys::IdlePriority::enter();
    sys::sleep_until_monotonic(start_ns);
    let mut checks = match Checks::load(Phase::Boot) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Boot);
            checks.fail(runtime, CheckId::C1, "cannot read the CPU sets");
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

/// Runs the deferred boot checks and then becomes the time daemon, whose discipline starts from
/// the initial sample accepted at the monotonic `accepted_ns`, if any.
fn boot_checker(
    runtime: &Runtime<'_>,
    accepted_ns: Option<i64>,
    c12_ns: i64,
    c12_cpu_ns: i64,
    c12_failures: usize,
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
    let daemon = kmsg.and_then(|kmsg| match daemon::become_daemon(kmsg, accepted_ns) {
        Ok(daemon) => Some(daemon),
        Err(error) => {
            checks.fail(
                runtime,
                CheckId::C10,
                format!("cannot start the time daemon: {}", error.desc()),
            );
            None
        }
    });
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        c12_ns.wrapping_add(clock_ns(sys::Clock::Monotonic).wrapping_sub(start)) / NSEC_PER_USEC,
        c12_cpu_ns.wrapping_add(cpu_time_ns().wrapping_sub(cpu_start)) / NSEC_PER_USEC,
        checks.tsc_hz,
        checks.lapic_hz,
    ) {
        checks.fail(
            runtime,
            CheckId::C10,
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
    runtime.set_generation(0);
    system::make_runtime_directories();
    let mut checks = Checks::empty(Phase::Boot);
    let initial = match initial_sample(runtime) {
        Ok(initial) => Some((initial, clock_ns(sys::Clock::Monotonic))),
        Err(detail) => {
            checks.fail(runtime, CheckId::C12, detail);
            None
        }
    };
    let mut state = TimeState::default();
    state.synchronized = initial.is_some();
    state.samples = u64::from(initial.is_some());
    if let Some((sample, _)) = initial {
        state.offset_ns = sample.theta;
        state.uncertainty_ns = sample.epsilon;
    }
    state.frequency_ppb = decoders::frequency_to_ppb(current_frequency(runtime));
    if let Err(error) = state_store(&state) {
        checks.fail(
            runtime,
            CheckId::C10,
            format!("cannot publish the time state: {}", error.desc()),
        );
        return Err(CommandError::Operation);
    }
    let c12_cpu = cpu_time_ns().wrapping_sub(cpu_start);
    match fork_process() {
        Ok(ForkResult::Child) => boot_checker(
            runtime,
            initial.map(|(_, accepted_ns)| accepted_ns),
            clock_ns(sys::Clock::Monotonic).wrapping_sub(start),
            c12_cpu,
            checks.failures,
        ),
        Ok(ForkResult::Parent(_)) => Ok(()),
        Err(error) => {
            checks.fail(
                runtime,
                CheckId::C10,
                format!("cannot start the boot checks: {error}"),
            );
            Err(CommandError::Operation)
        }
    }
}

/// Runs explicit capture or restore conformance checks.
fn cmd_check(
    runtime: &Runtime<'_>,
    arguments: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    const LINE: &[u8] = b"usage: nvx-time check --phase capture|restore [--new-cpus LIST]\n";
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    let mut phase = None;
    let mut new_cpus = CpuMask::EMPTY;
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        match (argument, arguments.next()) {
            (b"--phase", Some(b"capture")) => phase = Some(Phase::Capture),
            (b"--phase", Some(b"restore")) => phase = Some(Phase::Restore),
            (b"--new-cpus", Some(list)) => match CpuMask::parse_new(list) {
                Some(cpus) => new_cpus = cpus,
                None => return usage(err, LINE),
            },
            _ => return usage(err, LINE),
        }
    }
    let Some(phase) = phase else {
        return usage(err, LINE);
    };
    load_generation(runtime);
    let mut checks = match Checks::load(phase) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(phase);
            checks.fail(runtime, CheckId::C6, "cannot read the CPU sets");
            return Err(CommandError::Operation);
        }
    };
    if phase == Phase::Capture {
        checks::run_capture_checks(runtime, &mut checks);
    } else {
        checks::run_restore_checks(runtime, &mut checks, new_cpus);
    }
    let _ = checks::record_check(
        runtime,
        &checks,
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
        checks.fail(runtime, CheckId::C10, "the boot checks are still pending");
        return Err(CommandError::Operation);
    }
    let start = clock_ns(sys::Clock::Monotonic);
    let cpu_start = cpu_time_ns();
    runtime.set_generation(state.generation as u32);
    let mut checks = match Checks::load(Phase::Capture) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Capture);
            checks.fail(runtime, CheckId::C6, "cannot read the CPU sets");
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
        elapsed / NSEC_PER_USEC,
        cpu / NSEC_PER_USEC,
        0,
        0,
    ) {
        checks.fail(
            runtime,
            CheckId::C10,
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
    packet: &PacketHeader,
    theta: i64,
    epsilon: i64,
    step_realtime_ns: i64,
    frequency: i64,
    elapsed_us: i64,
) {
    state.generation = u64::from(packet.generation);
    state.discontinuities += 1;
    state.last_discontinuity = Discontinuity::Restore;
    state.last_step_ns = theta;
    state.last_step_realtime_ns = step_realtime_ns;
    state.last_downtime_ns = packet.downtime_ns;
    state.last_downtime_source = if packet.downtime_from_utc() {
        DowntimeSource::Utc
    } else {
        DowntimeSource::Monotonic
    };
    state.synchronized = true;
    state.offset_ns = theta;
    state.uncertainty_ns = epsilon;
    state.frequency_ppb = decoders::frequency_to_ppb(frequency);
    state.samples += 1;
    state.last_sample_error = SampleRejection::None;
    // Repair step 3 marks this restore's check pending, so `nvx-time status` waits for the
    // daemon's step 7. Until then, `restore_elapsed_us` is the readiness path up to the clock
    // step, at `last_step_realtime_ns`; `restore_cpu_us` counts only step 7, which records it.
    let generation = state.generation;
    if let Some(restore) = state.check_mut(Phase::Restore) {
        *restore = CheckState {
            status: Some(CheckStatus::Pending),
            elapsed_us,
            generation,
            ..CheckState::default()
        };
    }
}

/// Emits a fatal repair event and fails in report-only mode.
fn repair_failed(
    runtime: &Runtime<'_>,
    code: ViolationCode,
    detail: impl AsRef<[u8]>,
) -> Result<(), CommandError> {
    runtime.repair_failure(code, detail.as_ref());
    Err(CommandError::Operation)
}

/// What a restore transaction writes, and where.
struct RepairTarget<'a> {
    /// The VM generation ID before the snapshot, which the packet must replace.
    previous_id: &'a [u8; GENERATION_ID_SIZE],
    /// The path of the restored entropy file.
    entropy_path: &'a CStr,
    /// The restored entropy file.
    entropy_file: FileDescriptor,
    /// Whether to hand repair steps 6 and 7 to the daemon.
    finish: bool,
    /// The daemon that would run step 7.
    daemon_pid: Option<sys::ProcessId>,
}

/// What repair step 2's port reads returned, which repair validates once their transaction
/// ended.
enum PacketRead {
    /// The packet's header is invalid.
    InvalidHeader(PacketError),
    /// The packet is of the recorded generation: the VM continued without a restore.
    Stale,
    /// The packet's generation, which does not follow the recorded generation.
    Generation(u32),
    /// The packet, or why its body is invalid, and the realtime clock around its selection.
    Packet {
        /// Realtime before the selection.
        t0: i64,
        /// Realtime after the selection.
        t1: i64,
        /// The packet, or why its body is invalid.
        packet: Result<RestorePacket, PacketError>,
    },
}

/// Repair step 2's port reads, after `cmd_capture` read the status, step 1: selects the restore
/// packet, and reads its header and, when the header is valid and follows the `recorded`
/// generation, its body, with four-byte reads.
fn read_packet(ports: &PortAccess<'_>, recorded: u32) -> PacketRead {
    let t0 = clock_ns(sys::Clock::Realtime);
    ports.write_u8(Port::PortbStatus, SELECT_PACKET);
    let t1 = clock_ns(sys::Clock::Realtime);
    let mut header = [0_u8; PACKET_HEADER_SIZE];
    portb_read(ports, Port::PortbData, &mut header);
    let header = match PacketHeader::parse(&header) {
        Ok(header) => header,
        Err(error) => return PacketRead::InvalidHeader(error),
    };
    if header.generation == recorded {
        return PacketRead::Stale;
    }
    if u64::from(header.generation) != u64::from(recorded) + 1 {
        return PacketRead::Generation(header.generation);
    }
    let mut body = vec![0_u8; header.body_size()];
    portb_read(ports, Port::PortbData, &mut body);
    PacketRead::Packet {
        t0,
        t1,
        packet: RestorePacket::parse(header, &body),
    }
}

/// Repair steps 2 and 3, once the port transaction that read the packet ended: validates it,
/// steps and slews the clock, and records the restore.
fn repair_restore(
    runtime: &Runtime<'_>,
    read: PacketRead,
    target: RepairTarget<'_>,
    start_ns: i64,
    out: &mut dyn Write,
) -> Result<(), CommandError> {
    let (t0, t1, packet) = match read {
        PacketRead::InvalidHeader(error) => {
            return repair_failed(runtime, ViolationCode::RepairPacket, error.to_string());
        }
        PacketRead::Stale => {
            discard_entropy(target.entropy_file, target.entropy_path);
            let _ = out.write_all(b"0\n");
            return Ok(());
        }
        PacketRead::Generation(generation) => {
            return repair_failed(
                runtime,
                ViolationCode::RepairGeneration,
                format!(
                    "restore packet generation {} does not follow recorded generation {}",
                    generation,
                    runtime.generation()
                ),
            );
        }
        PacketRead::Packet { t0, t1, packet } => (t0, t1, packet),
    };
    let packet = match packet {
        Ok(packet) => packet,
        Err(error) => {
            return repair_failed(runtime, ViolationCode::RepairPacket, error.to_string());
        }
    };
    let header = packet.header;
    if packet.generation_id() == target.previous_id {
        return repair_failed(
            runtime,
            ViolationCode::RepairGeneration,
            b"the VM generation ID did not change",
        );
    }
    runtime.set_generation(header.generation);
    let (mut theta, mut epsilon) = decoders::pair_clocks(t0, t1, header.utc_ns);
    if epsilon > REPAIR_BOUND_NS {
        match system::best_sample(
            runtime,
            REPAIR_SAMPLE_ATTEMPTS,
            REPAIR_BOUND_NS,
            header.generation,
        ) {
            Err(error @ SampleError::Generation { .. }) => {
                return repair_failed(runtime, ViolationCode::RepairGeneration, error.to_string());
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
                ViolationCode::RepairSample,
                format!("UTC pairing uncertainty {epsilon} ns exceeds 1 ms"),
            );
        }
    }
    let frequency = decoders::restore_frequency(header.rate_deviation);
    if let Err(error) = step_clock(runtime, theta).and_then(|()| set_frequency(runtime, frequency))
    {
        return repair_failed(
            runtime,
            ViolationCode::RepairClock,
            format!("adjtimex: {}", error.desc()),
        );
    }
    let step_realtime_ns = clock_ns(sys::Clock::Realtime);
    let elapsed_us = clock_ns(sys::Clock::Monotonic).wrapping_sub(start_ns) / NSEC_PER_USEC;
    let finishing = target.finish
        && target.daemon_pid.is_some()
        && header.online_vp_count == 0
        && header.range_count == 0
        && !header.memory_target();
    if let Err(detail) =
        system::write_entropy(target.entropy_file, target.entropy_path, &packet.entropy)
    {
        return repair_failed(runtime, ViolationCode::RepairPacket, &detail);
    }
    if let Err(error) = state_apply(|state| {
        apply_repair(
            state,
            &header,
            theta,
            epsilon,
            step_realtime_ns,
            frequency,
            elapsed_us,
        );
    }) {
        return repair_failed(
            runtime,
            ViolationCode::RepairClock,
            format!("record the restore: {}", error.desc()),
        );
    }
    // Repair steps 6 and 7 on the readiness path, when the call finishes the restore. Without a
    // running daemon, or without the permission for the acknowledgement, the restore stays
    // unfinished, and `nvx-snapshot` finishes it with `restore-finish`.
    let daemon = if finishing {
        daemon::running_daemon(target.daemon_pid)
    } else {
        None
    };
    let handed_off = daemon.is_some_and(|daemon| {
        daemon::finish_restore(runtime, daemon, header.acknowledgement_required(), start_ns)
    });
    let mut line = format!(
        "{} {} {} {} {}",
        if handed_off { 2 } else { 1 },
        header.flags,
        header.online_vp_count,
        header.range_count,
        decoders::format_hex(packet.generation_id())
    );
    for (start, length) in &packet.ranges {
        let _ = write!(line, " {start} {length}");
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
    let Some(previous_id) = decoders::parse_hex::<GENERATION_ID_SIZE>(previous_id) else {
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
    // The port transaction of the snapshot request and of repair steps 1 and 2 starts before
    // the entropy file opens, so that a guest without the permission fails first.
    let ports = runtime
        .open_ports()
        .map_err(|error| port_failure(err, error))?;
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
    ports.write_u8(Port::Snapshot, request);
    let start = clock_ns(sys::Clock::Monotonic);
    let read = (ports.read_u8(Port::PortbStatus) & PORTB_PACKET_AVAILABLE != 0)
        .then(|| read_packet(&ports, runtime.generation()));
    drop(ports);
    let Some(read) = read else {
        discard_entropy(entropy_file, &entropy_path);
        let _ = out.write_all(b"0\n");
        return Ok(());
    };
    let target = RepairTarget {
        previous_id: &previous_id,
        entropy_path: &entropy_path,
        entropy_file,
        finish,
        daemon_pid,
    };
    repair_restore(runtime, read, target, start, out)
}

/// Records new CPUs and signals or inlines deferred restore completion.
fn cmd_restore_finish(
    runtime: &Runtime<'_>,
    arguments: &[&[u8]],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    const LINE: &[u8] = b"usage: nvx-time restore-finish [--ack] [--new-cpus LIST]\n";
    let mut acknowledge = false;
    let mut new_cpus = CpuMask::EMPTY;
    let mut arguments = arguments.iter().copied();
    while let Some(argument) = arguments.next() {
        match argument {
            b"--ack" => acknowledge = true,
            b"--new-cpus" => match arguments.next().and_then(CpuMask::parse_new) {
                Some(cpus) => new_cpus = cpus,
                None => return usage(err, LINE),
            },
            _ => return usage(err, LINE),
        }
    }
    let Ok(state) = state_load() else {
        return repair_failed(
            runtime,
            ViolationCode::RepairClock,
            b"the time state is unreadable",
        );
    };
    runtime.set_generation(state.generation as u32);
    let record = format!("generation={}\nnew_cpus={new_cpus}\n", state.generation);
    if let Err(error) = system::replace_file(RESTORE_PATH, RESTORE_TEMP_PATH, record.as_bytes()) {
        return repair_failed(
            runtime,
            ViolationCode::RepairClock,
            format!("record the restore: {}", error.desc()),
        );
    }
    let ports = if acknowledge {
        Some(
            runtime
                .open_ports()
                .map_err(|error| port_failure(err, error))?,
        )
    } else {
        None
    };
    let start = clock_ns(sys::Clock::Monotonic)
        .wrapping_sub(clock_ns(sys::Clock::Realtime).wrapping_sub(state.last_step_realtime_ns))
        .wrapping_sub(
            state
                .check(Phase::Restore)
                .map_or(0, |restore| restore.elapsed_us)
                * NSEC_PER_USEC,
        );
    let daemon = daemon::running_daemon(checks::daemon_pid());
    if daemon.is_none() && !runtime.report_only() {
        // The failure's event writes the debug console in a transaction of its own.
        drop(ports);
        let mut checks = Checks::empty(Phase::Restore);
        checks.fail(runtime, CheckId::C10, "the time daemon is not running");
        return Err(CommandError::Operation);
    }
    if let Some(ports) = ports {
        daemon::acknowledge_restore(ports);
    }
    let Some(daemon) = daemon else {
        let timing = daemon::inline_restore_timing(start);
        let _ = daemon::restore_work(runtime, new_cpus, true, &timing);
        return Ok(());
    };
    daemon::queue_restore(daemon, start).map_err(|_| {
        let mut checks = Checks::empty(Phase::Restore);
        checks.fail(runtime, CheckId::C10, "the time daemon is not running");
        CommandError::Operation
    })
}

/// Prints persisted status after waiting for pending checks.
fn cmd_status(runtime: &Runtime<'_>, out: &mut dyn Write) -> Result<(), CommandError> {
    let (state, completed) = checks::wait_for_checks(false);
    let _ = out.write_all(state.status_markers(runtime.report_only()).as_bytes());
    if completed && state.checks_ok() {
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
        .open_ports()
        .map_err(|error| port_failure(err, error))?;
    let status = ports.read_u8(Port::PortbStatus);
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
        system::take_sample(&ports)
    };
    drop(ports);
    let sample = sample.map_err(|error| {
        report(err, &[format!("nvx-time: {error}\n").as_bytes()]);
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
        .open_ports()
        .map_err(|error| port_failure(err, error))?;
    if ports.read_u8(Port::PortbStatus) & PORTB_GENERATION_ID_AVAILABLE == 0 {
        report(err, &[b"nvx-time: the VM generation ID is unavailable\n"]);
        return Err(CommandError::Operation);
    }
    let mut id = [0_u8; GENERATION_ID_SIZE];
    {
        let _lock = system::lock_portb().map_err(|detail| {
            report(err, &[b"nvx-time: ", &detail, b"\n"]);
            CommandError::Operation
        })?;
        ports.write_u8(Port::PortbStatus, SELECT_GENERATION_ID);
        portb_read(&ports, Port::PortbData, &mut id);
    }
    drop(ports);
    let mut text = decoders::format_hex(&id).into_bytes();
    text.push(b'\n');
    let _ = out.write_all(&text);
    Ok(())
}

/// Returns whether the kernel command line enables report-only mode.
fn cmdline_requests_report_only() -> bool {
    let Ok(text) = sys::read_file(kernel::COMMAND_LINE) else {
        return false;
    };
    text.split(|byte| matches!(byte, b' ' | b'\t' | b'\n'))
        .any(|token| token == REPORT_ONLY_TOKEN)
}

/// Dispatches a production `nvx-time` command.
///
/// A command takes the port permission only for each of its port transactions. When the kernel
/// refused to revoke it, the thread starts nothing more, and the command fails once its work
/// ended, because the thread may still hold some of the permission.
pub(super) fn run(
    arguments: &[&[u8]],
    report_only: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let runtime = Runtime::new(
        &system::RealOps,
        report_only || cmdline_requests_report_only(),
    );
    sys::reset_child_signal();
    let Some((&command, rest)) = arguments.split_first() else {
        return usage(err, USAGE);
    };
    let result = match (command, rest) {
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
    };
    match sys::check_released() {
        Ok(()) => result,
        Err(error) => Err(port_failure(err, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
