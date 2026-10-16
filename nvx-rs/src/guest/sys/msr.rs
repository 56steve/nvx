//! Owned access to Linux's per-CPU model-specific-register devices, on x86-64.

use std::ffi::CString;
use std::mem::size_of;

use super::{Errno, FileDescriptor, Mode, OpenFlags, open};

/// Whether an MSR device opens for reading only or for reading and writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MsrAccess {
    /// Reads registers.
    Read,
    /// Reads and writes registers.
    ReadWrite,
}

/// An owned `/dev/cpu/N/msr` descriptor, whose file offsets are register numbers.
///
/// The `msr` driver turns a register access that faults with `#GP` into `EIO`, which the time
/// tools check for the registers that must fault.
#[derive(Debug)]
pub(crate) struct MsrFile(FileDescriptor);

impl MsrFile {
    /// Returns the path of the MSR device of `cpu`.
    pub(crate) fn path(cpu: usize) -> String {
        format!("/dev/cpu/{cpu}/msr")
    }

    /// Opens the MSR device of `cpu`, which needs `CAP_SYS_RAWIO`.
    pub(crate) fn open(cpu: usize, access: MsrAccess) -> Result<Self, Errno> {
        let path = CString::new(Self::path(cpu)).map_err(|_| Errno::EINVAL)?;
        let access = match access {
            MsrAccess::Read => OpenFlags::O_RDONLY,
            MsrAccess::ReadWrite => OpenFlags::O_RDWR,
        };
        open(&path, access | OpenFlags::O_CLOEXEC, Mode::empty()).map(Self)
    }

    /// Reads the 64-bit register `register`. A short read is `EIO`.
    pub(crate) fn read(&self, register: u32) -> Result<u64, Errno> {
        let mut bytes = [0; size_of::<u64>()];
        match self.0.pread(&mut bytes, u64::from(register))? {
            count if count == bytes.len() => Ok(u64::from_ne_bytes(bytes)),
            _ => Err(Errno::EIO),
        }
    }

    /// Writes `value` to the 64-bit register `register`. A short write is `EIO`.
    pub(crate) fn write(&self, register: u32, value: u64) -> Result<(), Errno> {
        match self.0.pwrite(&value.to_ne_bytes(), u64::from(register))? {
            count if count == size_of::<u64>() => Ok(()),
            _ => Err(Errno::EIO),
        }
    }
}
