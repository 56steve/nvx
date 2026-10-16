//! The single-CPU instruction and clock latency probe.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use super::format::{f0, f1};
use super::support::{
    TscRate, bad_option, check_cpus_allowed, current_clocksource, cycles_to_ns, detect_tsc_rate,
    option_value, parse_rate, parse_u64,
};
use crate::guest::status::CommandError;
use crate::guest::sys::{self, x86::cpuid};

/// Timed batches that the probe measures without `--iterations`.
const DEFAULT_ITERATIONS: u64 = 2000;
/// Operations per timed batch without `--batch`.
const DEFAULT_BATCH: u64 = 50;
/// The batch counts that `--iterations` accepts.
const ITERATIONS: std::ops::RangeInclusive<u64> = 1..=10_000_000;
/// The operation counts that `--batch` accepts.
const BATCH: std::ops::RangeInclusive<u64> = 1..=1_000_000;

/// Optimizer-visible accumulation of every measured operation's result.
static SINK: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
/// Operation measured by one latency row.
enum Method {
    /// Unfenced `RDTSC`.
    Rdtsc,
    /// Serializing `RDTSCP`.
    Rdtscp,
    /// `LFENCE` followed by `RDTSC`.
    LfenceRdtsc,
    /// vDSO-aware monotonic clock read.
    ClockMonotonic,
    /// vDSO-aware realtime clock read.
    ClockRealtime,
    /// Direct monotonic `clock_gettime` syscall.
    SyscallMonotonic,
    /// Direct `getpid` syscall.
    SyscallGetpid,
    /// Scheduler yield syscall.
    SchedYield,
}

/// Stable measurement order and output names.
const METHODS: &[(Method, &str)] = &[
    (Method::Rdtsc, "rdtsc"),
    (Method::Rdtscp, "rdtscp"),
    (Method::LfenceRdtsc, "lfence_rdtsc"),
    (Method::ClockMonotonic, "clock_gettime_monotonic"),
    (Method::ClockRealtime, "clock_gettime_realtime"),
    (Method::SyscallMonotonic, "syscall_clock_gettime_monotonic"),
    (Method::SyscallGetpid, "syscall_getpid"),
    (Method::SchedYield, "sched_yield"),
];

#[derive(Clone, Copy, Default)]
/// Summary statistics for one measured operation.
struct ResultRow {
    /// Nearest-rank median latency in nanoseconds.
    p50: f64,
    /// Nearest-rank 99th-percentile latency in nanoseconds.
    p99: f64,
    /// Minimum latency in nanoseconds.
    min: f64,
    /// Arithmetic mean latency in nanoseconds.
    mean: f64,
    /// Number of timed batches.
    samples: usize,
}

/// Returns whether CPUID advertises RDTSCP.
fn cpu_has_rdtscp() -> bool {
    let maximum = sys::x86::cpuid(cpuid::LEAF_EXTENDED_MAXIMUM, 0)[0];
    maximum >= cpuid::LEAF_EXTENDED_FEATURES
        && cpuid::bit(
            sys::x86::cpuid(cpuid::LEAF_EXTENDED_FEATURES, 0)[3],
            cpuid::EXTENDED_FEATURES_EDX_RDTSCP,
        )
}

/// Executes `batch` copies of one operation and returns an optimizer sink.
fn run_batch(method: Method, batch: u32) -> u64 {
    let mut sink = 0_u64;
    for _ in 0..batch {
        let value = match method {
            Method::Rdtsc => sys::x86::rdtsc(),
            Method::Rdtscp => sys::x86::rdtscp(),
            Method::LfenceRdtsc => sys::x86::rdtsc_lfence(),
            Method::ClockMonotonic => {
                sys::clock_time(sys::Clock::Monotonic)
                    .unwrap_or_default()
                    .nanoseconds as u64
            }
            Method::ClockRealtime => {
                sys::clock_time(sys::Clock::Realtime)
                    .unwrap_or_default()
                    .nanoseconds as u64
            }
            Method::SyscallMonotonic => sys::syscall_monotonic_nanoseconds(),
            Method::SyscallGetpid => sys::syscall_process_id(),
            Method::SchedYield => u64::from(sys::yield_now()),
        };
        sink = sink.wrapping_add(value);
    }
    sink
}

/// Selects a nearest-rank percentile from sorted samples.
fn nearest_rank(sorted: &[f64], percentile: f64) -> f64 {
    let exact = percentile * sorted.len() as f64;
    let mut rank = exact as usize;
    if (rank as f64) < exact {
        rank += 1;
    }
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Measures the median fenced-TSC bracket overhead.
fn bracket_overhead(iterations: usize) -> Result<f64, ()> {
    let mut samples = Vec::new();
    samples.try_reserve_exact(iterations).map_err(|_| ())?;
    for _ in 0..iterations {
        let t0 = sys::x86::rdtsc_fenced();
        let t1 = sys::x86::rdtsc_fenced();
        samples.push(t1.wrapping_sub(t0) as f64);
    }
    samples.sort_unstable_by(f64::total_cmp);
    Ok(nearest_rank(&samples, 0.50))
}

/// Measures one method and computes its summary row.
fn measure(
    method: Method,
    iterations: usize,
    batch: u32,
    overhead: f64,
    rate: TscRate,
) -> Result<ResultRow, ()> {
    let mut per_op = Vec::new();
    per_op.try_reserve_exact(iterations).map_err(|_| ())?;
    SINK.fetch_add(run_batch(method, batch), Ordering::Relaxed);
    let mut sum = 0.0;
    for _ in 0..iterations {
        let t0 = sys::x86::rdtsc_fenced();
        let sink = run_batch(method, batch);
        let t1 = sys::x86::rdtsc_fenced();
        let cycles = (t1.wrapping_sub(t0) as f64 - overhead).max(0.0);
        SINK.fetch_add(sink, Ordering::Relaxed);
        let value = cycles_to_ns(cycles, rate) / f64::from(batch);
        per_op.push(value);
        sum += value;
    }
    per_op.sort_unstable_by(f64::total_cmp);
    Ok(ResultRow {
        p50: nearest_rank(&per_op, 0.50),
        p99: nearest_rank(&per_op, 0.99),
        min: per_op[0],
        mean: sum / iterations as f64,
        samples: iterations,
    })
}

/// Parses options and prints every latency measurement.
pub(super) fn run(
    arguments: &[&[u8]],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut iterations = DEFAULT_ITERATIONS;
    let mut batch = DEFAULT_BATCH;
    let mut cpu = None;
    let mut override_hz = 0.0;
    let mut index = 0;
    while index < arguments.len() {
        if let Some(value) = option_value(arguments, &mut index, b"--iterations") {
            iterations = parse_u64(value)
                .filter(|value| ITERATIONS.contains(value))
                .ok_or_else(|| bad_option(b"latency", arguments[index], err))?;
        } else if let Some(value) = option_value(arguments, &mut index, b"--batch") {
            batch = parse_u64(value)
                .filter(|value| BATCH.contains(value))
                .ok_or_else(|| bad_option(b"latency", arguments[index], err))?;
        } else if let Some(value) = option_value(arguments, &mut index, b"--cpu") {
            let parsed = parse_u64(value).filter(|value| *value < super::MAX_CPUS as u64);
            cpu =
                Some(parsed.ok_or_else(|| bad_option(b"latency", arguments[index], err))? as usize);
        } else if let Some(value) = option_value(arguments, &mut index, b"--tsc-hz") {
            override_hz =
                parse_rate(value).ok_or_else(|| bad_option(b"latency", arguments[index], err))?;
        } else {
            return Err(bad_option(b"latency", arguments[index], err));
        }
        index += 1;
    }

    let cpu = cpu.map_or_else(sys::current_cpu, Ok);
    if let Ok(cpu) = cpu
        && !check_cpus_allowed(b"latency", &[cpu], err)
    {
        return Err(CommandError::Usage);
    }
    // The C probe printed -1 for a CPU that `sched_getcpu` could not report.
    let cpu_label = cpu.map_or(-1, |cpu| cpu as i64);
    if let Err(error) = cpu.and_then(sys::pin_current_thread) {
        super::report(
            err,
            &[
                format!("nvx-time-probe latency: cannot pin to cpu {cpu_label}: ").as_bytes(),
                error.desc().as_bytes(),
                b"\n",
            ],
        );
        return Err(CommandError::Setup);
    }
    let rate = detect_tsc_rate(override_hz);
    if rate.hz <= 0.0 {
        super::report(
            err,
            &[b"nvx-time-probe latency: unknown TSC rate; pass --tsc-hz\n"],
        );
        return Err(CommandError::Setup);
    }
    let clocksource = current_clocksource();
    let overhead = bracket_overhead(iterations as usize).map_err(|()| {
        super::report(err, &[b"nvx-time-probe latency: out of memory\n"]);
        CommandError::Setup
    })?;
    let vdso = sys::kernel::has_vdso();
    let _ = writeln!(
        out,
        "nvx-time-probe {} latency: cpu={cpu_label} iterations={iterations} batch={batch} \
         tsc_hz={} tsc_hz_source={} clocksource={} vdso={} bracket_overhead_ns={}",
        super::VERSION,
        f0(rate.hz),
        rate.source,
        String::from_utf8_lossy(&clocksource),
        u8::from(vdso),
        f1(cycles_to_ns(overhead, rate))
    );
    let _ = writeln!(
        out,
        "{:<32} {:>10} {:>10} {:>10} {:>10}",
        "method", "p50_ns", "p99_ns", "min_ns", "mean_ns"
    );
    let mut results = [ResultRow::default(); METHODS.len()];
    let rdtscp = cpu_has_rdtscp();
    for (index, &(method, name)) in METHODS.iter().enumerate() {
        if method == Method::Rdtscp && !rdtscp {
            let _ = writeln!(
                out,
                "{name:<32} unsupported (CPUID.80000001H:EDX[27] clear)"
            );
            continue;
        }
        let row =
            measure(method, iterations as usize, batch as u32, overhead, rate).map_err(|()| {
                super::report(err, &[b"nvx-time-probe latency: out of memory\n"]);
                CommandError::Setup
            })?;
        results[index] = row;
        let _ = writeln!(
            out,
            "{name:<32} {:>10} {:>10} {:>10} {:>10}",
            f1(row.p50),
            f1(row.p99),
            f1(row.min),
            f1(row.mean)
        );
        let _ = out.flush();
    }
    for (index, &(_, name)) in METHODS.iter().enumerate() {
        let row = results[index];
        if row.samples == 0 {
            let _ = writeln!(out, "NVX-TIME-PROBE latency method={name} unsupported=1");
            continue;
        }
        let _ = writeln!(
            out,
            "NVX-TIME-PROBE latency method={name} p50_ns={} p99_ns={} min_ns={} \
             mean_ns={} samples={} batch={batch} cpu={cpu_label} clocksource={}",
            f1(row.p50),
            f1(row.p99),
            f1(row.min),
            f1(row.mean),
            row.samples,
            String::from_utf8_lossy(&clocksource)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_uses_the_harness_definition() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(nearest_rank(&values, 0.50), 2.0);
        assert_eq!(nearest_rank(&values, 0.99), 4.0);
    }
}
