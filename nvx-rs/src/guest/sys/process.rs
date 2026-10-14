//! Process-runtime behavior inherited from the C guest tools.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::signal::{SigHandler, Signal};

/// Whether this process inherited an ignored SIGPIPE across `exec`.
static SIGPIPE_IGNORED: AtomicBool = AtomicBool::new(false);

/// Runs before Rust's standard runtime changes SIGPIPE to `SIG_IGN`.
#[used]
#[unsafe(link_section = ".init_array")]
static CAPTURE_SIGPIPE: extern "C" fn() = capture_sigpipe;

/// Records the inherited SIGPIPE disposition before Rust startup, and leaves it in place.
extern "C" fn capture_sigpipe() {
    // SAFETY: `SIG_IGN` installs no handler. Setting it returns the previous disposition, which
    // the second call restores before another constructor runs.
    let Ok(inherited) = (unsafe { nix::sys::signal::signal(Signal::SIGPIPE, SigHandler::SigIgn) })
    else {
        return;
    };
    // SAFETY: `inherited` is the disposition that the preceding call returned.
    let _ = unsafe { nix::sys::signal::signal(Signal::SIGPIPE, inherited) };
    SIGPIPE_IGNORED.store(matches!(inherited, SigHandler::SigIgn), Ordering::Relaxed);
}

/// Restores the SIGPIPE disposition inherited across `exec`.
///
/// POSIX preserves `SIG_IGN` across `exec` and resets caught handlers to `SIG_DFL`. Rust's
/// standard runtime changes either state to `SIG_IGN` before `main`, so the constructor above
/// records the original first. The default disposition is restored unless the constructor saw an
/// ignored one, which preserves the historical behavior if a platform fails to run it.
pub(crate) fn restore_sigpipe() {
    let handler = if SIGPIPE_IGNORED.load(Ordering::Relaxed) {
        SigHandler::SigIgn
    } else {
        SigHandler::SigDfl
    };
    // SAFETY: neither `SIG_DFL` nor `SIG_IGN` installs a Rust callback.
    let _ = unsafe { nix::sys::signal::signal(Signal::SIGPIPE, handler) };
}

/// Standard output with the error behavior of C stdio.
///
/// C line-buffers a terminal, so a failed terminal write fails `printf`. It fully buffers other
/// destinations until `exit`, whose flush failure these tools ignored. This adapter performs the
/// write immediately but suppresses the same nonterminal errors; interrupted writes remain
/// visible so `Write::write_all` retries them.
pub(crate) struct Stdout {
    /// Whether C stdio would line-buffer standard output.
    terminal: bool,
}

impl Stdout {
    /// Returns an adapter for the process's standard output.
    pub(crate) fn new() -> Self {
        let stdout = io::stdout();
        let terminal = nix::unistd::isatty(&stdout).unwrap_or(false);
        Self { terminal }
    }
}

impl Write for Stdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let stdout = io::stdout();
        match nix::unistd::write(&stdout, bytes) {
            Ok(count) => Ok(count),
            Err(error) if self.terminal || error == nix::errno::Errno::EINTR => {
                Err(io::Error::from(error))
            }
            Err(_) => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
