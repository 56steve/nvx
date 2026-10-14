//! Exit statuses and failures shared by the guest programs.

use std::process::ExitCode;

/// Process exit statuses of the guest programs.
///
/// A program exits with [`Success`](Self::Success), or with the status that its
/// [`CommandError`] selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ExitStatus {
    /// The program completed.
    Success = 0,
    /// An operation failed.
    Failure = 1,
    /// The command line was invalid.
    Usage = 2,
}

/// Typed failure of a guest program, which selects its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandError {
    /// The command line is invalid. A program with a usage message has printed it.
    Usage,
    /// An operation failed. A program that reports failures has printed why.
    Operation,
}

impl From<Result<(), CommandError>> for ExitStatus {
    fn from(result: Result<(), CommandError>) -> Self {
        match result {
            Ok(()) => Self::Success,
            Err(CommandError::Usage) => Self::Usage,
            Err(CommandError::Operation) => Self::Failure,
        }
    }
}

impl From<ExitStatus> for ExitCode {
    fn from(status: ExitStatus) -> Self {
        Self::from(status as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_select_the_documented_statuses() {
        let cases = [
            (Ok(()), 0),
            (Err(CommandError::Operation), 1),
            (Err(CommandError::Usage), 2),
        ];
        for (result, code) in cases {
            assert_eq!(ExitStatus::from(result) as u8, code, "{result:?}");
        }
    }
}
