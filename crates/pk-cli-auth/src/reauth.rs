//! Recovering from a lapsed session without making the user re-run the command.
//!
//! Portal sessions expire on the provider's schedule, not the user's. A CLI
//! that caches one will sooner or later run a command against a dead session
//! and exit 3 — correct, but tedious when the credential needed to get a fresh
//! session is already sitting in the keychain.
//!
//! [`with_reauth`] closes that gap generically. It owns the *mechanism* —
//! notice an auth failure, re-establish once, retry once, never loop — and
//! knows nothing about how any particular provider logs in. The caller
//! supplies that as a closure, so a CLI whose login can't be automated (an
//! interactive second factor, say) simply returns an error from it and gets
//! the original behaviour back.
//!
//! # Why the rails are here rather than in each CLI
//!
//! Retry-on-auth looks like three lines until you enumerate the ways it goes
//! wrong: retrying a non-auth error, retrying forever against a provider that
//! answers 401 for a *permissions* problem, hammering a login endpoint into a
//! lockout, or masking "your password is wrong" behind "your session expired".
//! Those rails are identical for every CLI in the family, so they live once,
//! here, tested — rather than being re-derived per repo.

use pk_cli_core::CliError;

/// Run `op`; if it fails because the session has lapsed, re-authenticate with
/// `reauth` and run `op` exactly once more.
///
/// Only [`CliError::Auth`] triggers recovery — an upstream error, a not-found,
/// or a usage error is returned untouched, because retrying those either
/// changes nothing or repeats a side effect.
///
/// # Guarantees
///
/// - `reauth` runs **at most once**, and only after a real auth failure. There
///   is no loop and no backoff to tune, so a provider that always answers
///   `Auth` costs exactly two attempts, not an escalating series of login
///   requests that trips a lockout.
/// - If `reauth` itself fails, **its** error is returned, not the original.
///   "the portal rejected your password" is what the user must act on;
///   reporting "session expired" there would send them to re-run the login
///   that just failed.
/// - If the retry fails again, that second error is returned as-is — a session
///   that is dead immediately after a successful login is a real condition
///   (a revoked account, a provider outage) and must not be disguised.
///
/// # Idempotency
///
/// `op` may run twice, so it must be safe to run twice. Reads always are.
/// **Do not wrap a mutation in this** unless the provider deduplicates it: a
/// payment that succeeded but whose *response* looked like an auth failure
/// would be submitted a second time.
///
/// # Example
///
/// ```no_run
/// # use pk_cli_core::CliError;
/// # use pk_cli_auth::reauth::with_reauth;
/// # fn fetch() -> Result<String, CliError> { Ok(String::new()) }
/// # fn log_in() -> Result<(), CliError> { Ok(()) }
/// let body = with_reauth(fetch, log_in)?;
/// # Ok::<(), CliError>(())
/// ```
pub fn with_reauth<T>(
    op: impl Fn() -> Result<T, CliError>,
    reauth: impl FnOnce() -> Result<(), CliError>,
) -> Result<T, CliError> {
    match op() {
        Err(CliError::Auth(_)) => {
            // The login error, when there is one, is the actionable message.
            reauth()?;
            op()
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_successful_op_never_reauthenticates() {
        let reauthed = Cell::new(false);
        let out = with_reauth(
            || Ok::<_, CliError>("data"),
            || {
                reauthed.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out, "data");
        assert!(!reauthed.get(), "must not log in when nothing was wrong");
    }

    #[test]
    fn an_auth_failure_reauthenticates_once_and_retries() {
        let attempts = Cell::new(0);
        let reauths = Cell::new(0);
        let out = with_reauth(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(CliError::Auth("session expired".into()))
                } else {
                    Ok("fresh data")
                }
            },
            || {
                reauths.set(reauths.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out, "fresh data");
        assert_eq!(attempts.get(), 2, "op runs twice: once, then once more");
        assert_eq!(reauths.get(), 1);
    }

    /// The rail that keeps a broken provider from turning into a login storm.
    #[test]
    fn a_persistent_auth_failure_stops_after_one_retry() {
        let attempts = Cell::new(0);
        let reauths = Cell::new(0);
        let err = with_reauth(
            || {
                attempts.set(attempts.get() + 1);
                Err::<(), _>(CliError::Auth("still expired".into()))
            },
            || {
                reauths.set(reauths.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 2, "no loop");
        assert_eq!(reauths.get(), 1, "exactly one login attempt");
        // The second failure surfaces as itself, not as the first one.
        assert!(matches!(err, CliError::Auth(m) if m == "still expired"));
    }

    #[test]
    fn non_auth_errors_are_returned_untouched() {
        for original in [
            CliError::Upstream("portal down".into()),
            CliError::NotFound("no such payment".into()),
            CliError::Usage("bad flag".into()),
            CliError::ConfirmationRequired("needs --force".into()),
        ] {
            let attempts = Cell::new(0);
            let reauthed = Cell::new(false);
            let code = original.exit_code();
            let err = with_reauth(
                || {
                    attempts.set(attempts.get() + 1);
                    Err::<(), _>(clone_err(&original))
                },
                || {
                    reauthed.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(attempts.get(), 1, "must not retry a non-auth error");
            assert!(!reauthed.get(), "must not log in for a non-auth error");
            assert_eq!(err.exit_code(), code);
        }
    }

    /// A failed login is the actionable message; surfacing the stale "session
    /// expired" instead would tell the user to re-run what just failed.
    #[test]
    fn a_failed_reauth_reports_its_own_error() {
        let attempts = Cell::new(0);
        let err = with_reauth(
            || {
                attempts.set(attempts.get() + 1);
                Err::<(), _>(CliError::Auth("session expired".into()))
            },
            || Err(CliError::Auth("invalid username or password".into())),
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 1, "no retry once recovery is impossible");
        assert!(matches!(err, CliError::Auth(m) if m.contains("invalid username")));
    }

    /// A CLI that cannot log in unattended (an interactive second factor) opts
    /// out by returning an error, and keeps the plain exit-3 behaviour.
    #[test]
    fn opting_out_preserves_the_original_contract() {
        let err = with_reauth(
            || {
                Err::<(), _>(CliError::Auth(
                    "session expired — run `x auth login`".into(),
                ))
            },
            || {
                Err(CliError::Auth(
                    "two-factor code required — run `x auth login`".into(),
                ))
            },
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 3);
    }

    fn clone_err(e: &CliError) -> CliError {
        match e {
            CliError::Usage(m) => CliError::Usage(m.clone()),
            CliError::Auth(m) => CliError::Auth(m.clone()),
            CliError::NotFound(m) => CliError::NotFound(m.clone()),
            CliError::Upstream(m) => CliError::Upstream(m.clone()),
            CliError::ConfirmationRequired(m) => CliError::ConfirmationRequired(m.clone()),
            CliError::Keychain(m) => CliError::Keychain(m.clone()),
            CliError::Other(m) => CliError::Other(m.clone()),
        }
    }
}
