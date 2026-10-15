//! `nvx-port-io`: reads and writes x86 I/O ports through `/dev/port`, and reads the VM generation
//! ID that OpenVMM exposes behind a selector port.
//!
//! - `read-u8 PORT` prints the byte at PORT in decimal.
//! - `write-u8 PORT VALUE` writes the byte VALUE to PORT.
//! - `read-generation-id DATA_PORT SELECT_PORT [OUTPUT]` checks the availability bit of the
//!   status byte at SELECT_PORT, selects the generation ID, reads its 16 bytes from DATA_PORT, and
//!   prints them in hexadecimal or writes them to OUTPUT. It holds the lock of the time ABI's
//!   selector transactions meanwhile.

use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::process::ExitCode;

use super::parse::parse_u64;
use super::status::{CommandError, ExitStatus};
use super::sys::{self, Errno, FileDescriptor, FileLock, Mode, OpenFlags};

/// Exact usage diagnostic inherited from the C program.
const USAGE: &[u8] = b"usage: nvx-port-io read-generation-id DATA_PORT SELECT_PORT [OUTPUT] \
| read-u8 PORT | write-u8 PORT VALUE\n";
/// Largest x86 I/O-port number accepted by the command line.
const PORT_MAX: u64 = 0xffff;
/// Selector value that exposes the VM generation ID on the data port.
const GENERATION_ID_SELECT: u8 = 0xa6;
/// Size in bytes of a VM generation ID.
const GENERATION_ID_SIZE: usize = 16;
/// Status-port bit that says the VM generation ID is available.
const STATUS_GENERATION_ID_AVAILABLE: u8 = 32;

/// The files that the program uses.
struct Paths<'a> {
    /// The I/O port device, `/dev/port`.
    port: &'a CStr,
    /// The directory of the lock, `/run/nvx`.
    lock_directory: &'a CStr,
    /// The lock that serializes selector transactions with the time ABI, `/run/nvx/portb.lock`.
    lock: &'a CStr,
}

/// Production device and lock paths.
const PATHS: Paths<'static> = Paths {
    port: c"/dev/port",
    lock_directory: c"/run/nvx",
    lock: c"/run/nvx/portb.lock",
};

/// Runs `nvx-port-io` with the process's arguments. It exits with 0, 1 when an operation fails,
/// or 2 for a usage error.
pub fn main() -> ExitCode {
    sys::restore_sigpipe();
    ExitStatus::from(run(
        &sys::args(),
        &PATHS,
        &mut sys::Stdout::new(),
        &mut io::stderr(),
    ))
    .into()
}

/// Selects the requested port operation and runs it against injectable paths and output
/// streams, with owned resources.
fn run(
    args: &[CString],
    paths: &Paths<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let port = |index: usize| parse_u64(&args[index]).filter(|&port| port <= PORT_MAX);
    if args.len() >= 3
        && let Some(offset) = port(2)
    {
        let command = args[1].as_bytes();
        if args.len() == 3 && command == b"read-u8" {
            return read_u8(paths, offset, out, err);
        }
        if args.len() == 4
            && command == b"write-u8"
            && let Some(value) = parse_u64(&args[3]).filter(|&value| value <= u64::from(u8::MAX))
        {
            return write_u8(paths, offset, value as u8, err);
        }
        if (args.len() == 4 || args.len() == 5)
            && command == b"read-generation-id"
            && let Some(select_offset) = port(3)
        {
            return read_generation_id(paths, offset, select_offset, args.get(4), out, err);
        }
    }
    sys::report(err, &[USAGE]);
    Err(CommandError::Usage)
}

/// Reads and prints one byte from `offset` of the port device.
fn read_u8(
    paths: &Paths<'_>,
    offset: u64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let port = open(
        paths.port,
        OpenFlags::O_RDONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
        err,
    )?;
    let value = read_port_byte(&port, offset, err)?;
    close_port(paths, port, err)?;
    // The C program ignored the result of its printf.
    let _ = sys::print(out, format!("{value}\n").as_bytes());
    Ok(())
}

/// Writes `value` to `offset` of the port device.
fn write_u8(
    paths: &Paths<'_>,
    offset: u64,
    value: u8,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let port = open(
        paths.port,
        OpenFlags::O_WRONLY | OpenFlags::O_CLOEXEC,
        Mode::empty(),
        err,
    )?;
    write_port_byte(&port, offset, value, err)?;
    close_port(paths, port, err)
}

/// Reads the selected VM generation ID and writes it to `output` or standard output.
fn read_generation_id(
    paths: &Paths<'_>,
    data_offset: u64,
    select_offset: u64,
    output: Option<&CString>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let (port, generation_id) = read_locked_generation_id(paths, data_offset, select_offset, err)?;
    close_port(paths, port, err)?;

    let Some(path) = output else {
        let mut line: String = generation_id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        line.push('\n');
        if sys::print(out, line.as_bytes()).is_err() {
            sys::report(err, &[b"nvx-port-io: write output failed\n"]);
            return Err(CommandError::Operation);
        }
        return Ok(());
    };
    let flags = OpenFlags::O_WRONLY
        | OpenFlags::O_CREAT
        | OpenFlags::O_TRUNC
        | OpenFlags::O_CLOEXEC
        | OpenFlags::O_NOFOLLOW;
    let descriptor = open(path, flags, Mode::S_IRUSR | Mode::S_IWUSR, err)?;
    write_all(&descriptor, &generation_id, err)?;
    if let Err(errno) = descriptor.close() {
        report(err, b"close", path, errno);
        return Err(CommandError::Operation);
    }
    Ok(())
}

/// Selects and reads the VM generation ID under the portb lock, and returns it with the open
/// port device.
///
/// The lock is released when this returns, before the caller closes the port, as in the C
/// program. On a failure the port closes first, then the lock, also as in the C program.
fn read_locked_generation_id(
    paths: &Paths<'_>,
    data_offset: u64,
    select_offset: u64,
    err: &mut dyn Write,
) -> Result<(FileDescriptor, [u8; GENERATION_ID_SIZE]), CommandError> {
    let _lock = lock_portb(paths, err)?;
    let port = open(
        paths.port,
        OpenFlags::O_RDWR | OpenFlags::O_CLOEXEC,
        Mode::empty(),
        err,
    )?;
    let status = read_port_byte(&port, select_offset, err)?;
    if status & STATUS_GENERATION_ID_AVAILABLE == 0 {
        let message =
            format!("nvx-port-io: VM generation ID is unavailable on port {select_offset:#x}\n");
        sys::report(err, &[message.as_bytes()]);
        return Err(CommandError::Operation);
    }
    let mut generation_id = [0; GENERATION_ID_SIZE];
    write_port_byte(&port, select_offset, GENERATION_ID_SELECT, err)?;
    read_port_bytes(&port, data_offset, &mut generation_id, err)?;
    Ok((port, generation_id))
}

/// Takes the lock that serializes selector transactions on portb with the time ABI.
fn lock_portb(paths: &Paths<'_>, err: &mut dyn Write) -> Result<FileLock, CommandError> {
    if let Err(errno) = sys::create_directory(
        paths.lock_directory,
        Mode::S_IRWXU | Mode::S_IRGRP | Mode::S_IXGRP | Mode::S_IROTH | Mode::S_IXOTH,
    ) && errno != Errno::EEXIST
    {
        report(err, b"mkdir", paths.lock_directory, errno);
        return Err(CommandError::Operation);
    }
    let lock = open(
        paths.lock,
        OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
        err,
    )?;
    lock.lock_exclusive().map_err(|errno| {
        report(err, b"flock", paths.lock, errno);
        CommandError::Operation
    })
}

/// Opens `path`, and reports a failure.
fn open(
    path: &CStr,
    flags: OpenFlags,
    mode: Mode,
    err: &mut dyn Write,
) -> Result<FileDescriptor, CommandError> {
    sys::open(path, flags, mode).map_err(|errno| {
        report(err, b"open", path, errno);
        CommandError::Operation
    })
}

/// Closes the port device, and reports a failure.
fn close_port(
    paths: &Paths<'_>,
    port: FileDescriptor,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    if let Err(errno) = port.close() {
        report(err, b"close", paths.port, errno);
        return Err(CommandError::Operation);
    }
    Ok(())
}

/// Reads the byte at `offset` of the port device, retrying after `EINTR`.
fn read_port_byte(
    port: &FileDescriptor,
    offset: u64,
    err: &mut dyn Write,
) -> Result<u8, CommandError> {
    let mut value = 0_u8;
    match sys::retry(|| port.pread(std::slice::from_mut(&mut value), offset)) {
        Ok(1) => return Ok(value),
        Ok(0) => {
            let message = format!("nvx-port-io: read port {offset:#x}: ");
            sys::report(err, &[message.as_bytes(), b"unexpected end of file\n"]);
        }
        Ok(_) => unreachable!("a one-byte pread cannot return more than one byte"),
        Err(errno) => {
            let message = format!("nvx-port-io: read port {offset:#x}: ");
            sys::report(err, &[message.as_bytes(), errno.desc().as_bytes(), b"\n"]);
        }
    }
    Err(CommandError::Operation)
}

/// Writes `value` at `offset` of the port device, retrying after `EINTR`.
fn write_port_byte(
    port: &FileDescriptor,
    offset: u64,
    value: u8,
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    match sys::retry(|| port.pwrite(std::slice::from_ref(&value), offset)) {
        Ok(1) => return Ok(()),
        Ok(0) => {
            let message = format!("nvx-port-io: write port {offset:#x}: ");
            sys::report(err, &[message.as_bytes(), b"no byte written\n"]);
        }
        Ok(_) => unreachable!("a one-byte pwrite cannot return more than one byte"),
        Err(errno) => {
            let message = format!("nvx-port-io: write port {offset:#x}: ");
            sys::report(err, &[message.as_bytes(), errno.desc().as_bytes(), b"\n"]);
        }
    }
    Err(CommandError::Operation)
}

/// Reads each byte of `buffer` from the same port.
fn read_port_bytes(
    port: &FileDescriptor,
    offset: u64,
    buffer: &mut [u8],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    for byte in buffer {
        *byte = read_port_byte(port, offset, err)?;
    }
    Ok(())
}

/// Writes all of `bytes`, retrying after `EINTR` and short writes.
fn write_all(
    descriptor: &FileDescriptor,
    bytes: &[u8],
    err: &mut dyn Write,
) -> Result<(), CommandError> {
    let mut offset = 0;
    while offset < bytes.len() {
        let rest = &bytes[offset..];
        match sys::retry(|| descriptor.write(rest)) {
            Ok(count) if count > 0 => {
                offset += count;
                continue;
            }
            Ok(_) => {
                sys::report(err, &[b"nvx-port-io: write output: no byte written\n"]);
            }
            Err(errno) => {
                let reason = errno.desc().as_bytes();
                sys::report(err, &[b"nvx-port-io: write output: ", reason, b"\n"]);
            }
        }
        return Err(CommandError::Operation);
    }
    Ok(())
}

/// Reports a failed operation on `path`, as `nvx-port-io: OPERATION PATH: TEXT`.
fn report(err: &mut dyn Write, operation: &[u8], path: &CStr, errno: Errno) {
    sys::report(
        err,
        &[
            b"nvx-port-io: ",
            operation,
            b" ",
            path.to_bytes(),
            b": ",
            errno.desc().as_bytes(),
            b"\n",
        ],
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::guest::sys::tests::Failing;

    /// A regular file that stands for `/dev/port`, and a lock below a temporary directory.
    struct Fixture {
        directory: tempfile::TempDir,
        port: PathBuf,
        lock_directory: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let port = directory.path().join("port");
            fs::write(&port, [0; 0x100]).unwrap();
            let lock_directory = directory.path().join("run");
            Self {
                directory,
                port,
                lock_directory,
            }
        }

        fn run_with(&self, arguments: &[&str], out: &mut dyn Write) -> (u8, Vec<u8>) {
            let mut args = vec![CString::new("nvx-port-io").unwrap()];
            args.extend(
                arguments
                    .iter()
                    .map(|argument| CString::new(*argument).unwrap()),
            );
            let c_path = |path: &Path| CString::new(path.as_os_str().as_bytes()).unwrap();
            let (port, lock_directory) = (c_path(&self.port), c_path(&self.lock_directory));
            let lock = c_path(&self.lock_directory.join("portb.lock"));
            let paths = Paths {
                port: &port,
                lock_directory: &lock_directory,
                lock: &lock,
            };
            let mut err = Vec::new();
            let status = ExitStatus::from(run(&args, &paths, out, &mut err));
            (status as u8, err)
        }

        fn run(&self, arguments: &[&str]) -> (u8, Vec<u8>, Vec<u8>) {
            let mut out = Vec::new();
            let (status, err) = self.run_with(arguments, &mut out);
            (status, out, err)
        }

        fn set(&self, offset: usize, value: u8) {
            let mut bytes = fs::read(&self.port).unwrap();
            bytes[offset] = value;
            fs::write(&self.port, bytes).unwrap();
        }

        fn byte(&self, offset: usize) -> u8 {
            fs::read(&self.port).unwrap()[offset]
        }
    }

    fn message(operation: &str, path: &Path, errno: Errno) -> Vec<u8> {
        let mut message = format!("nvx-port-io: {operation} ").into_bytes();
        message.extend_from_slice(path.as_os_str().as_bytes());
        message.extend_from_slice(b": ");
        message.extend_from_slice(errno.desc().as_bytes());
        message.push(b'\n');
        message
    }

    #[test]
    fn usage_names_every_command() {
        let fixture = Fixture::new();
        for arguments in [
            &[][..],
            &["read-u8"],
            &["read-u8", "1", "2"],
            &["read-u8", "0x10000"],
            &["read-u8", "-1"],
            &["read-u8", "+1"],
            &["read-u8", "1 "],
            &["read-u8", "0161"],
            &["write-u8", "1"],
            &["write-u8", "1", "256"],
            &["write-u8", "1", "0x"],
            &["read-generation-id", "1"],
            &["read-generation-id", "1", "65536"],
            &["read-generation-id", "1", "2", "out", "extra"],
            &["read", "1"],
        ] {
            assert_eq!(
                fixture.run(arguments),
                (2, Vec::new(), USAGE.to_vec()),
                "{arguments:?}"
            );
        }
        assert_eq!(
            USAGE,
            b"usage: nvx-port-io read-generation-id DATA_PORT SELECT_PORT [OUTPUT] | \
read-u8 PORT | write-u8 PORT VALUE\n"
        );
    }

    #[test]
    fn reads_and_writes_a_byte() {
        let fixture = Fixture::new();
        fixture.set(0x70, 42);
        assert_eq!(
            fixture.run(&["read-u8", "0x70"]),
            (0, b"42\n".to_vec(), Vec::new())
        );
        assert_eq!(
            fixture.run(&["write-u8", "113", "255"]),
            (0, Vec::new(), Vec::new())
        );
        assert_eq!(fixture.byte(0x71), 255);
        // The C program ignored a failure to print the byte.
        let mut out = Failing(Errno::EIO);
        assert_eq!(
            fixture.run_with(&["read-u8", "0x70"], &mut out),
            (0, Vec::new())
        );
    }

    #[test]
    fn reports_a_read_past_the_end_and_a_missing_device() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.run(&["read-u8", "0x200"]),
            (
                1,
                Vec::new(),
                b"nvx-port-io: read port 0x200: unexpected end of file\n".to_vec()
            )
        );
        fs::remove_file(&fixture.port).unwrap();
        for arguments in [&["read-u8", "1"][..], &["write-u8", "1", "1"]] {
            assert_eq!(
                fixture.run(arguments),
                (1, Vec::new(), message("open", &fixture.port, Errno::ENOENT))
            );
        }
    }

    #[test]
    fn reads_the_generation_id() {
        let fixture = Fixture::new();
        fixture.set(0x50, STATUS_GENERATION_ID_AVAILABLE | 1);
        fixture.set(0x51, 0x5c);
        let mut expected = "5c".repeat(GENERATION_ID_SIZE).into_bytes();
        expected.push(b'\n');
        assert_eq!(
            fixture.run(&["read-generation-id", "0x51", "0x50"]),
            (0, expected, Vec::new())
        );
        assert_eq!(fixture.byte(0x50), GENERATION_ID_SELECT);
        assert!(fixture.lock_directory.join("portb.lock").is_file());

        fixture.set(0x50, STATUS_GENERATION_ID_AVAILABLE);
        let output = fixture.directory.path().join("generation-id");
        let arguments = ["read-generation-id", "81", "80", output.to_str().unwrap()];
        assert_eq!(fixture.run(&arguments), (0, Vec::new(), Vec::new()));
        assert_eq!(fs::read(&output).unwrap(), [0x5c; GENERATION_ID_SIZE]);

        fixture.set(0x50, STATUS_GENERATION_ID_AVAILABLE);
        let mut out = Failing(Errno::EIO);
        assert_eq!(
            fixture.run_with(&["read-generation-id", "0x51", "0x50"], &mut out),
            (1, b"nvx-port-io: write output failed\n".to_vec())
        );
    }

    #[test]
    fn reports_an_unavailable_generation_id() {
        let fixture = Fixture::new();
        fixture.set(0x50, !STATUS_GENERATION_ID_AVAILABLE);
        assert_eq!(
            fixture.run(&["read-generation-id", "0x51", "0x50"]),
            (
                1,
                Vec::new(),
                b"nvx-port-io: VM generation ID is unavailable on port 0x50\n".to_vec()
            )
        );
        assert_eq!(fixture.byte(0x50), !STATUS_GENERATION_ID_AVAILABLE);
    }

    #[test]
    fn refuses_to_follow_an_output_symlink() {
        let fixture = Fixture::new();
        fixture.set(0x50, STATUS_GENERATION_ID_AVAILABLE);
        let link = fixture.directory.path().join("link");
        std::os::unix::fs::symlink(fixture.directory.path().join("target"), &link).unwrap();
        let arguments = ["read-generation-id", "0x51", "0x50", link.to_str().unwrap()];
        assert_eq!(
            fixture.run(&arguments),
            (1, Vec::new(), message("open", &link, Errno::ELOOP))
        );
    }

    #[test]
    fn reports_a_lock_directory_that_cannot_exist() {
        let mut fixture = Fixture::new();
        fixture.lock_directory = fixture.port.join("run");
        assert_eq!(
            fixture.run(&["read-generation-id", "0x51", "0x50"]),
            (
                1,
                Vec::new(),
                message("mkdir", &fixture.lock_directory, Errno::ENOTDIR)
            )
        );
    }
}
