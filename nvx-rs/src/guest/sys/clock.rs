//! The monotonic clock.

use nix::time::{ClockId, clock_gettime};

use super::Errno;

/// Returns the monotonic clock in nanoseconds, wrapping as the C programs' `u64` arithmetic did.
///
/// The C library reads this clock through the kernel's vDSO, without a system call. The C
/// `nvx-device-io` made the system call itself, so each read here costs less than there, and the
/// benchmark loops that read the clock can complete slightly more operations. The clock never
/// moves backwards, and its precision is that of the guest kernel's clocksource.
pub(crate) fn monotonic_ns() -> Result<u64, Errno> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC)?;
    Ok((time.tv_sec() as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(time.tv_nsec() as u64))
}

#[cfg(test)]
mod tests {
    //! Checks of the clock against the standard library's.

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
}
