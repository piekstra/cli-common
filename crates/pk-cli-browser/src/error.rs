//! What a browser sign-in can fail with, typed so a CLI can branch on it
//! (fall back to its `--stdin` path when no Chrome is installed, stay quiet
//! on its own cancel) and converted into the family's `CliError` with `?`.

use std::fmt;

use pk_cli_core::CliError;

/// A browser sign-in failure. Messages name labels, hosts and cookie names,
/// never a value. Converts into [`CliError`] with the family's exit codes:
/// [`TimedOut`](BrowserError::TimedOut) and
/// [`WindowClosed`](BrowserError::WindowClosed) are exit 3 (the owner did
/// not finish signing in), [`Undecided`](BrowserError::Undecided) is exit 5
/// (the provider never answered clearly), [`Handler`](BrowserError::Handler)
/// is the CLI's own error, and everything else is exit 1.
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
    /// debug port, or a scratch profile directory could not be made.
    Launch(String),
    /// The CLI's kept browser profile needs repair before it can be used
    /// or removed: it is not a private (`0700`) directory of the current
    /// user's (a symlink, another owner, another mode), it could not be
    /// located, created or removed, or its site storage could not be removed
    /// ([`Launch::clear_site_storage`](crate::Launch::clear_site_storage)). The
    /// message names the path and the fix.
    Profile(String),
    /// The CLI's kept browser profile is busy: another sign-in, or a Chrome
    /// still running in it, has it open. Waiting (closing that window) or
    /// `--fresh-profile` gets past it; the message names the path, and the
    /// pid of a Chrome.
    ProfileInUse(String),
    /// Ctrl-C, or [`cancel`](crate::cancel), during the sign-in.
    Canceled,
    /// The sign-in did not finish within the timeout.
    TimedOut(String),
    /// The wait ran out while a confirm hook's read could not decide
    /// ([`Confirm::Undecided`](crate::Confirm::Undecided)).
    Undecided(String),
    /// The owner closed the window before the sign-in finished.
    WindowClosed(String),
    /// The CLI's own error, passed through unchanged: what the
    /// [`Browser::wait_each`](crate::Browser::wait_each) handler returned,
    /// or what a [`Capture::confirm`](crate::Capture::confirm) hook ended
    /// the wait with ([`Confirm::Fail`](crate::Confirm::Fail)).
    Handler(CliError),
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrowserError::Invalid(m)
            | BrowserError::ChromeNotFound(m)
            | BrowserError::Launch(m)
            | BrowserError::Profile(m)
            | BrowserError::ProfileInUse(m)
            | BrowserError::TimedOut(m)
            | BrowserError::Undecided(m)
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
            BrowserError::Undecided(_) => 5,
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
            BrowserError::Undecided(m) => CliError::Upstream(m),
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
            (
                BrowserError::Profile("the browser profile dir x is a symlink".into()),
                1,
                "the browser profile dir x is a symlink",
            ),
            (
                BrowserError::ProfileInUse("a Chrome (pid 7) is still running in x".into()),
                1,
                "a Chrome (pid 7) is still running in x",
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

    #[test]
    fn a_wait_that_ran_out_undecided_is_the_provider_s_exit_code() {
        let err = BrowserError::Undecided("no answer".into());
        assert_eq!(err.to_string(), "no answer");
        assert_eq!(err.exit_code(), 5);
        let cli = CliError::from(err);
        assert!(
            matches!(&cli, CliError::Upstream(m) if m == "no answer"),
            "{cli:?}"
        );
        assert_eq!(cli.exit_code(), 5);
    }
}
