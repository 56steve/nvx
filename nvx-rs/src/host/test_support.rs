//! Helpers that the host's unit tests share.

use std::thread;
use std::time::{Duration, Instant};

/// How long a test waits for a lock or claim that it released to be free.
///
/// A process that a concurrent test spawns holds a copy of each descriptor of the test process
/// from its fork until it executes its program, and with it the lock or claim of a file that a
/// test had open, for that moment.
const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// Interval at which a test checks a lock or claim that it released again, the interval at which
/// the host retries a lock.
const RELEASE_RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Returns whether `free` holds before [`RELEASE_WAIT`] passes, checking it again at every
/// [`RELEASE_RETRY_INTERVAL`] until then.
pub(crate) fn released(mut free: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + RELEASE_WAIT;
    while !free() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(RELEASE_RETRY_INTERVAL);
    }
    true
}
