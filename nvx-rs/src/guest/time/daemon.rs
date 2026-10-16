//! The time daemon: deferred work, violation watching, the wall clock's discipline, and repair
//! step 7.

use std::ffi::CStr;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use super::checks::{self, Checks};
use super::decoders;
use super::state::TimeState;
use super::system::{
    self, DAEMON_PID_PATH, DAEMON_PID_TEMP_PATH, DEFAULT_STALL_TIMEOUT_S, PowerOffStatus,
    RCU_SYNC_PATH, RESTORE_PATH, Runtime, SNAPSHOT_ACKNOWLEDGE, SampleError, SampleResult,
    clock_ns, cpu_time_ns, current_frequency, read_i64, read_text, set_frequency, state_apply,
    state_load, suppression_restore, write_i64, write_text,
};
use super::{
    DEFERRED_START_NS, IDLE_BOUND_NS, MAX_CPUS, NSEC_PER_MSEC, NSEC_PER_SEC, NSEC_PER_USEC, Phase,
};
use crate::guest::sys::{
    self, ChildStatus, Errno, FileDescriptor, ForkResult, Mode, Pollable, Port, ProcessId, Signal,
    SignalDescriptor, Timer, kernel,
};

/// Maximum accepted polling bracket width.
const POLL_BOUND_NS: i64 = 50_000;
/// Samples attempted by each discipline poll.
const SAMPLE_ATTEMPTS: usize = 3;
/// Maximum age of the latest accepted sample while synchronized.
const SYNCHRONIZED_AGE_NS: i64 = 128 * NSEC_PER_SEC;
/// Poll interval during initial convergence.
const FAST_PERIOD: Duration = Duration::from_secs(16);
/// Poll interval after initial convergence.
const SLOW_PERIOD: Duration = Duration::from_secs(64);
/// Number of initial fast polls.
const FAST_POLLS: i32 = 4;
/// Maximum wait for stable clock-event state during restore.
const TIMER_LIST_WAIT_NS: i64 = 200 * NSEC_PER_MSEC;
/// Pause before the daemon polls again after `poll` failed for a reason other than a signal.
const POLL_RETRY: Duration = Duration::from_secs(1);
/// Bytes of the largest kernel log record that the daemon reads, as the C program's buffer.
const KERNEL_LOG_RECORD_LIMIT: usize = 8192;
/// Longest prefix of a kernel message that a violation event quotes.
const WATCHED_MESSAGE_LIMIT: usize = 400;
/// Bytes of the C program's buffer for the restore record, its terminating NUL included.
const RESTORE_LIMIT: usize = 256;
/// The daemon's out-of-memory score adjustment, Linux's `OOM_SCORE_ADJ_MIN`, which exempts it
/// from the out-of-memory killer.
const OOM_SCORE_ADJUSTMENT: &[u8] = b"-1000";
/// The signals that the daemon reads from its signal descriptor: a queued restore, a stop, and
/// the end of a restore worker.
const DAEMON_SIGNALS: [Signal; 3] = [Signal::SIGUSR1, Signal::SIGTERM, Signal::SIGCHLD];
/// The signals that the daemon ignores: the hangup of its session, and writes to a closed pipe.
const IGNORED_SIGNALS: [Signal; 2] = [Signal::SIGHUP, Signal::SIGPIPE];
/// The source name of the tmpfs that forces an RCU grace period.
const RCU_SYNC_SOURCE: &CStr = c"nvx-rcu-sync";
/// The mount options of that tmpfs: the smallest size, one page.
const RCU_SYNC_OPTIONS: &CStr = c"size=4k";
/// The value of `rcu_normal` that forces normal, rather than expedited, grace periods.
const RCU_NORMAL_FORCED: i64 = 1;

/// Join-on-drop owner of the thread that moves the process's threads from idle to normal
/// scheduling once deferred work has run for `IDLE_BOUND_NS`.
///
/// Dropping it before then cancels the promotion, and the caller keeps its policy.
pub(crate) struct Promoter {
    /// Shared stop flag and notification condition.
    stop: Arc<(Mutex<bool>, Condvar)>,
    /// Running promoter thread, until it is joined.
    thread: Option<thread::JoinHandle<()>>,
}

impl Promoter {
    /// Starts a promoter whose deadline is `IDLE_BOUND_NS` after the monotonic `start_ns`.
    pub(crate) fn start(start_ns: i64) -> Option<Self> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let child = Arc::clone(&stop);
        let deadline = start_ns.wrapping_add(IDLE_BOUND_NS);
        let thread = thread::Builder::new()
            .name("nvx-time-promote".to_owned())
            .spawn(move || {
                let remaining = deadline.wrapping_sub(clock_ns(sys::Clock::Monotonic));
                let timeout = Duration::from_nanos(remaining.max(0) as u64);
                let (lock, changed) = &*child;
                let Ok(stopped) = lock.lock() else {
                    return;
                };
                let Ok((stopped, result)) =
                    changed.wait_timeout_while(stopped, timeout, |stopped| !*stopped)
                else {
                    return;
                };
                if !*stopped && result.timed_out() {
                    sys::promote_all_threads();
                }
            })
            .ok()?;
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Waits for the promoter to promote the threads at its deadline.
    pub(crate) fn finish(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Cancels a pending promotion and joins the thread, as dropping the promoter does.
    pub(crate) fn stop(self) {}
}

impl Drop for Promoter {
    /// Cancels a promotion that is still pending and joins the thread.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        {
            let (lock, changed) = &*self.stop;
            if let Ok(mut stopped) = lock.lock() {
                *stopped = true;
                changed.notify_one();
            }
        }
        let _ = thread.join();
    }
}

/// Drains structured kernel log records and supplies each one's message.
///
/// It returns when no record is left, or with the error of a failed read, such as `EPIPE` for
/// records that the kernel overwrote before the daemon read them.
fn kmsg_drain(log: &FileDescriptor, mut consume: impl FnMut(&[u8])) -> Result<(), Errno> {
    let mut record = [0_u8; KERNEL_LOG_RECORD_LIMIT];
    loop {
        let count = match log.read(&mut record[..KERNEL_LOG_RECORD_LIMIT - 1]) {
            Ok(count) => count,
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => return Ok(()),
            Err(error) => return Err(error),
        };
        if count == 0 {
            return Ok(());
        }
        let record = &record[..count];
        let Some(separator) = record.iter().position(|byte| *byte == b';') else {
            continue;
        };
        let message = &record[separator + 1..];
        let end = message
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(message.len());
        consume(&message[..end]);
    }
}

/// State update produced by one discipline poll.
struct PollOutcome {
    /// Whether a sample was accepted.
    accepted: bool,
    /// Whether the accepted correction stepped realtime.
    stepped: bool,
    /// Whether the accepted sample is recent enough for synchronization.
    synchronized: bool,
    /// Accepted clock offset in nanoseconds.
    theta: i64,
    /// Accepted uncertainty in nanoseconds.
    epsilon: i64,
    /// Realtime immediately after a step.
    step_realtime_ns: i64,
    /// Programmed scaled-ppm frequency.
    frequency: i64,
}

/// Applies one poll outcome to persistent time state.
fn apply_poll(state: &mut TimeState, outcome: &PollOutcome) {
    if outcome.accepted {
        state.samples = state.samples.wrapping_add(1);
        state.offset_ns = outcome.theta;
        state.uncertainty_ns = outcome.epsilon;
        state.frequency_ppb = decoders::frequency_to_ppb(outcome.frequency);
        state.last_sample_error = b"none".to_vec();
        if outcome.stepped {
            state.discontinuities = state.discontinuities.wrapping_add(1);
            state.last_discontinuity = b"step".to_vec();
            state.last_step_ns = outcome.theta;
            state.last_step_realtime_ns = outcome.step_realtime_ns;
        }
    } else {
        state.rejected_samples = state.rejected_samples.wrapping_add(1);
        state.last_sample_error = b"G_SAMPLE_UNCERTAIN".to_vec();
    }
    state.synchronized = u64::from(outcome.synchronized);
}

/// Owned resources and mutable counters of the long-running time daemon.
pub(crate) struct Daemon {
    /// Nonblocking kernel-log stream.
    kmsg: FileDescriptor,
    /// Blocked-signal delivery descriptor.
    signals: SignalDescriptor,
    /// Discipline-poll timer descriptor.
    timer: Timer,
    /// Completed discipline polls.
    polls: i32,
    /// Monotonic time of the latest accepted sample.
    last_accepted_ns: i64,
    /// Active restore worker, if any.
    worker_pid: Option<ProcessId>,
    /// Deadline for promoting the restore worker.
    worker_promote_ns: i64,
    /// Deadline for releasing stall suppression.
    worker_deadline_ns: i64,
    /// Whether suppression was already released for this worker.
    suppression_released: bool,
    /// Whether queued restore work awaits its defer interval.
    restore_pending: bool,
    /// Monotonic time at which queued restore work may start.
    restore_due_ns: i64,
    /// Restore readiness latency carried by the queued signal.
    readiness_us: i64,
}

/// Arms the daemon timer with the current fast or slow interval.
fn arm_timer(daemon: &Daemon) -> Result<(), Errno> {
    daemon.timer.arm_after(if daemon.polls < FAST_POLLS {
        FAST_PERIOD
    } else {
        SLOW_PERIOD
    })
}

/// Drains new kernel messages and raises matching runtime violations.
fn watch_kernel_log(runtime: &Runtime<'_>, daemon: &Daemon) {
    let result = kmsg_drain(&daemon.kmsg, |message| {
        if let Some(code) = decoders::match_watch_rule(message) {
            runtime.fail_fatal(
                PowerOffStatus::Violation,
                code.as_bytes(),
                b"watcher",
                Phase::Runtime,
                &message[..message.len().min(WATCHED_MESSAGE_LIMIT)],
            );
        }
    });
    if result == Err(Errno::EPIPE) {
        runtime.fail_fatal(
            PowerOffStatus::Violation,
            b"G_KMSG_OVERRUN",
            b"watcher",
            Phase::Runtime,
            b"kernel log records were lost (EPIPE)",
        );
    }
}

/// Takes one host sample and updates Linux clock discipline and state.
fn discipline_poll(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    let stalls = read_i64(kernel::RCU_STALL_COUNT).unwrap_or(0);
    if stalls != 0 {
        runtime.fail_fatal(
            PowerOffStatus::Violation,
            b"G_RCU_STALL",
            b"watcher",
            Phase::Runtime,
            format!("rcu_stall_count is {stalls}").as_bytes(),
        );
    }
    let fast = daemon.polls < FAST_POLLS;
    daemon.polls += 1;
    let mut outcome = PollOutcome {
        accepted: false,
        stepped: false,
        synchronized: false,
        theta: 0,
        epsilon: 0,
        step_realtime_ns: 0,
        frequency: 0,
    };
    let _lock = match system::lock_portb() {
        Ok(lock) => {
            let sample = system::best_sample(
                runtime,
                SAMPLE_ATTEMPTS,
                POLL_BOUND_NS,
                runtime.generation(),
            );
            match sample {
                Ok((SampleResult::Accepted, sample)) => {
                    outcome.theta = sample.theta;
                    outcome.epsilon = sample.epsilon;
                    let frequency = current_frequency(runtime);
                    let correction = decoders::plan_correction(sample.theta, sample.epsilon, fast);
                    outcome.stepped = matches!(correction, sys::ClockAdjustment::Step { .. });
                    outcome.accepted = runtime.ops.adjust_clock(correction).is_ok()
                        && (!outcome.stepped || set_frequency(runtime, frequency).is_ok());
                    outcome.step_realtime_ns = clock_ns(sys::Clock::Realtime);
                    outcome.frequency = current_frequency(runtime);
                    if outcome.accepted {
                        daemon.last_accepted_ns = clock_ns(sys::Clock::Monotonic);
                    }
                }
                Err(SampleError::Generation(_)) => return,
                Ok((SampleResult::Wide, _)) | Err(SampleError::Unavailable(_)) => {}
            }
            Some(lock)
        }
        Err(_) => None,
    };
    outcome.synchronized = daemon.last_accepted_ns != 0
        && clock_ns(sys::Clock::Monotonic).wrapping_sub(daemon.last_accepted_ns)
            <= SYNCHRONIZED_AGE_NS;
    let _ = state_apply(|state| apply_poll(state, &outcome));
}

/// A kernel setting forced to a value while the owner lives. Dropping the owner restores the
/// setting's saved value, best effort, unless [`restore`](Self::restore) did.
struct ForcedSetting {
    /// The setting's file.
    path: &'static CStr,
    /// The value to restore, until it is restored.
    saved: Option<i64>,
}

impl ForcedSetting {
    /// Saves the setting at `path` and forces it to `value`, or returns `None` when either
    /// fails, which leaves the setting unchanged.
    fn force(path: &'static CStr, value: i64) -> Option<Self> {
        let saved = read_i64(path).ok()?;
        write_i64(path, value).ok()?;
        Some(Self {
            path,
            saved: Some(saved),
        })
    }

    /// Restores the saved value, and reports a failure.
    fn restore(mut self) -> Result<(), Errno> {
        self.saved
            .take()
            .map_or(Ok(()), |saved| write_i64(self.path, saved))
    }
}

impl Drop for ForcedSetting {
    /// Restores the saved value, best effort, unless `restore` did.
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            let _ = write_i64(self.path, saved);
        }
    }
}

/// Forces an RCU grace period with a tmpfs mount and unmount in a private mount namespace,
/// under normal grace periods, and returns whether every step succeeded.
fn grace_period_release() -> bool {
    let Some(normal) = ForcedSetting::force(kernel::RCU_NORMAL, RCU_NORMAL_FORCED) else {
        return false;
    };
    let released = sys::enter_private_mount_namespace().is_ok() && {
        let directory = sys::ScratchDirectory::create(RCU_SYNC_PATH, Mode::S_IRWXU);
        sys::TmpfsMount::mount(RCU_SYNC_SOURCE, directory.path(), RCU_SYNC_OPTIONS)
            .and_then(sys::TmpfsMount::unmount)
            .is_ok()
    };
    normal.restore().is_ok() && released
}

/// Runs the grace-period release in a child, and waits for it for at most `timeout_s`
/// seconds, after which the release counts as done, so that a stalled release cannot hold the
/// suppression back.
fn release_with_deadline(timeout_s: i64) -> bool {
    let deadline = clock_ns(sys::Clock::Monotonic).wrapping_add(timeout_s * NSEC_PER_SEC);
    let Ok(wait) = sys::ChildSignalWait::new() else {
        return false;
    };
    let process = match sys::fork() {
        Ok(ForkResult::Child) => {
            let _ = wait.restore();
            sys::exit_immediately(u8::from(!grace_period_release()));
        }
        Ok(ForkResult::Parent(process)) => process,
        Err(_) => return false,
    };
    loop {
        let remaining = deadline.wrapping_sub(clock_ns(sys::Clock::Monotonic));
        match sys::try_wait(process) {
            Ok(Some(ChildStatus::Exited(status))) => return status == 0,
            Ok(Some(_)) => return false,
            Ok(None) | Err(Errno::EINTR) => {}
            Err(_) => return false,
        }
        if remaining <= 0 {
            return true;
        }
        wait.wait(Duration::from_nanos(remaining as u64));
    }
}

/// The times that repair step 7 records. The wall time is `readiness_us`, the readiness
/// path's, plus the checks' since `start_ns`, when they were due. The CPU time is step 7's
/// alone: this process's since `cpu_base_ns`, plus the daemon's preparation, `prior_cpu_ns` up
/// to the fork, and the fork's own share, which the daemon writes to `fork_reader` once the
/// fork returns.
pub(crate) struct RestoreTiming {
    /// Monotonic start used for elapsed accounting.
    pub(crate) start_ns: i64,
    /// Readiness latency measured before deferred work.
    pub(crate) readiness_us: i64,
    /// CPU time accumulated before the fork.
    pub(crate) prior_cpu_ns: i64,
    /// Child process CPU-time baseline.
    cpu_base_ns: i64,
    /// Child-owned reader for the parent's at-fork CPU share.
    fork_reader: Option<FileDescriptor>,
}

/// Repair step 7's CPU time so far, in a process that `timing` describes. The read waits for
/// the daemon's fork share if the daemon has not yet written it.
pub(crate) fn deferred_restore_cpu_ns(timing: &RestoreTiming) -> i64 {
    let mut cpu = timing
        .prior_cpu_ns
        .wrapping_add(cpu_time_ns())
        .wrapping_sub(timing.cpu_base_ns);
    if let Some(reader) = &timing.fork_reader {
        let mut bytes = [0_u8; size_of::<i64>()];
        if reader.read_exact(&mut bytes).is_ok() {
            let fork_share = i64::from_ne_bytes(bytes);
            if fork_share > 0 {
                cpu = cpu.wrapping_add(fork_share);
            }
        }
    }
    cpu
}

/// Forks the repair step 7 worker at `SCHED_IDLE` for a daemon whose preparation started at
/// `cpu_start_ns`, its CPU clock, and completes `timing`'s CPU fields for the child. The parent
/// returns to normal priority as soon as the fork does, after `at_fork`, which stands for a
/// `pthread_atfork` parent handler. The fork costs the parent CPU time after the child's copy
/// of `timing` was taken, so the parent writes that share to a pipe that the child reads.
pub(crate) fn fork_deferred_restore(
    cpu_start_ns: i64,
    timing: &mut RestoreTiming,
    at_fork: impl FnOnce(),
) -> Result<ForkResult, Errno> {
    let pipe = sys::create_pipe().ok();
    timing.fork_reader = None;
    timing.cpu_base_ns = 0;
    let forked = {
        let _idle = sys::IdlePriority::enter();
        timing.prior_cpu_ns = cpu_time_ns().wrapping_sub(cpu_start_ns);
        let forked = sys::fork();
        if matches!(forked, Ok(ForkResult::Parent(_))) {
            at_fork();
        }
        forked
    };
    match forked {
        Ok(ForkResult::Child) => {
            timing.fork_reader = pipe.map(|(reader, _writer)| reader);
        }
        Ok(ForkResult::Parent(_)) => {
            if let Some((_reader, writer)) = pipe {
                let share = cpu_time_ns()
                    .wrapping_sub(cpu_start_ns)
                    .wrapping_sub(timing.prior_cpu_ns);
                let _ = writer.write_all(&share.to_ne_bytes());
            }
        }
        Err(_) => {}
    }
    forked
}

/// Repair step 7, fail-fast: the restore checks, the grace-period release, the saved watchdog
/// settings, the deferred C7, and the restore check's record. A child of the daemon runs it at
/// `SCHED_IDLE` from `DEFERRED_START_NS` after the acknowledgement; the daemon raises it to
/// normal priority `IDLE_BOUND_NS` later and releases the suppression itself at the stall
/// timeout. `restore-finish` runs it inline, with its own deadline (`with_deadline`), in
/// report-only mode when no daemon runs. Neither recorded time counts the grace period wait or
/// C7. Returns whether the grace period was released.
pub(crate) fn restore_work(
    runtime: &Runtime<'_>,
    new_cpus: &[bool; MAX_CPUS],
    new_count: usize,
    with_deadline: bool,
    timing: &RestoreTiming,
) -> bool {
    let mut checks = match Checks::load(Phase::Restore) {
        Ok(checks) => checks,
        Err(_) => {
            let mut checks = Checks::empty(Phase::Restore);
            checks.fail(runtime, "C6", "cannot read the CPU sets");
            checks
        }
    };
    if checks.online_count > 0 {
        checks::run_restore_checks(runtime, &mut checks, new_cpus, new_count);
    }
    let elapsed = clock_ns(sys::Clock::Monotonic).wrapping_sub(timing.start_ns);
    let cpu = deferred_restore_cpu_ns(timing);
    let timeout = read_i64(kernel::RCU_STALL_TIMEOUT).unwrap_or(DEFAULT_STALL_TIMEOUT_S);
    let released = if with_deadline {
        release_with_deadline(timeout)
    } else {
        grace_period_release()
    };
    if !released {
        runtime.fail_fatal(
            PowerOffStatus::Repair,
            b"G_REPAIR_SUPPRESSION",
            b"repair",
            Phase::Restore,
            b"the RCU grace-period release failed",
        );
    }
    if let Err(detail) = suppression_restore(true) {
        runtime.fail_fatal(
            PowerOffStatus::Repair,
            b"G_REPAIR_SUPPRESSION",
            b"repair",
            Phase::Restore,
            &detail,
        );
    }
    checks::check_timer_list(runtime, &mut checks, TIMER_LIST_WAIT_NS);
    if let Err(error) = checks::record_check(
        runtime,
        &checks,
        b"ok",
        timing.readiness_us.wrapping_add(elapsed / NSEC_PER_USEC),
        cpu / NSEC_PER_USEC,
        0,
        0,
    ) {
        checks.fail(
            runtime,
            "C10",
            format!("cannot record the restore check: {}", error.desc()),
        );
    }
    released
}

/// Parses and removes the private restore-work record.
fn parse_restore_state() -> Result<([bool; MAX_CPUS], usize, u64), Vec<u8>> {
    let state = state_load().map_err(|_| b"the time state is unreadable".to_vec())?;
    let text = match read_text(RESTORE_PATH, RESTORE_LIMIT) {
        Ok(text) => text,
        Err(_) => return Ok(([false; MAX_CPUS], 0, state.generation)),
    };
    let (generation, mut cpus, mut count) = decoders::parse_restore_record(&text)
        .ok_or_else(|| b"the restore record is malformed".to_vec())?;
    if generation != state.generation {
        cpus = [false; MAX_CPUS];
        count = 0;
    }
    Ok((cpus, count, state.generation))
}

/// Starts repair step 7 when it is due, `DEFERRED_START_NS` after the acknowledgement, in a
/// child at `SCHED_IDLE`, so the watcher and the discipline keep normal priority; the fork
/// itself also runs at `SCHED_IDLE`. The readiness path's wall time came with the signal.
fn start_restore(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    if daemon.worker_pid.is_some() {
        return;
    }
    let cpu_start = cpu_time_ns();
    let (new_cpus, new_count, generation) = match parse_restore_state() {
        Ok(value) => value,
        Err(detail) => {
            runtime.fail_fatal(
                PowerOffStatus::Repair,
                b"G_REPAIR_SUPPRESSION",
                b"repair",
                Phase::Restore,
                &detail,
            );
            return;
        }
    };
    runtime.set_generation(generation as u32);
    let timeout = read_i64(kernel::RCU_STALL_TIMEOUT).unwrap_or(DEFAULT_STALL_TIMEOUT_S);
    let mut timing = RestoreTiming {
        start_ns: daemon.restore_due_ns,
        readiness_us: daemon.readiness_us,
        prior_cpu_ns: 0,
        cpu_base_ns: 0,
        fork_reader: None,
    };
    let process = match fork_deferred_restore(cpu_start, &mut timing, || {}) {
        Ok(ForkResult::Child) => {
            let released = restore_work(runtime, &new_cpus, new_count, false, &timing);
            sys::exit_immediately(u8::from(!released));
        }
        Ok(ForkResult::Parent(process)) => process,
        Err(error) => {
            runtime.fail_fatal(
                PowerOffStatus::Repair,
                b"G_REPAIR_SUPPRESSION",
                b"repair",
                Phase::Restore,
                format!("cannot start the restore work: {}", error.desc()).as_bytes(),
            );
            return;
        }
    };
    daemon.worker_pid = Some(process);
    daemon.worker_promote_ns = daemon.restore_due_ns.wrapping_add(IDLE_BOUND_NS);
    daemon.worker_deadline_ns =
        clock_ns(sys::Clock::Monotonic).wrapping_add(timeout * NSEC_PER_SEC);
    daemon.suppression_released = false;
}

/// Restores saved stall settings after a worker deadline or completion.
fn release_suppression(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    daemon.suppression_released = true;
    if let Err(detail) = suppression_restore(true) {
        runtime.fail_fatal(
            PowerOffStatus::Repair,
            b"G_REPAIR_SUPPRESSION",
            b"repair",
            Phase::Restore,
            &detail,
        );
    }
}

/// Reaps every available child and handles the active restore worker.
fn reap_children(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    while let Some(child) = sys::reap_any_nonblocking() {
        if Some(child.process) != daemon.worker_pid {
            continue;
        }
        daemon.worker_pid = None;
        daemon.worker_promote_ns = 0;
        if !matches!(child.status, ChildStatus::Exited(_)) {
            release_suppression(runtime, daemon);
            let signal = match child.status {
                ChildStatus::Signaled(signal) => signal,
                _ => 0,
            };
            runtime.fail_fatal(
                PowerOffStatus::Repair,
                b"G_REPAIR_SUPPRESSION",
                b"repair",
                Phase::Restore,
                format!("the restore work ended with signal {signal}").as_bytes(),
            );
        }
        // Repair step 7 ends by restarting the discipline at the fast cadence.
        daemon.polls = 0;
        let _ = arm_timer(daemon);
    }
}

/// Consumes queued daemon signals and schedules restore work.
fn handle_signals(runtime: &Runtime<'_>, daemon: &mut Daemon) {
    while let Some(event) = daemon.signals.read() {
        match event.signal {
            Some(Signal::SIGTERM) => sys::exit_immediately(0),
            // `sigqueue` carries the readiness path's wall time. Step 7 is due
            // `DEFERRED_START_NS` later; the repair paired the clock with the packet's UTC now.
            Some(Signal::SIGUSR1) => {
                daemon.restore_pending = true;
                daemon.last_accepted_ns = clock_ns(sys::Clock::Monotonic);
                daemon.restore_due_ns = daemon.last_accepted_ns.wrapping_add(DEFERRED_START_NS);
                daemon.readiness_us = if event.queued {
                    i64::from(event.value)
                } else {
                    0
                };
            }
            _ => {}
        }
    }
    reap_children(runtime, daemon);
}

/// Creates all daemon-owned signal and timer resources.
fn setup_daemon(kmsg: FileDescriptor, last_accepted_ns: i64) -> Result<Daemon, Errno> {
    sys::ignore_signals(&IGNORED_SIGNALS);
    let signals = SignalDescriptor::new(&DAEMON_SIGNALS)?;
    let timer = Timer::new()?;
    let daemon = Daemon {
        kmsg,
        signals,
        timer,
        polls: 0,
        last_accepted_ns,
        worker_pid: None,
        worker_promote_ns: 0,
        worker_deadline_ns: 0,
        suppression_released: false,
        restore_pending: false,
        restore_due_ns: 0,
        readiness_us: 0,
    };
    write_text(kernel::OOM_SCORE_ADJUSTMENT, OOM_SCORE_ADJUSTMENT)?;
    arm_timer(&daemon)?;
    Ok(daemon)
}

/// Completes daemon setup and atomically publishes its process identifier.
pub(crate) fn become_daemon(kmsg: FileDescriptor, last_accepted_ns: i64) -> Result<Daemon, Errno> {
    let daemon = setup_daemon(kmsg, last_accepted_ns)?;
    system::replace_file(
        DAEMON_PID_PATH,
        DAEMON_PID_TEMP_PATH,
        &sys::current_process().line(),
    )?;
    Ok(daemon)
}

/// Converts an absolute monotonic nanosecond deadline, or zero for none, to a poll timeout
/// rounded up to whole milliseconds, or `None` to wait without one.
fn timeout_until(deadline_ns: i64) -> Option<Duration> {
    if deadline_ns == 0 {
        return None;
    }
    let remaining = deadline_ns.wrapping_sub(clock_ns(sys::Clock::Monotonic));
    let milliseconds = if remaining <= 0 {
        0
    } else {
        ((remaining + NSEC_PER_MSEC - 1) / NSEC_PER_MSEC).min(i64::from(i32::MAX))
    };
    Some(Duration::from_millis(milliseconds as u64))
}

/// Returns the earlier of two poll timeouts, where `None` waits without one.
fn earlier(first: Option<Duration>, second: Option<Duration>) -> Option<Duration> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, second) => first.or(second),
    }
}

/// Runs the kernel watcher, discipline poller, and restore-worker coordinator.
pub(crate) fn daemon_loop(runtime: &Runtime<'_>, mut daemon: Daemon) -> ! {
    loop {
        let waiting = daemon.worker_pid.is_some() && !daemon.suppression_released;
        let idle = daemon.worker_pid.is_some() && daemon.worker_promote_ns != 0;
        let due = daemon.restore_pending && daemon.worker_pid.is_none();
        let deadline =
            |active: bool, deadline_ns: i64| timeout_until(if active { deadline_ns } else { 0 });
        let timeout = earlier(
            earlier(
                deadline(idle, daemon.worker_promote_ns),
                deadline(waiting, daemon.worker_deadline_ns),
            ),
            deadline(due, daemon.restore_due_ns),
        );
        let events = match sys::poll(
            [
                Pollable::File(&daemon.kmsg),
                Pollable::Signals(&daemon.signals),
                Pollable::Timer(&daemon.timer),
            ],
            timeout,
        ) {
            Ok(events) => events,
            Err(Errno::EINTR) => Default::default(),
            Err(_) => {
                thread::sleep(POLL_RETRY);
                continue;
            }
        };
        let [log, signals, timer] = events;
        if log.readable || log.error {
            watch_kernel_log(runtime, &daemon);
        }
        if signals.readable {
            handle_signals(runtime, &mut daemon);
        }
        if timer.readable {
            daemon.timer.consume();
            discipline_poll(runtime, &mut daemon);
            let _ = arm_timer(&daemon);
        }
        let mut now = clock_ns(sys::Clock::Monotonic);
        // Step 7 starts `DEFERRED_START_NS` after the acknowledgement, and continues at normal
        // priority `IDLE_BOUND_NS` after it starts.
        if daemon.restore_pending && daemon.worker_pid.is_none() && now >= daemon.restore_due_ns {
            daemon.restore_pending = false;
            start_restore(runtime, &mut daemon);
            now = clock_ns(sys::Clock::Monotonic);
        }
        if let Some(process) = daemon.worker_pid
            && daemon.worker_promote_ns != 0
            && now >= daemon.worker_promote_ns
        {
            daemon.worker_promote_ns = 0;
            sys::set_scheduling_policy(Some(process), sys::SchedulingPolicy::Normal);
        }
        if daemon.worker_pid.is_some()
            && !daemon.suppression_released
            && now >= daemon.worker_deadline_ns
        {
            release_suppression(runtime, &mut daemon);
        }
    }
}

/// How repair steps 6 and 7 reached the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestoreSignal {
    /// The daemon received the queued signal.
    Queued,
    /// No daemon runs.
    NoDaemon,
    /// The daemon runs, but the signal could not be queued.
    Failed,
}

/// Repair steps 6 and 7 on the readiness path: the acknowledgement when the host gated its
/// input, then hand step 7 to the daemon. The `sigqueue` value is the readiness path's wall
/// time since `start_ns`, on the monotonic clock, in microseconds clamped to 32 bits, read
/// after the acknowledgement so that the read does not delay it.
pub(crate) fn signal_restore(
    runtime: &Runtime<'_>,
    daemon: Option<ProcessId>,
    acknowledge: bool,
    start_ns: i64,
) -> RestoreSignal {
    let Some(daemon) = daemon.filter(|&process| sys::process_exists(process)) else {
        return RestoreSignal::NoDaemon;
    };
    if acknowledge && let Ok(ports) = runtime.ports() {
        ports.outb(Port::Snapshot, SNAPSHOT_ACKNOWLEDGE);
    }
    let elapsed = clock_ns(sys::Clock::Monotonic).wrapping_sub(start_ns) / NSEC_PER_USEC;
    let value = elapsed.clamp(0, i64::from(u32::MAX)) as u32;
    if sys::queue_user_signal(daemon, value).is_ok() {
        RestoreSignal::Queued
    } else {
        RestoreSignal::Failed
    }
}

/// Constructs accounting for an inline report-only restore.
pub(crate) fn inline_restore_timing(start_ns: i64) -> RestoreTiming {
    let now = clock_ns(sys::Clock::Monotonic);
    RestoreTiming {
        start_ns: now,
        readiness_us: now.wrapping_sub(start_ns) / NSEC_PER_USEC,
        prior_cpu_ns: 0,
        cpu_base_ns: cpu_time_ns(),
        fork_reader: None,
    }
}

/// Returns the accounting that a test of `fork_deferred_restore` starts from, which the fork
/// completes.
pub(crate) fn empty_restore_timing() -> RestoreTiming {
    RestoreTiming {
        start_ns: 0,
        readiness_us: 0,
        prior_cpu_ns: 0,
        cpu_base_ns: 0,
        fork_reader: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_timing_constants_match_the_abi() {
        assert_eq!(POLL_BOUND_NS, 50_000);
        assert_eq!(SYNCHRONIZED_AGE_NS, 128_000_000_000);
        assert_eq!(
            (FAST_PERIOD.as_secs(), SLOW_PERIOD.as_secs(), FAST_POLLS),
            (16, 64, 4)
        );
    }

    #[test]
    fn poll_deadlines_round_up_to_milliseconds() {
        let millisecond = Some(Duration::from_millis(1));
        assert_eq!(
            earlier(None, Some(Duration::from_millis(4))),
            Some(Duration::from_millis(4))
        );
        assert_eq!(
            earlier(
                Some(Duration::from_millis(7)),
                Some(Duration::from_millis(4))
            ),
            Some(Duration::from_millis(4))
        );
        assert_eq!(earlier(None, None), None);
        assert_eq!(timeout_until(0), None);
        let now = clock_ns(sys::Clock::Monotonic);
        assert_eq!(timeout_until(now - 1), Some(Duration::ZERO));
        let soon = timeout_until(now + NSEC_PER_MSEC / 2);
        assert!(soon <= millisecond, "{soon:?}");
    }

    #[test]
    fn a_dropped_promoter_cancels_its_promotion() {
        let far = clock_ns(sys::Clock::Monotonic) + 10 * NSEC_PER_SEC;
        let started = std::time::Instant::now();
        drop(Promoter::start(far).unwrap());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
