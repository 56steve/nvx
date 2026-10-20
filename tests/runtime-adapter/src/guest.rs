//! The adapter's guest half: it serves the guest's role of the NVXS session with the adapter's
//! own [framing](crate::frame), through `nvx::guest::nvxs` alone.

use std::fmt;
use std::io::{Read, Write};

use nvx::guest::nvxs::{GuestConnection, GuestConnectionError};
use nvx::protocol::OUTER_MIN_RECEIVE_WINDOW;
use nvx::protocol::nvxs::GuestEvent;

use crate::frame::{Decoder, FrameError, Message};

/// The receive window that the guest acknowledges each epoch with: the smallest that NVXS allows,
/// so that the rest of a longer message arrives only as the guest credits what it took.
pub const RECEIVE_WINDOW: u32 = OUTER_MIN_RECEIVE_WINDOW;

/// How serving ended, when nothing failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// The host asked the guest to stop, and the guest confirmed.
    Stopped,
    /// The broker closed the stream at a record boundary.
    Closed,
}

/// A failure that ended serving.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServeError {
    /// The NVXS session failed.
    Session(GuestConnectionError),
    /// The host sent a malformed message.
    Frame(FrameError),
    /// The host sent a message of a kind that only the guest sends.
    Unexpected(u8),
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::Unexpected(kind) => {
                write!(
                    formatter,
                    "the host sent a message of kind {kind}, which only the guest sends"
                )
            }
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::Unexpected(_) => None,
        }
    }
}

impl From<GuestConnectionError> for ServeError {
    fn from(error: GuestConnectionError) -> Self {
        Self::Session(error)
    }
}

impl From<FrameError> for ServeError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

/// Serves the guest's role of the NVXS session on `stream`, which connects to the broker's guest
/// leg, until the host stops the guest or the broker closes the stream.
///
/// The guest attaches, then:
///
/// - It acknowledges each RESET with [`RECEIVE_WINDOW`], once it dropped the earlier epoch's
///   partial message: a new epoch starts with a new message.
/// - It credits each DATA payload back once it holds the payload, so that a message longer than
///   the window arrives whole.
/// - It answers [`Message::Ping`] with [`Message::Pong`], sends each [`Message::Echo`] back, and
///   answers [`Message::Stop`] with [`Message::Stopped`], then returns without reading further.
///
/// A failure of the session, a malformed message, and a message that only the guest sends end
/// serving with an error.
pub fn serve<S: Read + Write>(stream: S) -> Result<Served, ServeError> {
    let mut connection = GuestConnection::new(stream);
    connection.attach()?;
    let mut decoder = Decoder::new();
    loop {
        let payload = match connection.next_event() {
            Ok(GuestEvent::Reset(_)) => {
                decoder.clear();
                connection.acknowledge(RECEIVE_WINDOW)?;
                continue;
            }
            Ok(GuestEvent::Data(payload)) => payload,
            Err(GuestConnectionError::Closed) => return Ok(Served::Closed),
            Err(error) => return Err(error.into()),
        };
        // A DATA payload has at most `OUTER_MAX_PAYLOAD` bytes, so its length fits a `u32`.
        connection.credit(payload.len() as u32)?;
        let mut rest = payload.as_slice();
        while !rest.is_empty() {
            let (consumed, message) = decoder.push(rest)?;
            rest = &rest[consumed..];
            let reply = match message {
                None => continue,
                Some(Message::Ping) => Message::Pong,
                Some(Message::Echo(body)) => Message::Echo(body),
                Some(Message::Stop) => {
                    connection.send(&Message::Stopped.encode()?)?;
                    return Ok(Served::Stopped);
                }
                Some(message) => return Err(ServeError::Unexpected(message.kind())),
            };
            connection.send(&reply.encode()?)?;
        }
    }
}
