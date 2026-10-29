//! What a browser sign-in can fail with, typed so a CLI can branch on it
//! (fall back to its `--stdin` path when no Chrome is installed, stay quiet
//! on its own cancel) and converted into the family's `CliError` with `?`.

use std::fmt;

use pk_cli_core::CliError;

/// A browser sign-in failure. Messages name labels, hosts and cookie names,
/// never a value. Converts into [`CliError`] with the family's exit codes:
/// [`TimedOut`](BrowserError::TimedOut) and
/// [`WindowClosed`](BrowserError::WindowClosed) are exit 3 (the owner did
/// not finish signing in), [`Handler`](BrowserError::Handler) is the
/// handler's own error, and everything else is exit 1.
#[derive(Debug)]
#[non_exhaustive]
pub enum BrowserError {
    /// A [`Capture`](crate::Capture), [`HostRule`](crate::HostRule) or
    /// start URL is malformed: a bug in the calling CLI, caught before
    /// Chrome starts where possible.
    Invalid(String),
    /// No Chrome binary: none at the standard paths, or `<PREFIX>_CHROME`
    /// points at nothing. A CLI can fall back to its manual path on this.
    ChromeNotFound(String),
    /// Chrome could not be started, or exited or stalled before opening its
    /// debug port, or its profile directory could not be made.
    Launch(String),
    /// Ctrl-C, or [`cancel`](crate::cancel), during the sign-in.
    Canceled,
    /// The sign-in did not finish within the timeout.
    TimedOut(String),
    /// The owner closed the window before the sign-in finished.
    WindowClosed(String),
    /// The error the [`Browser::wait_each`](crate::Browser::wait_each)
    /// handler returned, passed through unchanged.
    Handler(CliError),
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrowserError::Invalid(m)
            | BrowserError::ChromeNotFound(m)
            | BrowserError::Launch(m)
            | BrowserError::TimedOut(m)
            | BrowserError::WindowClosed(m) => f.write_str(m),
            BrowserError::Canceled => f.write_str("browser login canceled"),
            BrowserError::Handler(e) => write!(f, "{e}"),
        }
    }
}

impl BrowserError {
    /// The family exit code this error converts to (see the type's docs).
    pub fn exit_code(&self) -> i32 {
        match self {
            BrowserError::TimedOut(_) | BrowserError::WindowClosed(_) => 3,
            BrowserError::Handler(e) => e.exit_code(),
            _ => 1,
        }
    }
}

impl std::error::Error for BrowserError {}

impl From<BrowserError> for CliError {
    fn from(e: BrowserError) -> Self {
        match e {
            BrowserError::TimedOut(m) | BrowserError::WindowClosed(m) => CliError::Auth(m),
            BrowserError::Handler(e) => e,
            other => CliError::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_the_family_exit_codes() {
        let cases: Vec<(BrowserError, i32, &str)> = vec![
            (
                BrowserError::Invalid("invalid capture: x".into()),
                1,
                "invalid capture: x",
            ),
            (
                BrowserError::ChromeNotFound("no Chrome found".into()),
                1,
                "no Chrome found",
            ),
            (
                BrowserError::Launch("launching x: y".into()),
                1,
                "launching x: y",
            ),
            (BrowserError::Canceled, 1, "browser login canceled"),
            (BrowserError::TimedOut("timed out".into()), 3, "timed out"),
            (BrowserError::WindowClosed("closed".into()), 3, "closed"),
            (
                BrowserError::Handler(CliError::Usage("bad session".into())),
                2,
                "bad session",
            ),
        ];
        for (err, code, message) in cases {
            assert_eq!(err.to_string(), message);
            assert_eq!(err.exit_code(), code, "{message}");
            let cli = CliError::from(err);
            assert_eq!(cli.exit_code(), code, "{message}");
            assert_eq!(cli.to_string(), message);
        }
    }
}
