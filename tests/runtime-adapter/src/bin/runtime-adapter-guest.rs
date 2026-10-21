//! The runtime adapter's guest agent, a static `x86_64-unknown-linux-musl` executable.
//!
//! - `runtime-adapter-guest` reports the contract that it was built against.
//! - `runtime-adapter-guest serve` serves the host program with the adapter's own framing on its
//!   standard input and output, which connect to the guest's end of the control session between
//!   the host and the guest, such as a control console in raw mode. It exits with 0 once the host
//!   stopped it or the broker closed the stream, and with 1, after a diagnostic on standard error,
//!   once serving failed.
//!
//! Anything else is a usage error, with status 2.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => {
            println!("runtime-adapter-guest: {}", runtime_adapter::contract());
            ExitCode::SUCCESS
        }
        [command] if command == "serve" => serve(),
        _ => {
            eprintln!("usage: runtime-adapter-guest [serve]");
            ExitCode::from(2)
        }
    }
}

/// Serves the session on standard input and output.
#[cfg(target_os = "linux")]
fn serve() -> ExitCode {
    let stdio = Stdio {
        input: std::io::stdin().lock(),
        output: std::io::stdout().lock(),
    };
    match runtime_adapter::guest::serve(stdio) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("runtime-adapter-guest: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Standard input and output as one stream, which carries nothing but the session's records.
#[cfg(target_os = "linux")]
struct Stdio {
    /// Standard input, from the broker.
    input: std::io::StdinLock<'static>,
    /// Standard output, to the broker.
    output: std::io::StdoutLock<'static>,
}

#[cfg(target_os = "linux")]
impl std::io::Read for Stdio {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buffer)
    }
}

#[cfg(target_os = "linux")]
impl std::io::Write for Stdio {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.output.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

// The guest agent runs only in Linux guests. Elsewhere this stub fails, so that every target still
// builds with every feature.
#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("runtime-adapter-guest runs only in Linux guests");
    ExitCode::FAILURE
}
