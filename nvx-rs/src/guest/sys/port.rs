//! OpenVMM's x86 I/O ports, and the guest programs' access to them.
//!
//! The guest programs reach OpenVMM's ports in one of two ways. A program with `CAP_SYS_RAWIO`
//! may ask Linux for permission to execute `in` and `out` on them, which [`PortAccess`] owns on
//! x86-64. Any other access goes through [`PORT_DEVICE`], where the file offset is the port and
//! each byte read or written is one byte-wide port access.

use std::ffi::CStr;

/// The device whose file offsets are I/O ports, and whose bytes are byte-wide port accesses.
pub(crate) const PORT_DEVICE: &CStr = c"/dev/port";

/// OpenVMM's I/O ports that the guest programs use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Port {
    /// Port B's data port, OpenVMM's debug console: the guest writes diagnostic bytes to it, and
    /// reads the time ABI's restore packet and VM generation ID from it four bytes at a time.
    PortbData,
    /// Port B's status port: reading it returns the time ABI's status bits, and writing it
    /// selects what the data port or the window returns next.
    PortbStatus,
    /// Port B's four-byte window, from which the guest reads a time sample.
    PortbWindow,
    /// The control port: a byte written to it stops the VM with that byte as its exit status.
    Control,
    /// The snapshot port, which requests a capture and acknowledges a restore.
    Snapshot,
}

impl Port {
    /// Returns the port's number.
    pub(crate) const fn number(self) -> u16 {
        match self {
            Self::PortbData => 0xe9,
            Self::PortbStatus => 0xea,
            Self::PortbWindow => 0xeb,
            Self::Control => 0x604,
            Self::Snapshot => 0x605,
        }
    }

    /// Returns the port's offset in [`PORT_DEVICE`].
    pub(crate) const fn offset(self) -> u64 {
        self.number() as u64
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) use access::{PortAccess, power_off};

/// Permission to execute `in` and `out` on OpenVMM's ports, on x86-64.
#[cfg(target_arch = "x86_64")]
mod access {
    use std::arch::asm;
    use std::ffi::c_ulong;
    use std::mem::size_of;

    use super::{PORT_DEVICE, Port};
    use crate::guest::sys::{Errno, Mode, OpenFlags, open};

    /// Ranges of ports, as their first port and the last port of an access that starts in the
    /// range, whose permission [`PortAccess`] holds: port B, whose window a four-byte access
    /// reads, and the control and snapshot ports.
    const GRANTED: [(Port, Port, u16); 2] = [
        (Port::PortbData, Port::PortbWindow, size_of::<u32>() as u16),
        (Port::Control, Port::Snapshot, size_of::<u8>() as u16),
    ];

    /// The value of `ioperm`'s `turn_on` argument that grants permission.
    const GRANT: i32 = 1;
    /// The value of `ioperm`'s `turn_on` argument that revokes permission.
    const REVOKE: i32 = 0;

    /// Permission for the calling thread to execute `in` and `out` on OpenVMM's ports, which
    /// the time tools' port-B transactions and VM power-off need.
    ///
    /// Linux keeps the permission in the calling thread's I/O bitmap, which the threads and the
    /// processes that the thread creates later inherit, so a program requests it on its main
    /// thread before it starts threads or forks. Dropping the owner revokes the permission of
    /// the thread that drops it. `nix` does not wrap `ioperm`, so it is called through libc.
    #[derive(Debug)]
    pub(crate) struct PortAccess(());

    /// Changes the calling thread's permission for every port in `GRANTED` to `turn_on`.
    fn set_permission(turn_on: i32) -> Result<(), Errno> {
        for (first, last, width) in GRANTED {
            let count = last.number() + width - first.number();
            // SAFETY: `ioperm` reads only its integer arguments, which name ports in the x86 I/O
            // bitmap's range.
            Errno::result(unsafe {
                nix::libc::ioperm(c_ulong::from(first.number()), c_ulong::from(count), turn_on)
            })?;
        }
        Ok(())
    }

    impl PortAccess {
        /// Requests permission for OpenVMM's ports, which needs `CAP_SYS_RAWIO`.
        pub(crate) fn request() -> Result<Self, Errno> {
            set_permission(GRANT).map(|()| Self(()))
        }

        /// Reads one byte from `port` with one `in al, dx`.
        #[inline]
        pub(crate) fn read_u8(&self, port: Port) -> u8 {
            let value: u8;
            // SAFETY: `self` proves that this process may access `port`, and `in` accesses no
            // memory.
            unsafe {
                asm!(
                    "in al, dx",
                    in("dx") port.number(),
                    out("al") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }

        /// Reads four bytes from `port` with exactly one `in eax, dx`, never a string or a
        /// narrower access, as the time ABI's window requires.
        #[inline]
        pub(crate) fn read_u32(&self, port: Port) -> u32 {
            let value: u32;
            // SAFETY: `self` proves that this process may access `port` and the three ports
            // after it, and `in` accesses no memory.
            unsafe {
                asm!(
                    "in eax, dx",
                    in("dx") port.number(),
                    out("eax") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }

        /// Writes one byte to `port` with one `out dx, al`.
        #[inline]
        pub(crate) fn write_u8(&self, port: Port, value: u8) {
            // SAFETY: `self` proves that this process may access `port`, and `out` accesses no
            // memory.
            unsafe {
                asm!(
                    "out dx, al",
                    in("dx") port.number(),
                    in("al") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
    }

    impl Drop for PortAccess {
        /// Revokes the permission, best effort, as the process would at its exit.
        fn drop(&mut self) {
            let _ = set_permission(REVOKE);
        }
    }

    /// Stops the VM with `status` through the control port, and waits for OpenVMM to end the
    /// process.
    ///
    /// The port is written directly when the process may access it, and otherwise through
    /// [`PORT_DEVICE`], as the C program did. Neither write returns an error that the caller
    /// could act on, so the process waits for signals until the VM stops.
    pub(crate) fn power_off(status: u8) -> ! {
        match PortAccess::request() {
            Ok(access) => access.write_u8(Port::Control, status),
            Err(_) => {
                let flags = OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC;
                if let Ok(device) = open(PORT_DEVICE, flags, Mode::empty()) {
                    let _ = device.pwrite(&[status], Port::Control.offset());
                }
            }
        }
        loop {
            nix::unistd::pause();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Checks of OpenVMM's port map.

    use super::*;

    #[test]
    fn ports_have_openvmm_numbers() {
        let ports = [
            (Port::PortbData, 0xe9),
            (Port::PortbStatus, 0xea),
            (Port::PortbWindow, 0xeb),
            (Port::Control, 0x604),
            (Port::Snapshot, 0x605),
        ];
        for (port, number) in ports {
            assert_eq!(port.number(), number, "{port:?}");
            assert_eq!(port.offset(), u64::from(number), "{port:?}");
        }
    }
}
