//! Connected datagram sockets.

use std::net::SocketAddrV4;
use std::os::fd::AsRawFd;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, SockaddrIn};

use super::{Errno, FileDescriptor};

/// Creates a close-on-exec IPv4 UDP socket connected to `peer`.
///
/// Connecting fixes the destination of every `write` and the only source from which `read`
/// receives, as the `socket` and `connect` calls of `nvx-device-io`'s C program did.
pub(crate) fn connect_datagram(peer: SocketAddrV4) -> Result<FileDescriptor, Errno> {
    let socket = nix::sys::socket::socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    nix::sys::socket::connect(socket.as_raw_fd(), &SockaddrIn::from(peer))?;
    Ok(FileDescriptor::from_owned(socket))
}
