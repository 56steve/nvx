//! Pairwise cross-CPU TSC warp and offset checks.

use std::io::{self, Write};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::thread;

use super::format::{f0, f1};
use super::support::{
    NANOSECONDS_PER_SECOND, TscRate, affinity_cpus, bad_option, check_cpus_allowed, cycles_to_ns,
    detect_tsc_rate, format_cpu_list, option_value, parse_cpu_list, parse_rate, parse_u64,
    round_ll,
};
use crate::guest::status::CommandError;
use crate::guest::sys::{self, Errno};

/// Milliseconds that each test runs per CPU pair without `--duration-ms`.
const DEFAULT_DURATION_MS: u64 = 100;
/// The durations in milliseconds that `--duration-ms` accepts.
const DURATIONS_MS: std::ops::RangeInclusive<u64> = 1..=600_000;
/// The verdict's bound on skew in nanoseconds without `--bound-ns`.
const DEFAULT_BOUND_NS: u64 = 1000;
/// Milliseconds in a second, which scale the duration to cycles.
const MILLISECONDS_PER_SECOND: f64 = 1000.0;
/// Spinlock iterations between the leader's checks of its deadline.
const WARP_CHECK_MASK: u64 = 7;
/// Spinlock iterations between the leader's checks of its stall deadline.
const WARP_STALL_CHECK_MASK: u64 = 4095;
/// Ping-pong rounds between the leader's checks of its deadline.
const PING_CHECK_MASK: u64 = 63;

/// Ping-pong sequence value that asks the follower to stop.
const PP_STOP: u64 = u64::MAX;
/// Maximum stored offset samples per CPU pair.
const PP_MAX_SAMPLES: usize = 1 << 18;
/// Extra wall-clock allowance before a worker is considered stalled.
const STALL_SECONDS: f64 = 2.0;
/// Spin-iteration mask controlling periodic stall checks.
const SPIN_CHECK_MASK: u64 = 0x0f_ffff;

#[repr(align(64))]
/// Cache-line-aligned owner preventing false sharing of hot atomics.
struct Aligned<T>(T);

/// Shared state for the spinlock lower-bound warp test.
struct WarpState {
    /// Test-and-set spinlock on its own cache line.
    lock: Aligned<AtomicI32>,
    /// TSC read by the previous lock owner.
    last_tsc: AtomicU64,
    /// Largest observed backward delta.
    max_warp: AtomicU64,
    /// Number of backward TSC observations.
    warps: AtomicU64,
    /// Two-thread startup barrier on its own cache line.
    ready: Aligned<AtomicI32>,
    /// Startup failure flag.
    abort: AtomicI32,
    /// Leader-controlled completion flag.
    stop: AtomicI32,
}

impl WarpState {
    /// Creates cleared state for one CPU pair.
    fn new() -> Self {
        Self {
            lock: Aligned(AtomicI32::new(0)),
            last_tsc: AtomicU64::new(0),
            max_warp: AtomicU64::new(0),
            warps: AtomicU64::new(0),
            ready: Aligned(AtomicI32::new(0)),
            abort: AtomicI32::new(0),
            stop: AtomicI32::new(0),
        }
    }
}

/// Shared state for the ping-pong offset estimate.
struct PingState {
    /// Release/acquire request and response sequence.
    sequence: Aligned<AtomicU64>,
    /// Follower timestamp paired with the current sequence.
    t2: AtomicU64,
    /// Two-thread startup barrier on its own cache line.
    ready: Aligned<AtomicI32>,
    /// Startup failure flag.
    abort: AtomicI32,
}

impl PingState {
    /// Creates cleared ping-pong state for one CPU pair.
    fn new() -> Self {
        Self {
            sequence: Aligned(AtomicU64::new(0)),
            t2: AtomicU64::new(0),
            ready: Aligned(AtomicI32::new(0)),
            abort: AtomicI32::new(0),
        }
    }
}

#[derive(Default)]
/// Complete warp and offset result for one unordered CPU pair.
struct PairResult {
    /// Leader CPU.
    cpu_a: usize,
    /// Follower CPU.
    cpu_b: usize,
    /// Combined spinlock-test iterations.
    warp_iterations: u64,
    /// Number of backward TSC observations.
    warps: u64,
    /// Largest backward delta in cycles.
    max_warp_cycles: u64,
    /// Completed ping-pong rounds.
    rounds: u64,
    /// Smallest round trip in cycles.
    min_rtt_cycles: u64,
    /// Twice the offset from the smallest round trip.
    min_rtt_offset_x2: i64,
    /// Greatest lower offset bound.
    lo_cycles: i64,
    /// Least upper offset bound.
    hi_cycles: i64,
    /// Nearest-median sampled offset in cycles.
    median_offset_cycles: f64,
    /// Selected offset estimate in cycles.
    offset_cycles: f64,
    /// Selected half-width uncertainty in cycles.
    uncertainty_cycles: f64,
    /// Whether all ping-pong interval bounds intersect.
    consistent: bool,
    /// Whether either worker exceeded its stall bound.
    stalled: bool,
}

/// Synchronizes two pinned workers and propagates startup failure.
fn barrier(ready: &AtomicI32, abort: &AtomicI32, failed: bool) -> bool {
    if failed {
        abort.store(1, Ordering::SeqCst);
    }
    ready.fetch_add(1, Ordering::SeqCst);
    while ready.load(Ordering::SeqCst) < 2 {
        sys::x86::pause();
    }
    abort.load(Ordering::SeqCst) == 0
}

/// Reads coarse monotonic time for infrequent stall detection.
fn coarse_ns() -> u64 {
    sys::clock_time(sys::Clock::MonotonicCoarse)
        .unwrap_or_default()
        .wrapping_nanoseconds() as u64
}

/// Returns the `errno` of a thread that could not start, or `EAGAIN`, as `pthread_create`
/// returns for a lack of resources.
fn spawn_errno(error: &io::Error) -> Errno {
    error.raw_os_error().map_or(Errno::EAGAIN, Errno::from_raw)
}

/// Runs one side of the spinlock-based TSC monotonicity test.
fn warp_worker(
    state: &WarpState,
    cpu: usize,
    leader: bool,
    budget_cycles: u64,
    rate: TscRate,
) -> Result<u64, Errno> {
    let pinned = sys::pin_current_thread(cpu);
    if !barrier(&state.ready.0, &state.abort, pinned.is_err()) {
        return pinned.map(|()| 0);
    }
    let end = sys::x86::rdtsc_lfence().wrapping_add(budget_cycles);
    let deadline = coarse_ns().wrapping_add(
        (cycles_to_ns(budget_cycles as f64, rate) + STALL_SECONDS * NANOSECONDS_PER_SECOND) as u64,
    );
    let mut iteration = 0_u64;
    loop {
        while state.lock.0.swap(1, Ordering::Acquire) != 0 {
            while state.lock.0.load(Ordering::Relaxed) != 0 {
                sys::x86::pause();
            }
        }
        let previous = state.last_tsc.load(Ordering::Relaxed);
        let now = sys::x86::rdtsc_lfence();
        state.last_tsc.store(now, Ordering::Relaxed);
        if previous > now {
            state.max_warp.fetch_max(previous - now, Ordering::Relaxed);
            state.warps.fetch_add(1, Ordering::Relaxed);
        }
        state.lock.0.store(0, Ordering::Release);
        if iteration & WARP_CHECK_MASK == 0 {
            if leader
                && (sys::x86::rdtsc() > end
                    || (iteration & WARP_STALL_CHECK_MASK == 0 && coarse_ns() > deadline))
            {
                state.stop.store(1, Ordering::Relaxed);
            }
            if state.stop.load(Ordering::Relaxed) != 0 {
                break;
            }
        }
        iteration = iteration.wrapping_add(1);
    }
    Ok(iteration.wrapping_add(1))
}

/// Leader-only aggregate from the ping-pong test.
struct LeaderResult {
    /// Completed request/response rounds.
    rounds: u64,
    /// Smallest round trip in cycles.
    min_rtt: u64,
    /// Twice the offset associated with the smallest round trip.
    min_offset_x2: i64,
    /// Greatest lower offset bound.
    lo: i64,
    /// Least upper offset bound.
    hi: i64,
    /// Number of offsets retained for median selection.
    stored: usize,
    /// Whether the follower stalled.
    stalled: bool,
}

/// Runs the ping-pong leader and records offset intervals.
fn ping_leader(
    state: &PingState,
    cpu: usize,
    budget_cycles: u64,
    stall_cycles: u64,
    offsets: &mut [i64],
) -> Result<LeaderResult, Errno> {
    let pinned = sys::pin_current_thread(cpu);
    if !barrier(&state.ready.0, &state.abort, pinned.is_err()) {
        return Err(pinned.err().unwrap_or(Errno::EINVAL));
    }
    let end = sys::x86::rdtsc_lfence().wrapping_add(budget_cycles);
    let mut result = LeaderResult {
        rounds: 0,
        min_rtt: u64::MAX,
        min_offset_x2: 0,
        lo: i64::MIN,
        hi: i64::MAX,
        stored: 0,
        stalled: false,
    };
    let mut round = 1_u64;
    loop {
        let mut spins = 0_u64;
        let t1 = sys::x86::rdtsc_fenced();
        state
            .sequence
            .0
            .store(round.wrapping_mul(2).wrapping_sub(1), Ordering::Release);
        while state.sequence.0.load(Ordering::Acquire) != round.wrapping_mul(2) {
            spins = spins.wrapping_add(1);
            if spins & SPIN_CHECK_MASK == 0 && sys::x86::rdtsc().wrapping_sub(t1) > stall_cycles {
                result.stalled = true;
                state.sequence.0.store(PP_STOP, Ordering::Release);
                return Ok(result);
            }
            sys::x86::pause();
        }
        let t3 = sys::x86::rdtsc_lfence();
        let t2 = state.t2.load(Ordering::Relaxed);
        let rtt = t3.wrapping_sub(t1);
        let lo = t2.wrapping_sub(t3) as i64;
        let hi = t2.wrapping_sub(t1) as i64;
        if rtt < result.min_rtt {
            result.min_rtt = rtt;
            result.min_offset_x2 = lo.wrapping_add(hi);
        }
        result.lo = result.lo.max(lo);
        result.hi = result.hi.min(hi);
        if result.stored < offsets.len() {
            offsets[result.stored] = lo.wrapping_add(hi);
            result.stored += 1;
        }
        result.rounds = round;
        if round & PING_CHECK_MASK == 0 && sys::x86::rdtsc() > end {
            state.sequence.0.store(PP_STOP, Ordering::Release);
            return Ok(result);
        }
        round = round.wrapping_add(1);
    }
}

/// Runs the ping-pong follower on its pinned CPU.
fn ping_follower(state: &PingState, cpu: usize, stall_cycles: u64) -> Result<(), Errno> {
    let pinned = sys::pin_current_thread(cpu);
    if !barrier(&state.ready.0, &state.abort, pinned.is_err()) {
        return pinned;
    }
    let mut round = 1_u64;
    loop {
        let expected = round.wrapping_mul(2).wrapping_sub(1);
        let mut spins = 0_u64;
        let mut wait_start = 0_u64;
        loop {
            let value = state.sequence.0.load(Ordering::Acquire);
            if value == expected {
                break;
            }
            if value == PP_STOP {
                return Ok(());
            }
            spins = spins.wrapping_add(1);
            if spins & SPIN_CHECK_MASK == 0 {
                let now = sys::x86::rdtsc();
                if wait_start == 0 {
                    wait_start = now;
                } else if now.wrapping_sub(wait_start) > stall_cycles {
                    return Ok(());
                }
            }
            sys::x86::pause();
        }
        state.t2.store(sys::x86::rdtsc_fenced(), Ordering::Relaxed);
        state
            .sequence
            .0
            .store(round.wrapping_mul(2), Ordering::Release);
        round = round.wrapping_add(1);
    }
}

/// Runs both pairwise tests with scoped, joined worker threads.
fn run_pair(
    cpu_a: usize,
    cpu_b: usize,
    budget_cycles: u64,
    stall_cycles: u64,
    offsets: &mut [i64],
    rate: TscRate,
) -> Result<PairResult, Errno> {
    let warp = WarpState::new();
    let (first, second) = thread::scope(|scope| {
        let first = thread::Builder::new()
            .spawn_scoped(scope, || {
                warp_worker(&warp, cpu_a, true, budget_cycles, rate)
            })
            .map_err(|error| spawn_errno(&error))?;
        let second = match thread::Builder::new().spawn_scoped(scope, || {
            warp_worker(&warp, cpu_b, false, budget_cycles, rate)
        }) {
            Ok(thread) => thread,
            Err(error) => {
                warp.abort.store(1, Ordering::SeqCst);
                warp.ready.0.fetch_add(1, Ordering::SeqCst);
                let _ = first.join();
                return Err(spawn_errno(&error));
            }
        };
        let first = first.join().map_err(|_| Errno::EIO)??;
        let second = second.join().map_err(|_| Errno::EIO)??;
        Ok((first, second))
    })?;
    let ping = PingState::new();
    let leader = thread::scope(|scope| {
        let leader = thread::Builder::new()
            .spawn_scoped(scope, || {
                ping_leader(&ping, cpu_a, budget_cycles, stall_cycles, offsets)
            })
            .map_err(|error| spawn_errno(&error))?;
        let follower = match thread::Builder::new()
            .spawn_scoped(scope, || ping_follower(&ping, cpu_b, stall_cycles))
        {
            Ok(thread) => thread,
            Err(error) => {
                ping.abort.store(1, Ordering::SeqCst);
                ping.ready.0.fetch_add(1, Ordering::SeqCst);
                let _ = leader.join();
                return Err(spawn_errno(&error));
            }
        };
        let leader = leader.join().map_err(|_| Errno::EIO)??;
        follower.join().map_err(|_| Errno::EIO)??;
        Ok(leader)
    })?;

    let mut result = PairResult {
        cpu_a,
        cpu_b,
        warp_iterations: first.wrapping_add(second),
        warps: warp.warps.load(Ordering::Relaxed),
        max_warp_cycles: warp.max_warp.load(Ordering::Relaxed),
        rounds: leader.rounds,
        min_rtt_cycles: leader.min_rtt,
        min_rtt_offset_x2: leader.min_offset_x2,
        lo_cycles: leader.lo,
        hi_cycles: leader.hi,
        stalled: leader.stalled,
        ..PairResult::default()
    };
    if leader.stored == 0 {
        result.stalled = true;
        return Ok(result);
    }
    offsets[..leader.stored].sort_unstable();
    result.median_offset_cycles = offsets[(leader.stored - 1) / 2] as f64 / 2.0;
    result.consistent = result.lo_cycles <= result.hi_cycles;
    if result.consistent {
        result.offset_cycles = (result.lo_cycles as f64 + result.hi_cycles as f64) / 2.0;
        result.uncertainty_cycles = (result.hi_cycles as f64 - result.lo_cycles as f64) / 2.0;
    } else {
        result.offset_cycles = result.min_rtt_offset_x2 as f64 / 2.0;
        result.uncertainty_cycles = result.min_rtt_cycles as f64 / 2.0;
    }
    Ok(result)
}

/// Parses options and runs every unordered CPU pair.
pub(super) fn run(
    arguments: &[&[u8]],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut duration_ms = DEFAULT_DURATION_MS;
    let mut bound_ns = DEFAULT_BOUND_NS;
    let mut override_hz = 0.0;
    let mut cpus = None;
    let mut index = 0;
    while index < arguments.len() {
        if let Some(value) = option_value(arguments, &mut index, b"--duration-ms") {
            duration_ms = parse_u64(value)
                .filter(|value| DURATIONS_MS.contains(value))
                .ok_or_else(|| bad_option(b"warp", arguments[index], err))?;
        } else if let Some(value) = option_value(arguments, &mut index, b"--cpus") {
            let parsed = parse_cpu_list(value).filter(|cpus| !cpus.is_empty());
            cpus = Some(parsed.ok_or_else(|| bad_option(b"warp", arguments[index], err))?);
        } else if let Some(value) = option_value(arguments, &mut index, b"--bound-ns") {
            bound_ns =
                parse_u64(value).ok_or_else(|| bad_option(b"warp", arguments[index], err))?;
        } else if let Some(value) = option_value(arguments, &mut index, b"--tsc-hz") {
            override_hz =
                parse_rate(value).ok_or_else(|| bad_option(b"warp", arguments[index], err))?;
        } else {
            return Err(bad_option(b"warp", arguments[index], err));
        }
        index += 1;
    }
    // Linux never reports an empty affinity mask, which would be `EINVAL` to set.
    let cpus = cpus
        .map_or_else(affinity_cpus, Ok)
        .and_then(|cpus| {
            if cpus.is_empty() {
                Err(Errno::EINVAL)
            } else {
                Ok(cpus)
            }
        })
        .map_err(|error| {
            super::report(
                err,
                &[
                    b"nvx-time-probe warp: sched_getaffinity: ",
                    error.desc().as_bytes(),
                    b"\n",
                ],
            );
            CommandError::Setup
        })?;
    if !check_cpus_allowed(b"warp", &cpus, err) {
        return Err(CommandError::Usage);
    }
    let rate = detect_tsc_rate(override_hz);
    if rate.hz <= 0.0 {
        super::report(
            err,
            &[b"nvx-time-probe warp: unknown TSC rate; pass --tsc-hz\n"],
        );
        return Err(CommandError::Setup);
    }
    let budget_cycles = (duration_ms as f64 * rate.hz / MILLISECONDS_PER_SECOND) as u64;
    let stall_cycles = (STALL_SECONDS * rate.hz) as u64 + budget_cycles;
    let mut offsets = Vec::new();
    if offsets.try_reserve_exact(PP_MAX_SAMPLES).is_err() {
        super::report(err, &[b"nvx-time-probe warp: out of memory\n"]);
        return Err(CommandError::Setup);
    }
    offsets.resize(PP_MAX_SAMPLES, 0_i64);
    let cpu_text = format_cpu_list(&cpus);
    let expected_pairs = cpus.len() * cpus.len().saturating_sub(1) / 2;
    let _ = writeln!(
        out,
        "nvx-time-probe {} warp: cpus={cpu_text} pairs={expected_pairs} duration_ms={duration_ms} \
         (per test per pair) tsc_hz={} tsc_hz_source={} bound_ns={bound_ns}",
        super::VERSION,
        f0(rate.hz),
        rate.source
    );
    let _ = out.flush();

    let mut pairs = 0;
    let mut inconsistent = 0;
    let mut stalled = 0;
    let mut total_warps = 0_u64;
    let mut max_backward = 0_u64;
    let mut max_abs_offset = 0.0_f64;
    let mut max_uncertainty = 0.0_f64;
    let mut max_skew_bound = 0.0_f64;
    for (first, &cpu_a) in cpus.iter().enumerate() {
        for &cpu_b in &cpus[first + 1..] {
            let result = match run_pair(
                cpu_a,
                cpu_b,
                budget_cycles,
                stall_cycles,
                &mut offsets,
                rate,
            ) {
                Ok(result) => result,
                Err(error) => {
                    super::report(
                        err,
                        &[
                            format!("nvx-time-probe warp: pair {cpu_a}-{cpu_b}: ").as_bytes(),
                            error.desc().as_bytes(),
                            b"\n",
                        ],
                    );
                    return Err(CommandError::Setup);
                }
            };
            pairs += 1;
            total_warps = total_warps.wrapping_add(result.warps);
            max_backward = max_backward.max(result.max_warp_cycles);
            if result.stalled {
                stalled += 1;
                let _ = writeln!(
                    out,
                    "pair {}-{}: warp_iterations={} warps={} max_backward_cycles={} \
                     ping_pong=stalled",
                    result.cpu_a,
                    result.cpu_b,
                    result.warp_iterations,
                    result.warps,
                    result.max_warp_cycles
                );
                continue;
            }
            if !result.consistent {
                inconsistent += 1;
            }
            let skew_bound = if result.consistent {
                (result.lo_cycles as f64)
                    .abs()
                    .max((result.hi_cycles as f64).abs())
            } else {
                result.offset_cycles.abs() + result.uncertainty_cycles
            };
            max_abs_offset = max_abs_offset.max(result.offset_cycles.abs());
            max_uncertainty = max_uncertainty.max(result.uncertainty_cycles);
            max_skew_bound = max_skew_bound.max(skew_bound);
            let _ = writeln!(
                out,
                "pair {}-{}: warp_iterations={} warps={} max_backward_cycles={} \
                 max_backward_ns={} rounds={} min_rtt_ns={} offset_cycles={} offset_ns={} \
                 uncertainty_ns={} bounds_ns=[{},{}] median_offset_ns={} consistent={}",
                result.cpu_a,
                result.cpu_b,
                result.warp_iterations,
                result.warps,
                result.max_warp_cycles,
                round_ll(cycles_to_ns(result.max_warp_cycles as f64, rate)),
                result.rounds,
                round_ll(cycles_to_ns(result.min_rtt_cycles as f64, rate)),
                f1(result.offset_cycles),
                round_ll(cycles_to_ns(result.offset_cycles, rate)),
                round_ll(cycles_to_ns(result.uncertainty_cycles, rate)),
                round_ll(cycles_to_ns(result.lo_cycles as f64, rate)),
                round_ll(cycles_to_ns(result.hi_cycles as f64, rate)),
                round_ll(cycles_to_ns(result.median_offset_cycles, rate)),
                u8::from(result.consistent)
            );
            let _ = out.flush();
        }
    }
    if pairs == 0 {
        let _ = writeln!(out, "single CPU: no pairs to compare");
    }
    let pass = stalled == 0
        && cycles_to_ns(max_backward as f64, rate) <= bound_ns as f64
        && cycles_to_ns(max_abs_offset, rate) <= bound_ns as f64;
    let conclusive =
        pairs == 0 || (stalled == 0 && cycles_to_ns(max_uncertainty, rate) < bound_ns as f64);
    if !conclusive {
        let _ = writeln!(
            out,
            "warning: offset uncertainty (+/-{} ns) is not below the bound ({bound_ns} ns); \
             the verdict is not conclusive (trapped RDTSC or a descheduled vCPU?)",
            round_ll(cycles_to_ns(max_uncertainty, rate))
        );
    }
    let _ = writeln!(
        out,
        "NVX-TIME-PROBE warp max_backward_cycles={max_backward} max_backward_ns={} \
         max_abs_offset_ns={} pairs={pairs} verdict={} bound_ns={bound_ns}",
        round_ll(cycles_to_ns(max_backward as f64, rate)),
        round_ll(cycles_to_ns(max_abs_offset, rate)),
        if pass { "PASS" } else { "FAIL" }
    );
    let _ = writeln!(
        out,
        "NVX-TIME-PROBE warp-detail max_abs_offset_cycles={} max_uncertainty_ns={} \
         max_skew_bound_ns={} total_warps={total_warps} inconsistent_pairs={inconsistent} \
         stalled_pairs={stalled} cpus={cpu_text} duration_ms={duration_ms} tsc_hz={} \
         tsc_hz_source={} conclusive={}",
        f1(max_abs_offset),
        round_ll(cycles_to_ns(max_uncertainty, rate)),
        round_ll(cycles_to_ns(max_skew_bound, rate)),
        f0(rate.hz),
        rate.source,
        u8::from(conclusive)
    );
    if pass {
        Ok(())
    } else {
        Err(CommandError::Operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cacheline_wrappers_are_really_aligned() {
        assert_eq!(std::mem::align_of::<Aligned<AtomicU64>>(), 64);
        assert!(std::mem::size_of::<Aligned<AtomicU64>>() >= 64);
    }
}
