//! Safe readiness polling for owned descriptors.

use std::os::fd::BorrowedFd;
use std::time::Duration;

use nix::poll::{PollFd, PollFlags, PollTimeout};

use super::{ChildSignalWait, Errno, FileDescriptor, SignalDescriptor, Timer};

/// Polls up to three descriptors for readable data or hangup.
///
/// `None` entries are omitted, and the returned booleans preserve the caller's three positions.
/// Any reported event wakes that position, matching the C agent's `revents != 0` test.
pub(crate) fn readable_or_hung_up(
    descriptors: [Option<&FileDescriptor>; 3],
    timeout_milliseconds: u16,
) -> Result<[bool; 3], Errno> {
    let mut positions = Vec::with_capacity(descriptors.len());
    let mut polls = Vec::with_capacity(descriptors.len());
    for (position, descriptor) in descriptors.into_iter().enumerate() {
        if let Some(descriptor) = descriptor {
            positions.push(position);
            polls.push(PollFd::new(
                descriptor.borrowed(),
                PollFlags::POLLIN | PollFlags::POLLHUP,
            ));
        }
    }
    nix::poll::poll(&mut polls, timeout_milliseconds)?;
    let mut ready = [false; 3];
    for (position, descriptor) in positions.into_iter().zip(polls) {
        ready[position] = descriptor
            .revents()
            .is_some_and(|events| !events.is_empty());
    }
    Ok(ready)
}

/// A descriptor owner that [`poll`] waits on for input.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pollable<'a> {
    /// A file, device, pipe, or socket.
    File(&'a FileDescriptor),
    /// A timer, which polls readable once it expires.
    Timer(&'a Timer),
    /// A signal descriptor, which polls readable while one of its signals is pending.
    Signals(&'a SignalDescriptor),
    /// A child-signal wait, which polls readable while `SIGCHLD` is pending.
    ChildSignal(&'a ChildSignalWait),
}

impl Pollable<'_> {
    /// Borrows the owner's descriptor.
    fn borrowed(&self) -> BorrowedFd<'_> {
        match self {
            Self::File(descriptor) => descriptor.borrowed(),
            Self::Timer(timer) => timer.borrowed(),
            Self::Signals(signals) => signals.borrowed(),
            Self::ChildSignal(wait) => wait.borrowed(),
        }
    }
}

/// The events that [`poll`] reports for one descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PollEvents {
    /// The descriptor has input to read.
    pub(crate) readable: bool,
    /// The descriptor has an error condition, which a read returns.
    pub(crate) error: bool,
}

/// Waits at most `timeout`, or without a limit for `None`, until one of `descriptors` has input
/// or an error, with one `poll` call and without allocating, and returns each descriptor's
/// events in the caller's order. An interrupted call is `EINTR`, not a retry.
///
/// A timeout of more than `i32::MAX` milliseconds waits for `i32::MAX` milliseconds, about 24
/// days.
pub(crate) fn poll<const N: usize>(
    descriptors: [Pollable<'_>; N],
    timeout: Option<Duration>,
) -> Result<[PollEvents; N], Errno> {
    let timeout = timeout.map_or(PollTimeout::NONE, |timeout| {
        PollTimeout::try_from(timeout.as_millis()).unwrap_or(PollTimeout::MAX)
    });
    let mut raw = descriptors
        .each_ref()
        .map(|descriptor| PollFd::new(descriptor.borrowed(), PollFlags::POLLIN));
    nix::poll::poll(&mut raw, timeout)?;
    Ok(raw.map(|descriptor| {
        let returned = descriptor.revents().unwrap_or(PollFlags::empty());
        PollEvents {
            readable: returned.contains(PollFlags::POLLIN),
            error: returned.contains(PollFlags::POLLERR),
        }
    }))
}

#[cfg(test)]
mod tests {
    //! Checks of polling pipes.

    use super::*;
    use crate::guest::sys::create_pipe;

    #[test]
    fn poll_reports_input_in_the_callers_order() {
        let (idle_reader, _idle_writer) = create_pipe().unwrap();
        let (reader, writer) = create_pipe().unwrap();
        writer.write_all(b"x").unwrap();
        let events = poll(
            [Pollable::File(&idle_reader), Pollable::File(&reader)],
            Some(Duration::ZERO),
        )
        .unwrap();
        assert_eq!(
            events,
            [
                PollEvents::default(),
                PollEvents {
                    readable: true,
                    error: false
                }
            ]
        );
    }
}
