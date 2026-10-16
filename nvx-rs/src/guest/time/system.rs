//! The time tools' runtime: persistent state, violation events, port-B transactions, and the
//! wall clock's discipline, over the system layer's owners.

use std::ffi::{CStr, CString};
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::decoders::{self, SAMPLE_SIZE, TimeSample};
use super::events::format_event;
use super::state::TimeState;
use super::{NSEC_PER_MSEC, Phase};
use crate::guest::sys::{
    self, ClockAdjustment, ClockDiscipline, CreatedFile, Errno, FileDescriptor, FileLock, Mode,
    MsrAccess, MsrFile, OpenFlags, Port, PortAccess, kernel,
};

/// Statuses with which a fatal time-ABI failure stops the VM through the control port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PowerOffStatus {
    /// A time-ABI conformance check failed.
    Conformance = 193,
    /// The runtime watcher observed a time-ABI violation.
    Violation = 194,
    /// Restore-time repair could not complete safely.
    Repair = 195,
}

impl PowerOffStatus {
    /// Returns the exact byte written to the control port.
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }
}

/// Selector requesting the restore packet.
pub(crate) const SELECT_PACKET: u8 = 0xa5;
/// Selector requesting the VM generation identifier.
pub(crate) const SELECT_GENERATION_ID: u8 = 0xa6;
/// Selector requesting a host time sample.
pub(crate) const SELECT_TIME_SAMPLE: u8 = 0xa7;
/// Status bit indicating that a restore packet can be read.
pub(crate) const PORTB_PACKET_AVAILABLE: u8 = 0x02;
/// Status bit indicating that a generation identifier can be read.
pub(crate) const PORTB_GENERATION_ID_AVAILABLE: u8 = 0x20;
/// Status bit indicating that the time-sample window is supported.
pub(crate) const PORTB_TIME_WINDOW: u8 = 0x40;
/// Snapshot-port value acknowledging completed restore repair.
pub(crate) const SNAPSHOT_ACKNOWLEDGE: u8 = 2;

/// Top-level NVX runtime directory.
pub(crate) const RUN_DIR: &CStr = c"/run/nvx";
/// Time-tool runtime directory.
pub(crate) const TIME_DIR: &CStr = c"/run/nvx/time";
/// Published time state file.
pub(crate) const STATE_PATH: &CStr = c"/run/nvx/time/state";
/// Temporary state file atomically renamed into place.
const STATE_TEMP_PATH: &CStr = c"/run/nvx/time/.state";
/// Advisory lock protecting state updates.
const STATE_LOCK_PATH: &CStr = c"/run/nvx/time/state.lock";
/// Published time-daemon process identifier.
pub(crate) const DAEMON_PID_PATH: &CStr = c"/run/nvx/time/daemon.pid";
/// Temporary daemon-PID file.
pub(crate) const DAEMON_PID_TEMP_PATH: &CStr = c"/run/nvx/time/daemon.pid.tmp";
/// Saved stall-detector settings during capture/restore.
pub(crate) const SUPPRESSION_PATH: &CStr = c"/run/nvx/time/suppression";
/// Advisory lock protecting suppression state.
const SUPPRESSION_LOCK_PATH: &CStr = c"/run/nvx/time/suppression.lock";
/// Private restore record consumed by the daemon.
pub(crate) const RESTORE_PATH: &CStr = c"/run/nvx/time/restore";
/// Temporary restore-record file.
pub(crate) const RESTORE_TEMP_PATH: &CStr = c"/run/nvx/time/restore.tmp";
/// Advisory lock serializing port-window transactions.
pub(crate) const PORTB_LOCK_PATH: &CStr = c"/run/nvx/portb.lock";
/// Temporary mount point used to force an RCU grace period.
pub(crate) const RCU_SYNC_PATH: &CStr = c"/run/nvx/rcu-sync";

/// Mode of the runtime directories: `rwxr-xr-x`.
const DIRECTORY_MODE: Mode = Mode::S_IRWXU
    .union(Mode::S_IRGRP)
    .union(Mode::S_IXGRP)
    .union(Mode::S_IROTH)
    .union(Mode::S_IXOTH);
/// Mode of the files that other users may read, such as the state file: `rw-r--r--`.
const PUBLIC_FILE_MODE: Mode = Mode::S_IRUSR
    .union(Mode::S_IWUSR)
    .union(Mode::S_IRGRP)
    .union(Mode::S_IROTH);
/// Mode of the lock files, the suppression record, and the restored entropy: `rw-------`.
pub(crate) const PRIVATE_FILE_MODE: Mode = Mode::S_IRUSR.union(Mode::S_IWUSR);

/// Bytes of the C program's buffer for the state file, its terminating NUL included.
const STATE_LIMIT: usize = 2048;
/// Bytes of the C program's buffer for the suppression record, its terminating NUL included.
const SUPPRESSION_LIMIT: usize = 512;
/// Bytes of the C program's buffer for a file that holds one number, its NUL included.
const NUMBER_LIMIT: usize = 64;
/// Longest prefix of a path that a restore diagnostic quotes, as C's `%.200s` did.
const QUOTED_PATH_LIMIT: usize = 200;

/// The stall detectors that capture suppresses, and the value that suppresses each: RCU's stall
/// warnings, the soft-lockup detector, and the hung-task detector. Only the first must exist.
const SUPPRESSED_DETECTORS: [(&CStr, i64); 3] = [
    (kernel::RCU_STALL_SUPPRESS, 1),
    (kernel::SOFT_WATCHDOG, 0),
    (kernel::HUNG_TASK_TIMEOUT, 0),
];

/// The priority prefix of an event that the program writes to the kernel log: `KERN_CRIT`.
const KERNEL_LOG_PRIORITY: &[u8] = b"<2>";

/// Number of host samples considered when selecting the narrowest.
const SAMPLE_ATTEMPTS: usize = 3;

/// Seconds after which RCU reports a CPU stall in NVX's guest kernels, which the checks require
/// and the daemon assumes when it cannot read the setting.
pub(crate) const DEFAULT_STALL_TIMEOUT_S: i64 = 21;

/// Bytes of the C program's buffer for the kernel command line, its NUL included.
pub(crate) const COMMAND_LINE_LIMIT: usize = 4096;

/// Bytes of the C program's buffer for a CPU list, its NUL included.
pub(crate) const CPU_LIST_LIMIT: usize = 256;

/// The time ABI's I/O ports, which a fake replaces in tests.
pub(crate) trait PortIo: Sync {
    /// Reads one byte from `port`.
    fn inb(&self, port: Port) -> u8;
    /// Reads four bytes from `port` with one scalar access.
    fn inl(&self, port: Port) -> u32;
    /// Writes one byte to `port`.
    fn outb(&self, port: Port, value: u8);
}

impl PortIo for PortAccess {
    fn inb(&self, port: Port) -> u8 {
        self.read_u8(port)
    }

    fn inl(&self, port: Port) -> u32 {
        self.read_u32(port)
    }

    fn outb(&self, port: Port, value: u8) {
        self.write_u8(port, value);
    }
}

/// The privileged operations of the time tools, which a fake replaces in tests, so that their
/// retry and failure paths are testable.
pub(crate) trait PrivilegedOps: Sync {
    /// Returns the time ABI's ports, or why the process may not access them.
    fn ports(&self) -> Result<&dyn PortIo, Errno>;
    /// Opens the MSR device of `cpu`.
    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno>;
    /// Reads the register `msr`.
    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno>;
    /// Writes `value` to the register `msr`.
    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno>;
    /// Applies `adjustment` to the wall clock's discipline.
    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno>;
    /// Stops the VM with `status`, which in production does not return.
    fn power_off(&self, status: PowerOffStatus);
}

/// An MSR device, or the token of a test's fake.
pub(crate) enum MsrDescriptor {
    /// A device that the kernel opened.
    Real(MsrFile),
    /// A test's token, which owns no descriptor.
    #[cfg(test)]
    Fake,
}

/// The production privileged operations.
///
/// They request the port permission when the command starts, on the main thread, because Linux
/// grants it per thread, and the threads and processes that the command starts later inherit it.
pub(crate) struct RealOps {
    /// The port permission, or why the kernel refused it.
    ports: Result<PortAccess, Errno>,
}

impl RealOps {
    /// Requests the port permission for the command.
    pub(crate) fn new() -> Self {
        Self {
            ports: PortAccess::request(),
        }
    }
}

impl PrivilegedOps for RealOps {
    fn ports(&self) -> Result<&dyn PortIo, Errno> {
        match &self.ports {
            Ok(ports) => Ok(ports),
            Err(error) => Err(*error),
        }
    }

    fn open_msr(&self, cpu: usize, access: MsrAccess) -> Result<MsrDescriptor, Errno> {
        MsrFile::open(cpu, access).map(MsrDescriptor::Real)
    }

    fn read_msr(&self, descriptor: &MsrDescriptor, msr: u32) -> Result<u64, Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.read(msr),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

    fn write_msr(&self, descriptor: &MsrDescriptor, msr: u32, value: u64) -> Result<(), Errno> {
        match descriptor {
            MsrDescriptor::Real(file) => file.write(msr, value),
            #[cfg(test)]
            MsrDescriptor::Fake => Err(Errno::EIO),
        }
    }

    fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
        sys::adjust_clock(adjustment)
    }

    fn power_off(&self, status: PowerOffStatus) {
        sys::power_off(status.code())
    }
}

/// Returns the negative `errno` of an MSR access, or zero for one that succeeded, as the time
/// ABI's diagnostics print it.
pub(crate) fn msr_error<T>(result: &Result<T, Errno>) -> i32 {
    result.as_ref().err().map_or(0, |&error| -(error as i32))
}

/// Shared runtime state and injectable privileged operations.
pub(crate) struct Runtime<'a> {
    /// Privileged-operation implementation.
    pub(crate) ops: &'a dyn PrivilegedOps,
    /// Whether violations are reported instead of powering off the VM.
    report_only: bool,
    /// Current VM generation.
    generation: AtomicU32,
    /// Whether an asynchronous child may have interrupted console output.
    async_output: AtomicBool,
    /// Serializes multi-destination violation records.
    report_lock: Mutex<()>,
}

impl<'a> Runtime<'a> {
    /// Creates runtime state around one privileged-operation implementation.
    pub(crate) fn new(ops: &'a dyn PrivilegedOps, report_only: bool) -> Self {
        Self {
            ops,
            report_only,
            generation: AtomicU32::new(0),
            async_output: AtomicBool::new(false),
            report_lock: Mutex::new(()),
        }
    }

    /// Returns whether conformance failures are report-only.
    pub(crate) fn report_only(&self) -> bool {
        self.report_only
    }

    /// Returns the current VM generation.
    pub(crate) fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Publishes the current VM generation to concurrent checks.
    pub(crate) fn set_generation(&self, value: u32) {
        self.generation.store(value, Ordering::Relaxed);
    }

    /// Marks whether console output may need a leading newline.
    pub(crate) fn set_async_output(&self, value: bool) {
        self.async_output.store(value, Ordering::Relaxed);
    }

    /// Returns the time ABI's ports, or why the process may not access them.
    pub(crate) fn ports(&self) -> Result<&dyn PortIo, Errno> {
        self.ops.ports()
    }

    /// Emits one serialized violation record to every ABI destination.
    pub(crate) fn emit_event(&self, code: &[u8], source: &[u8], phase: Phase, detail: &[u8]) {
        let _guard = self.report_lock.lock().ok();
        self.emit_event_unlocked(code, source, phase, detail);
    }

    /// Emits an event while the caller already owns `report_lock`.
    fn emit_event_unlocked(&self, code: &[u8], source: &[u8], phase: Phase, detail: &[u8]) {
        let line = format_event(
            self.report_only(),
            code,
            source,
            phase,
            self.generation(),
            clock_ns(sys::Clock::Boottime),
            detail,
        );
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC;
        if let Ok(log) = sys::open(kernel::KERNEL_LOG_DEVICE, flags, Mode::empty()) {
            let _ = log.write_all(&[KERNEL_LOG_PRIORITY, line.as_slice()].concat());
        }
        if let Ok(ports) = self.ports() {
            if self.async_output.load(Ordering::Relaxed) {
                ports.outb(Port::PortbData, b'\n');
            }
            for &byte in &line {
                ports.outb(Port::PortbData, byte);
            }
        }
        self.console_write(&line);
        let _ = count_violation();
    }

    /// Reports a conformance failure and powers off unless report-only.
    pub(crate) fn conformance_failure(&self, code: &[u8], phase: Phase, detail: &[u8]) {
        let _guard = self.report_lock.lock().ok();
        self.emit_event_unlocked(code, b"conformance", phase, detail);
        if !self.report_only() {
            self.ops.power_off(PowerOffStatus::Conformance);
        }
    }

    /// Emits a fatal event and requests its typed power-off status.
    pub(crate) fn fail_fatal(
        &self,
        status: PowerOffStatus,
        code: &[u8],
        source: &[u8],
        phase: Phase,
        detail: &[u8],
    ) {
        self.emit_event(code, source, phase, detail);
        if !self.report_only() {
            self.ops.power_off(status);
        }
    }

    /// Writes one event to the console or standard error fallback.
    fn console_write(&self, line: &[u8]) {
        let flags = OpenFlags::O_WRONLY | OpenFlags::O_NOCTTY | OpenFlags::O_CLOEXEC;
        let separator: &[u8] = if self.async_output.load(Ordering::Relaxed) {
            b"\n"
        } else {
            b""
        };
        match sys::open(kernel::CONSOLE_DEVICE, flags, Mode::empty()) {
            Ok(console) => {
                let _ = console.write_all(separator);
                let _ = console.write_all(line);
            }
            Err(_) => {
                sys::write_standard_error(separator);
                sys::write_standard_error(line);
            }
        }
    }
}

/// Reads one Linux clock as wrapping nanoseconds, or zero when it cannot be read.
pub(crate) fn clock_ns(clock: sys::Clock) -> i64 {
    sys::clock_time(clock)
        .unwrap_or_default()
        .wrapping_nanoseconds()
}

/// Returns process-wide CPU time in wrapping nanoseconds.
pub(crate) fn cpu_time_ns() -> i64 {
    clock_ns(sys::Clock::ProcessCpuTime)
}

/// Creates the NVX runtime directories, ignoring preexistence and failures.
pub(crate) fn make_runtime_directories() {
    let _ = sys::create_directory(RUN_DIR, DIRECTORY_MODE);
    let _ = sys::create_directory(TIME_DIR, DIRECTORY_MODE);
}

/// Reads at most `limit - 1` bytes of `path`, as the C program read into a `limit`-byte buffer
/// with room for its NUL, and reports a failure to close the file.
pub(crate) fn read_text(path: &CStr, limit: usize) -> Result<Vec<u8>, Errno> {
    let mut file = sys::open(
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut text = Vec::new();
    Read::take(&mut file, limit.saturating_sub(1) as u64)
        .read_to_end(&mut text)
        .map_err(|error| sys::errno_of(&error))?;
    file.close().map(|()| text)
}

/// Reads the whole of `path`.
pub(crate) fn read_file(path: &CStr) -> Result<Vec<u8>, Errno> {
    let mut file = sys::open(
        path,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| sys::errno_of(&error))?;
    Ok(bytes)
}

/// Reads one exact signed decimal value from a bounded text file.
pub(crate) fn read_i64(path: &CStr) -> Result<i64, Errno> {
    let text = read_text(path, NUMBER_LIMIT)?;
    decoders::parse_i64_exact(&text).ok_or(Errno::EINVAL)
}

/// Replaces an existing file's bytes and reports write or close failure.
pub(crate) fn write_text(path: &CStr, text: &[u8]) -> Result<(), Errno> {
    let file = sys::open(
        path,
        OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
    )?;
    file.write_all(text)?;
    file.close()
}

/// Writes one signed decimal value followed by a newline.
pub(crate) fn write_i64(path: &CStr, value: i64) -> Result<(), Errno> {
    write_text(path, format!("{value}\n").as_bytes())
}

/// Durably writes `text` to `temporary`, and atomically renames it to `path`. A failure after
/// the file was created removes it.
pub(crate) fn replace_file(path: &CStr, temporary: &CStr, text: &[u8]) -> Result<(), Errno> {
    let file = CreatedFile::create(temporary, false, PUBLIC_FILE_MODE)?;
    file.write_all(text)?;
    file.sync()?;
    file.rename(path)
}

/// Opens or creates a runtime lock file and takes its exclusive advisory lock.
pub(crate) fn lock_file(path: &CStr) -> Result<FileLock, Errno> {
    make_runtime_directories();
    sys::lock_file(path, PRIVATE_FILE_MODE)
}

/// Loads and parses the published time state.
pub(crate) fn state_load() -> Result<TimeState, Errno> {
    let text = read_text(STATE_PATH, STATE_LIMIT)?;
    TimeState::parse(&text).ok_or(Errno::EINVAL)
}

/// Atomically persists one complete time state.
pub(crate) fn state_store(state: &TimeState) -> Result<(), Errno> {
    replace_file(STATE_PATH, STATE_TEMP_PATH, &state.format())
}

/// Applies one read-modify-write update to the time state under its lock.
pub(crate) fn state_apply(update: impl FnOnce(&mut TimeState)) -> Result<(), Errno> {
    let _lock = lock_file(STATE_LOCK_PATH)?;
    let mut state = state_load().unwrap_or_default();
    update(&mut state);
    state_store(&state)
}

/// Increments the persisted violation counter when state exists.
fn count_violation() -> Result<(), Errno> {
    if !sys::path_exists(STATE_PATH) {
        return Ok(());
    }
    state_apply(|state| state.violations = state.violations.wrapping_add(1))
}

/// Loads validated TSC and LAPIC rates from state.
pub(crate) fn rates_load() -> Result<(u64, u64), Errno> {
    let state = state_load()?;
    if state.tsc_hz == 0 || state.lapic_hz == 0 {
        Err(Errno::EINVAL)
    } else {
        Ok((state.tsc_hz, state.lapic_hz))
    }
}

/// Saves the stall detectors' settings in the suppression record, and suppresses them for a
/// snapshot capture. A record that exists already, of a pending capture, is `EEXIST`.
pub(crate) fn suppression_save() -> Result<(), Errno> {
    let mut saved = Vec::with_capacity(SUPPRESSION_LIMIT);
    for (index, (path, _)) in SUPPRESSED_DETECTORS.iter().enumerate() {
        if let Err(error) = sys::check_exists(path) {
            if index == 0 {
                return Err(error);
            }
            continue;
        }
        let value = read_i64(path)?;
        saved.extend_from_slice(path.to_bytes());
        saved.extend_from_slice(format!("={value}\n").as_bytes());
    }
    let record = CreatedFile::create(SUPPRESSION_PATH, true, PRIVATE_FILE_MODE)?;
    record.write_all(&saved)?;
    // As in the C program, a record that fails to close stays, and the suppression goes on.
    let _ = record.keep();
    for (path, value) in SUPPRESSED_DETECTORS {
        if sys::path_exists(path) {
            write_i64(path, value)?;
        }
    }
    Ok(())
}

/// Formats a diagnostic: `prefix`, then the description of `error`.
fn describe(prefix: &[u8], error: Errno) -> Vec<u8> {
    [prefix, error.desc().as_bytes()].concat()
}

/// Restores the stall detectors' settings from the suppression record, and removes the record.
/// A missing record succeeds when `missing_ok`.
pub(crate) fn suppression_restore(missing_ok: bool) -> Result<(), Vec<u8>> {
    let _lock = lock_file(SUPPRESSION_LOCK_PATH)
        .map_err(|error| describe(b"lock /run/nvx/time/suppression.lock: ", error))?;
    let text = match read_text(SUPPRESSION_PATH, SUPPRESSION_LIMIT) {
        Ok(text) => text,
        Err(Errno::ENOENT) if missing_ok => return Ok(()),
        Err(error) => return Err(describe(b"read /run/nvx/time/suppression: ", error)),
    };
    for line in text
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Some(equal) = line.iter().position(|byte| *byte == b'=') else {
            break;
        };
        let Ok(path) = CString::new(&line[..equal]) else {
            break;
        };
        if let Err(error) = write_text(&path, &line[equal + 1..]) {
            let quoted = &line[..equal.min(QUOTED_PATH_LIMIT)];
            return Err(describe(
                &[b"restore ".as_slice(), quoted, b": "].concat(),
                error,
            ));
        }
    }
    sys::unlink(SUPPRESSION_PATH)
        .map_err(|error| describe(b"remove /run/nvx/time/suppression: ", error))
}

/// Reads a byte buffer using one scalar four-byte port access per chunk.
pub(crate) fn portb_read(ports: &dyn PortIo, port: Port, buffer: &mut [u8]) {
    for chunk in buffer.chunks_mut(size_of::<u32>()) {
        let bytes = ports.inl(port).to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// Locks the shared port-window transaction and formats lock failures.
pub(crate) fn lock_portb() -> Result<FileLock, Vec<u8>> {
    lock_file(PORTB_LOCK_PATH).map_err(|error| describe(b"portb lock: ", error))
}

/// Requests and decodes one host time sample, bracketed by realtime reads.
pub(crate) fn take_sample(ports: &dyn PortIo) -> Result<TimeSample, String> {
    let mut bytes = [0_u8; SAMPLE_SIZE];
    let t0 = clock_ns(sys::Clock::Realtime);
    ports.outb(Port::PortbStatus, SELECT_TIME_SAMPLE);
    let t1 = clock_ns(sys::Clock::Realtime);
    portb_read(ports, Port::PortbWindow, &mut bytes);
    Ok(TimeSample {
        t0,
        t1,
        ..TimeSample::parse(&bytes)?
    })
}

/// Narrowest valid sample observed during one attempt group.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BestSample {
    /// Guest-to-host offset in nanoseconds.
    pub(crate) theta: i64,
    /// Half-round-trip uncertainty in nanoseconds.
    pub(crate) epsilon: i64,
}

/// Whether the selected sample satisfied the requested uncertainty bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleResult {
    /// The sample was within the bound.
    Accepted,
    /// A valid sample was selected but remained wider than the bound.
    Wide,
}

/// Why no sample could be selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SampleError {
    /// A sample came from another VM generation, which a restore must handle first.
    Generation(Vec<u8>),
    /// No sample was valid, or the ports are unavailable.
    Unavailable(Vec<u8>),
}

impl SampleError {
    /// Returns the diagnostic of the failure.
    pub(crate) fn detail(self) -> Vec<u8> {
        match self {
            Self::Generation(detail) | Self::Unavailable(detail) => detail,
        }
    }
}

/// Selects the lowest-uncertainty valid sample from a bounded attempt count.
pub(crate) fn best_sample(
    runtime: &Runtime<'_>,
    attempts: usize,
    bound_ns: i64,
    generation: u32,
) -> Result<(SampleResult, BestSample), SampleError> {
    let ports = runtime
        .ports()
        .map_err(|error| SampleError::Unavailable(describe(b"ioperm: ", error)))?;
    let status = ports.inb(Port::PortbStatus);
    if status & PORTB_TIME_WINDOW == 0 {
        return Err(SampleError::Unavailable(
            format!("portb status 0x{status:02x} has no time-sample window").into_bytes(),
        ));
    }
    let mut best = None;
    let mut detail = b"no valid time sample".to_vec();
    for _ in 0..attempts {
        let sample = match take_sample(ports) {
            Ok(sample) => sample,
            Err(error) => {
                detail = error.into_bytes();
                continue;
            }
        };
        if sample.generation != generation {
            return Err(SampleError::Generation(
                format!(
                    "time sample generation {}, expected {generation}",
                    sample.generation
                )
                .into_bytes(),
            ));
        }
        let (theta, epsilon) = decoders::pair_clocks(sample.t0, sample.t1, sample.utc_ns);
        let candidate = BestSample { theta, epsilon };
        if best
            .as_ref()
            .is_none_or(|current: &BestSample| epsilon < current.epsilon)
        {
            best = Some(candidate);
        }
        if epsilon <= bound_ns {
            return Ok((SampleResult::Accepted, candidate));
        }
        detail = format!("time sample uncertainty {epsilon} ns exceeds {bound_ns} ns").into_bytes();
    }
    best.map(|best| (SampleResult::Wide, best))
        .ok_or(SampleError::Unavailable(detail))
}

/// Steps realtime by a signed nanosecond offset.
pub(crate) fn step_clock(runtime: &Runtime<'_>, offset_ns: i64) -> Result<(), Errno> {
    let (seconds, nanoseconds) = decoders::split_offset(offset_ns);
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Step {
            seconds,
            nanoseconds,
        })
        .map(drop)
}

/// Programs Linux's scaled-ppm frequency correction.
pub(crate) fn set_frequency(runtime: &Runtime<'_>, scaled_ppm: i64) -> Result<(), Errno> {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Frequency { scaled_ppm })
        .map(drop)
}

/// Queries the current scaled-ppm frequency, returning zero on failure.
pub(crate) fn current_frequency(runtime: &Runtime<'_>) -> i64 {
    runtime
        .ops
        .adjust_clock(ClockAdjustment::Query)
        .map_or(0, |discipline| discipline.scaled_ppm)
}

/// Takes the initial bounded sample and steps realtime to it.
pub(crate) fn initial_sample(runtime: &Runtime<'_>) -> Result<BestSample, Vec<u8>> {
    let (_, sample) = {
        let _lock = lock_portb()?;
        best_sample(
            runtime,
            SAMPLE_ATTEMPTS,
            NSEC_PER_MSEC,
            runtime.generation(),
        )
        .map_err(SampleError::detail)?
    };
    step_clock(runtime, sample.theta).map_err(|error| describe(b"adjtimex step: ", error))?;
    Ok(sample)
}

/// Writes `entropy` at the start of the restored entropy file, and reports a failure to write
/// or close it with the file's path.
pub(crate) fn write_entropy(
    file: FileDescriptor,
    path: &CStr,
    entropy: &[u8],
) -> Result<(), Vec<u8>> {
    let written = loop {
        match file.pwrite(entropy, 0) {
            Err(Errno::EINTR) => {}
            result => break result,
        }
    };
    let closed = file.close();
    let error = match (written, closed) {
        (Ok(count), Ok(())) if count == entropy.len() => return Ok(()),
        (Err(error), _) | (Ok(_), Err(error)) => error,
        (Ok(_), Ok(())) => Errno::EIO,
    };
    let path = &path.to_bytes()[..path.to_bytes().len().min(QUOTED_PATH_LIMIT)];
    Err(describe(
        &[b"write ".as_slice(), path, b": "].concat(),
        error,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    //! A fake of the privileged operations, and checks of the runtime over it.

    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::guest::sys::x86::msr;

    /// Privileged operations that record their requests and replay canned port input.
    pub(crate) struct FakeOps {
        /// The result of the port permission.
        pub(crate) ports: Result<(), Errno>,
        /// Values that successive port reads return.
        pub(crate) input: Mutex<VecDeque<u32>>,
        /// Port writes, in order.
        pub(crate) output: Mutex<Vec<(Port, u8)>>,
        /// Clock adjustments, in order.
        pub(crate) adjustments: Mutex<Vec<ClockAdjustment>>,
        /// The last power-off status, or zero.
        pub(crate) power: AtomicUsize,
    }

    impl FakeOps {
        /// Returns a fake whose port reads return `input`.
        pub(crate) fn new(input: impl IntoIterator<Item = u32>) -> Self {
            Self {
                ports: Ok(()),
                input: Mutex::new(input.into_iter().collect()),
                output: Mutex::new(Vec::new()),
                adjustments: Mutex::new(Vec::new()),
                power: AtomicUsize::new(0),
            }
        }
    }

    impl PortIo for FakeOps {
        fn inb(&self, _: Port) -> u8 {
            self.input.lock().unwrap().pop_front().unwrap_or_default() as u8
        }

        fn inl(&self, _: Port) -> u32 {
            self.input.lock().unwrap().pop_front().unwrap_or_default()
        }

        fn outb(&self, port: Port, value: u8) {
            self.output.lock().unwrap().push((port, value));
        }
    }

    impl PrivilegedOps for FakeOps {
        fn ports(&self) -> Result<&dyn PortIo, Errno> {
            self.ports.map(|()| self as &dyn PortIo)
        }

        fn open_msr(&self, _: usize, _: MsrAccess) -> Result<MsrDescriptor, Errno> {
            Ok(MsrDescriptor::Fake)
        }

        fn read_msr(&self, _: &MsrDescriptor, _: u32) -> Result<u64, Errno> {
            Err(Errno::EIO)
        }

        fn write_msr(&self, _: &MsrDescriptor, _: u32, _: u64) -> Result<(), Errno> {
            Err(Errno::EIO)
        }

        fn adjust_clock(&self, adjustment: ClockAdjustment) -> Result<ClockDiscipline, Errno> {
            self.adjustments.lock().unwrap().push(adjustment);
            Ok(ClockDiscipline::default())
        }

        fn power_off(&self, status: PowerOffStatus) {
            self.power
                .store(usize::from(status.code()), Ordering::Relaxed);
        }
    }

    #[test]
    fn port_reader_uses_one_four_byte_read_per_chunk() {
        let ops = FakeOps::new([0x0403_0201, 0x0807_0605]);
        let mut bytes = [0; 7];
        portb_read(&ops, Port::PortbData, &mut bytes);
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7]);
        assert!(ops.input.lock().unwrap().is_empty());
    }

    #[test]
    fn packet_without_ranges_costs_exactly_twenty_four_data_reads() {
        let ops = FakeOps::new(0..24);
        let mut header = [0; decoders::PACKET_HEADER_SIZE];
        let mut entropy = [0; decoders::PACKET_ENTROPY_SIZE];
        portb_read(&ops, Port::PortbData, &mut header);
        portb_read(&ops, Port::PortbData, &mut entropy);
        assert!(ops.input.lock().unwrap().is_empty());
    }

    #[test]
    fn msr_errors_print_as_negative_errno_values() {
        let ops = FakeOps::new([]);
        let runtime = Runtime::new(&ops, false);
        let descriptor = MsrDescriptor::Fake;
        let read = runtime.ops.read_msr(&descriptor, msr::HV_GUEST_OS_ID);
        assert_eq!(msr_error(&read), -(Errno::EIO as i32));
        let written = runtime.ops.write_msr(&descriptor, msr::HV_GUEST_OS_ID, 0);
        assert_eq!(msr_error(&written), -(Errno::EIO as i32));
        assert_eq!(msr_error(&Ok::<u64, Errno>(1)), 0);
    }

    #[test]
    fn a_refused_port_permission_reaches_the_sample_diagnostic() {
        let mut ops = FakeOps::new([]);
        ops.ports = Err(Errno::EPERM);
        let runtime = Runtime::new(&ops, false);
        let error = best_sample(&runtime, 3, NSEC_PER_MSEC, 0).unwrap_err();
        let expected = [b"ioperm: ".as_slice(), Errno::EPERM.desc().as_bytes()].concat();
        assert_eq!(error, SampleError::Unavailable(expected));
    }

    #[test]
    fn a_sample_from_another_generation_is_a_generation_error() {
        let mut sample = [0_u8; SAMPLE_SIZE];
        sample[0] = 1;
        sample[4..8].copy_from_slice(&7_u32.to_le_bytes());
        sample[8..16].copy_from_slice(&1_u64.to_le_bytes());
        let words = sample
            .chunks(size_of::<u32>())
            .map(|word| u32::from_ne_bytes(word.try_into().unwrap()));
        let ops = FakeOps::new(std::iter::once(u32::from(PORTB_TIME_WINDOW)).chain(words));
        let runtime = Runtime::new(&ops, false);
        let error = best_sample(&runtime, 1, NSEC_PER_MSEC, 6).unwrap_err();
        assert!(matches!(error, SampleError::Generation(_)), "{error:?}");
        assert_eq!(
            ops.output.lock().unwrap().as_slice(),
            [(Port::PortbStatus, SELECT_TIME_SAMPLE)]
        );
    }

    #[test]
    fn privileged_adjustment_and_power_paths_are_injectable() {
        let ops = FakeOps::new([]);
        let runtime = Runtime::new(&ops, false);
        step_clock(&runtime, -1_128_000_000).unwrap();
        set_frequency(&runtime, 65_536).unwrap();
        assert_eq!(current_frequency(&runtime), 0);
        assert_eq!(
            ops.adjustments.lock().unwrap().as_slice(),
            [
                ClockAdjustment::Step {
                    seconds: -2,
                    nanoseconds: 872_000_000,
                },
                ClockAdjustment::Frequency { scaled_ppm: 65_536 },
                ClockAdjustment::Query,
            ]
        );
        runtime.ops.power_off(PowerOffStatus::Repair);
        assert_eq!(ops.power.load(Ordering::Relaxed), 195);
    }

    #[test]
    fn modes_of_the_runtime_files_match_the_c_program() {
        assert_eq!(DIRECTORY_MODE.bits(), 0o755);
        assert_eq!(PUBLIC_FILE_MODE.bits(), 0o644);
        assert_eq!(PRIVATE_FILE_MODE.bits(), 0o600);
    }
}
