//! The kernel's clocks: reading them and sleeping on them.

use nix::sys::time::TimeSpec;
use nix::time::{ClockId, ClockNanosleepFlags, clock_gettime, clock_nanosleep};

use super::Errno;

/// Nanoseconds in a second, which scale the clock's whole seconds.
const NANOSECONDS_PER_SECOND: u64 = 1_000_000_000;

/// Nanoseconds in a millisecond, which `monotonic_ms` divides away.
const NANOSECONDS_PER_MILLISECOND: u64 = 1_000_000;

/// A clock that the guest programs read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    /// The wall clock, which the time tools step and slew to the host's.
    Realtime,
    /// The time since boot, without suspend, which the wall clock's discipline slews.
    Monotonic,
    /// The time since boot, with suspend.
    Boottime,
    /// The CPU time of every thread of the process, exited threads' included.
    ProcessCpuTime,
}

impl Clock {
    /// Returns the kernel's identifier of the clock.
    const fn id(self) -> ClockId {
        match self {
            Self::Realtime => ClockId::CLOCK_REALTIME,
            Self::Monotonic => ClockId::CLOCK_MONOTONIC,
            Self::Boottime => ClockId::CLOCK_BOOTTIME,
            Self::ProcessCpuTime => ClockId::CLOCK_PROCESS_CPUTIME_ID,
        }
    }
}

/// One reading of a clock: whole seconds, and nanoseconds within the second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClockTime {
    /// Whole seconds.
    pub(crate) seconds: i64,
    /// Nanoseconds within the second.
    pub(crate) nanoseconds: i64,
}

impl ClockTime {
    /// Returns the reading in nanoseconds, wrapping as the C programs' `int64_t` arithmetic did.
    pub(crate) fn wrapping_nanoseconds(self) -> i64 {
        self.seconds
            .wrapping_mul(NANOSECONDS_PER_SECOND as i64)
            .wrapping_add(self.nanoseconds)
    }
}

/// Reads `clock`, through the vDSO where the kernel provides one.
pub(crate) fn clock_time(clock: Clock) -> Result<ClockTime, Errno> {
    clock_gettime(clock.id()).map(|time| ClockTime {
        seconds: time.tv_sec(),
        nanoseconds: time.tv_nsec(),
    })
}

/// Returns the monotonic clock in nanoseconds, wrapping as the C programs' `u64` arithmetic did.
///
/// The C library reads this clock through the kernel's vDSO, without a system call. The C
/// `nvx-device-io` made the system call itself, so each read here costs less than there, and the
/// benchmark loops that read the clock can complete slightly more operations. The clock never
/// moves backwards, and its precision is that of the guest kernel's clocksource.
pub(crate) fn monotonic_ns() -> Result<u64, Errno> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC)?;
    Ok((time.tv_sec() as u64)
        .wrapping_mul(NANOSECONDS_PER_SECOND)
        .wrapping_add(time.tv_nsec() as u64))
}

/// Returns the monotonic clock in whole milliseconds.
///
/// It equals the C programs' `tv_sec * 1000 + tv_nsec / 1000000` until the nanosecond count
/// wraps, 584 years after the clock's start.
pub(crate) fn monotonic_ms() -> Result<u64, Errno> {
    monotonic_ns().map(|nanoseconds| nanoseconds / NANOSECONDS_PER_MILLISECOND)
}

/// Sleeps until the monotonic clock reaches `deadline_ns`, resuming after interruptions.
///
/// A deadline that has passed returns at once.
pub(crate) fn sleep_until_monotonic(deadline_ns: i64) {
    let per_second = NANOSECONDS_PER_SECOND as i64;
    let deadline = TimeSpec::new(deadline_ns / per_second, deadline_ns % per_second);
    while clock_nanosleep(
        ClockId::CLOCK_MONOTONIC,
        ClockNanosleepFlags::TIMER_ABSTIME,
        &deadline,
    ) == Err(Errno::EINTR)
    {}
}

#[cfg(test)]
mod tests {
    //! Checks of the clocks against the standard library's.

    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn advances_with_the_standard_monotonic_clock() {
        let started = monotonic_ns().unwrap();
        let instant = Instant::now();
        std::thread::sleep(Duration::from_millis(2));
        let elapsed = instant.elapsed();
        let ended = monotonic_ns().unwrap();
        assert!(started > 0);
        assert!(
            ended - started >= elapsed.as_nanos() as u64,
            "{started} {ended} {elapsed:?}"
        );
    }

    #[test]
    fn milliseconds_truncate_the_nanoseconds() {
        let before = monotonic_ns().unwrap() / NANOSECONDS_PER_MILLISECOND;
        let milliseconds = monotonic_ms().unwrap();
        let after = monotonic_ns().unwrap() / NANOSECONDS_PER_MILLISECOND;
        assert!(
            (before..=after).contains(&milliseconds),
            "{before} {milliseconds} {after}"
        );
    }

    #[test]
    fn every_clock_reads_and_converts_to_nanoseconds() {
        for clock in [
            Clock::Realtime,
            Clock::Monotonic,
            Clock::Boottime,
            Clock::ProcessCpuTime,
        ] {
            let time = clock_time(clock).unwrap();
            assert!(
                (0..NANOSECONDS_PER_SECOND as i64).contains(&time.nanoseconds),
                "{clock:?} {time:?}"
            );
        }
        let time = ClockTime {
            seconds: 2,
            nanoseconds: 5,
        };
        assert_eq!(time.wrapping_nanoseconds(), 2_000_000_005);
    }

    #[test]
    fn sleeps_reach_their_deadlines() {
        let start = clock_time(Clock::Monotonic).unwrap().wrapping_nanoseconds();
        let deadline = start + 2 * NANOSECONDS_PER_MILLISECOND as i64;
        sleep_until_monotonic(deadline);
        assert!(clock_time(Clock::Monotonic).unwrap().wrapping_nanoseconds() >= deadline);
        sleep_until_monotonic(start);
    }
}
