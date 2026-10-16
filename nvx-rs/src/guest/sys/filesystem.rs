//! Safe path, mount, ownership, mode, and working-directory operations.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) use nix::mount::MsFlags as MountFlags;
use nix::sched::CloneFlags;
use nix::sys::stat::{FchmodatFlags, Mode};
use nix::unistd::{AccessFlags, Gid, Uid};

use super::Errno;

/// Converts a Unix path to a NUL-terminated string, or `EINVAL` if it contains a NUL.
pub(crate) fn path_c_string(path: &Path) -> Result<CString, Errno> {
    super::c_string(path.as_os_str().as_bytes())
}

/// Mounts `source` on `target` with no data argument.
pub(crate) fn mount(
    source: Option<&CStr>,
    target: &CStr,
    filesystem: Option<&CStr>,
    flags: MountFlags,
) -> Result<(), Errno> {
    nix::mount::mount(source, target, filesystem, flags, None::<&CStr>)
}

/// Changes the owner and group of `path`, following the final symlink like C `chown`.
pub(crate) fn chown(path: &CStr, uid: u32, gid: u32) -> Result<(), Errno> {
    nix::unistd::chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
}

/// Changes the permission bits of `path`, following the final symlink like C `chmod`.
///
/// `fchmodat` relative to the working directory, without `AT_SYMLINK_NOFOLLOW`, is the system
/// call that `chmod` makes.
pub(crate) fn chmod(path: &CStr, mode: Mode) -> Result<(), Errno> {
    nix::sys::stat::fchmodat(
        nix::fcntl::AT_FDCWD,
        path,
        mode,
        FchmodatFlags::FollowSymlink,
    )
}

/// Changes the process working directory to `path`.
pub(crate) fn change_directory(path: &CStr) -> Result<(), Errno> {
    nix::unistd::chdir(path)
}

/// Checks that `path` exists with `access` and `F_OK`, which does not check permissions, and
/// returns `access`'s error when it does not.
pub(crate) fn check_exists(path: &CStr) -> Result<(), Errno> {
    nix::unistd::access(path, AccessFlags::F_OK)
}

/// Returns whether `path` exists; see [`check_exists`].
pub(crate) fn path_exists(path: &CStr) -> bool {
    check_exists(path).is_ok()
}

/// Moves the calling thread into a new mount namespace whose mounts, starting with the root's,
/// stop propagating to and from the namespace that it leaves, so that its mounts stay private.
pub(crate) fn enter_private_mount_namespace() -> Result<(), Errno> {
    nix::sched::unshare(CloneFlags::CLONE_NEWNS)?;
    nix::mount::mount(
        None::<&CStr>,
        c"/",
        None::<&CStr>,
        MountFlags::MS_REC | MountFlags::MS_PRIVATE,
        None::<&CStr>,
    )
}

/// A directory at a fixed path, which dropping the owner removes, best effort.
///
/// It also removes a directory that existed already, such as one that an interrupted earlier
/// owner left behind, which the owner then reuses.
#[derive(Debug)]
pub(crate) struct ScratchDirectory<'a>(&'a CStr);

impl<'a> ScratchDirectory<'a> {
    /// Creates `path` as a directory with `mode` unless it exists already, ignoring other
    /// failures as the C program did, because a later use of the directory reports them.
    pub(crate) fn create(path: &'a CStr, mode: Mode) -> Self {
        let _ = nix::unistd::mkdir(path, mode);
        Self(path)
    }

    /// Returns the directory's path.
    pub(crate) fn path(&self) -> &'a CStr {
        self.0
    }
}

impl Drop for ScratchDirectory<'_> {
    /// Removes the directory, which must be empty, best effort.
    fn drop(&mut self) {
        let _ = nix::unistd::unlinkat(
            nix::fcntl::AT_FDCWD,
            self.0,
            nix::unistd::UnlinkatFlags::RemoveDir,
        );
    }
}

/// A tmpfs mount, which dropping the owner unmounts, best effort, unless
/// [`unmount`](Self::unmount) did.
#[derive(Debug)]
pub(crate) struct TmpfsMount<'a>(Option<&'a CStr>);

impl<'a> TmpfsMount<'a> {
    /// Mounts a tmpfs named `source` on `target`, without set-user-ID files, device files, or
    /// executables, and with the filesystem's mount options `options`.
    pub(crate) fn mount(source: &CStr, target: &'a CStr, options: &CStr) -> Result<Self, Errno> {
        nix::mount::mount(
            Some(source),
            target,
            Some(c"tmpfs"),
            MountFlags::MS_NOSUID | MountFlags::MS_NODEV | MountFlags::MS_NOEXEC,
            Some(options),
        )?;
        Ok(Self(Some(target)))
    }

    /// Unmounts the filesystem, and reports a failure.
    pub(crate) fn unmount(mut self) -> Result<(), Errno> {
        self.0.take().map_or(Ok(()), nix::mount::umount)
    }
}

impl Drop for TmpfsMount<'_> {
    /// Unmounts the filesystem, best effort, unless [`unmount`](Self::unmount) did.
    fn drop(&mut self) {
        if let Some(target) = self.0.take() {
            let _ = nix::mount::umount(target);
        }
    }
}

/// Returns the physical working directory, or `ERANGE` if it needs `maximum` bytes or more.
///
/// `libc`'s `getcwd` writes into a fixed buffer, which preserves the C agent's `ERANGE`, whereas
/// `nix` and the standard library grow their storage until the path fits.
pub(crate) fn current_directory(maximum: usize) -> Result<Vec<u8>, Errno> {
    let mut bytes = vec![0u8; maximum];
    // SAFETY: the vector supplies `maximum` writable bytes, and libc NUL-terminates the path on
    // success.
    let result = unsafe { nix::libc::getcwd(bytes.as_mut_ptr().cast(), bytes.len()) };
    if result.is_null() {
        return Err(Errno::last());
    }
    let length = bytes
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(Errno::ERANGE)?;
    bytes.truncate(length);
    Ok(bytes)
}
