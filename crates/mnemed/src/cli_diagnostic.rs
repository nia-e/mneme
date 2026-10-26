//! CLI termination without Rust's default debug formatting or duplicate reports.

use crate::AnyErr;
use std::{fmt, process::ExitCode};

/// A complete failure was already rendered in the command's own format.
/// Keep the underlying error alive until the CLI finishes, including any guards
/// it owns. This marker does not catch panics or turn failures into success.
#[derive(Debug)]
struct ReportedError(AnyErr);

impl fmt::Display for ReportedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for ReportedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub(crate) fn reported(error: impl Into<AnyErr>) -> AnyErr {
    Box::new(ReportedError(error.into()))
}

pub(crate) fn finish(result: Result<(), AnyErr>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if !error.is::<ReportedError>() {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_failure_keeps_its_cause() {
        let error = reported(std::io::Error::other("retained failure"));
        assert!(error.is::<ReportedError>());
        assert_eq!(error.to_string(), "retained failure");
        assert!(error.source().unwrap().is::<std::io::Error>());
    }
}
