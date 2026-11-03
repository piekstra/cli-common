//! Application errors mapped to a stable exit-code contract so scripts and
//! agents can branch on `$?` (SPEC v1 §1.5) and, in `--json` mode, on the
//! `error.code` slug.
//!
//! The variant is the exit code. A caller that answers differently by cause
//! (an MCP tool naming the one command that fixes it, a retry that only a lapsed
//! session earns) reads [`CliError::reason`], which a producer sets with
//! [`CliError::because`]. It never reads the message, which is for people and gets
//! reworded. [`CliError::with_remedy`] keeps what to do next apart from what went
//! wrong, so a tool can give its own remedy in place of a terminal's.
//!
//! ```
//! use pk_cli_core::{CliError, Reason};
//!
//! let e = CliError::Auth("the session lapsed".into())
//!     .because(Reason::CredentialRefused)
//!     .with_remedy("run `svc auth login`");
//! assert_eq!(e.exit_code(), 3);
//! assert_eq!(e.reason(), Some(Reason::CredentialRefused));
//! assert_eq!(e.message(), "the session lapsed");
//! assert_eq!(e.to_string(), "the session lapsed \u{2014} run `svc auth login`");
//! ```

use std::fmt;

use serde_json::json;

/// Errors every family CLI maps onto the shared exit-code contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// Bad usage / missing required input. Exit 2 (clap's convention).
    Usage(String),
    /// Authentication required, or credential invalid/expired. Exit 3.
    Auth(String),
    /// Nothing matched (unknown account, empty history, etc.). Exit 4.
    NotFound(String),
    /// Network or upstream provider failure (non-2xx, portal down,
    /// scrape mismatch, rate limit). Exit 5.
    Upstream(String),
    /// A mutation needed confirmation but ran non-interactively without
    /// `--force`. Exit 6.
    ConfirmationRequired(String),
    /// OS keychain failure. Exit 1.
    Keychain(String),
    /// Anything else. Exit 1.
    Other(String),
    /// One of the above with its cause or remedy typed. Built by
    /// [`CliError::because`] and [`CliError::with_remedy`]; it keeps the exit code,
    /// slug and display of the error it wraps. A pattern such as `CliError::Auth(_)`
    /// does not see through it: match on [`CliError::base`], or ask
    /// [`CliError::is_auth`].
    Detailed(Box<Detail>),
}

/// Why a failure happened, for a caller that answers differently by cause.
/// Carried by any kind of error; the kind still sets the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// Nothing is stored to use: never signed in, or signed out.
    NoCredential,
    /// The provider refused the credential, or it has lapsed: a 401, a dead refresh
    /// token. A new sign-in fixes it.
    CredentialRefused,
    /// The credential was fine and this request is not allowed: a 403. A new sign-in
    /// may not fix it.
    Forbidden,
    /// A credential is stored and this build cannot read it.
    CredentialUnreadable,
    /// It has lapsed, and this CLI's own setting says not to renew it.
    RenewalOff,
    /// The provider refused the request's arguments: a 400.
    BadRequest,
    /// The refusal is of this client and not the session: a bot check or a block at
    /// the provider's edge. A new sign-in may not fix it, and costs one.
    ClientBlocked,
    /// A cause only this CLI has. Name it once as a `const`, and compare with that.
    Provider(&'static str),
}

/// What [`CliError::Detailed`] holds. Read it through `CliError`'s methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    base: CliError,
    reason: Option<Reason>,
    remedy: Option<String>,
}

impl CliError {
    /// Type this error's cause, keeping its kind, message and any remedy.
    pub fn because(self, reason: Reason) -> Self {
        let mut detail = self.detail();
        detail.reason = Some(reason);
        Self::Detailed(Box::new(detail))
    }

    /// What to do next, apart from the message. `Display` and `--json` print it after
    /// the message; a tool error leaves it out and gives its own.
    pub fn with_remedy(self, remedy: impl Into<String>) -> Self {
        let mut detail = self.detail();
        detail.remedy = Some(remedy.into());
        Self::Detailed(Box::new(detail))
    }

    fn detail(self) -> Detail {
        match self {
            Self::Detailed(detail) => *detail,
            base => Detail {
                base,
                reason: None,
                remedy: None,
            },
        }
    }

    /// The cause a producer typed with [`Self::because`].
    pub fn reason(&self) -> Option<Reason> {
        match self {
            Self::Detailed(detail) => detail.reason,
            _ => None,
        }
    }

    /// The remedy a producer kept apart with [`Self::with_remedy`].
    pub fn remedy(&self) -> Option<&str> {
        match self {
            Self::Detailed(detail) => detail.remedy.as_deref(),
            _ => None,
        }
    }

    /// The kind under any detail: always one of the plain variants.
    pub fn base(&self) -> &CliError {
        match self {
            Self::Detailed(detail) => &detail.base,
            plain => plain,
        }
    }

    /// As [`Self::base`], by value: the reason and the remedy are dropped.
    pub fn into_base(self) -> CliError {
        match self {
            Self::Detailed(detail) => detail.base,
            plain => plain,
        }
    }

    /// Exit 3, with or without detail.
    pub fn is_auth(&self) -> bool {
        matches!(self.base(), Self::Auth(_))
    }

    /// What went wrong, as the producer wrote it: without `Display`'s prefix for a
    /// terminal, and without a remedy kept apart.
    pub fn message(&self) -> &str {
        match self.base() {
            Self::Usage(m)
            | Self::Auth(m)
            | Self::NotFound(m)
            | Self::Upstream(m)
            | Self::ConfirmationRequired(m)
            | Self::Keychain(m)
            | Self::Other(m) => m,
            Self::Detailed(_) => unreachable!("a base is never detailed"),
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            CliError::Auth(_) => 3,
            CliError::NotFound(_) => 4,
            CliError::Upstream(_) => 5,
            CliError::ConfirmationRequired(_) => 6,
            CliError::Keychain(_) | CliError::Other(_) => 1,
            CliError::Detailed(detail) => detail.base.exit_code(),
        }
    }

    /// Stable machine-readable slug for `--json` error output.
    pub fn code(&self) -> &'static str {
        match self {
            CliError::Usage(_) => "usage",
            CliError::Auth(_) => "auth",
            CliError::NotFound(_) => "not_found",
            CliError::Upstream(_) => "upstream",
            CliError::ConfirmationRequired(_) => "confirmation_required",
            CliError::Keychain(_) => "keychain",
            CliError::Other(_) => "other",
            CliError::Detailed(detail) => detail.base.code(),
        }
    }

    /// The `{"error": {"code", "message"}}` DTO emitted on stdout in `--json`
    /// mode (SPEC v1 §1.4).
    pub fn to_json(&self) -> serde_json::Value {
        json!({ "error": { "code": self.code(), "message": self.to_string() } })
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(m) => write!(f, "{m}"),
            CliError::Auth(m) => write!(f, "{m}"),
            CliError::NotFound(m) => write!(f, "not found: {m}"),
            CliError::Upstream(m) => write!(f, "network/upstream error: {m}"),
            CliError::ConfirmationRequired(m) => write!(f, "confirmation required: {m}"),
            CliError::Keychain(m) => write!(f, "keychain error: {m}"),
            CliError::Other(m) => write!(f, "{m}"),
            CliError::Detailed(detail) => match &detail.remedy {
                Some(remedy) => write!(f, "{} \u{2014} {remedy}", detail.base),
                None => write!(f, "{}", detail.base),
            },
        }
    }
}

impl std::error::Error for CliError {}

#[cfg(feature = "reqwest")]
impl From<reqwest::Error> for CliError {
    /// reqwest's own message quotes the whole URL, and a path or query can carry
    /// an account id or a token, so this keeps only the origin. A redirect policy's
    /// reason is only in the error's source, and it holds no URL.
    fn from(e: reqwest::Error) -> Self {
        let origin = e.url().map(|url| url.origin().ascii_serialization());
        let reason = e
            .is_redirect()
            .then(|| std::error::Error::source(&e).map(ToString::to_string))
            .flatten();
        let mut message = e.without_url().to_string();
        if let Some(origin) = origin {
            message = format!("{message} ({origin})");
        }
        if let Some(reason) = reason {
            message = format!("{message}: {reason}");
        }
        CliError::Upstream(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable() {
        assert_eq!(CliError::Usage(String::new()).exit_code(), 2);
        assert_eq!(CliError::Auth(String::new()).exit_code(), 3);
        assert_eq!(CliError::NotFound(String::new()).exit_code(), 4);
        assert_eq!(CliError::Upstream(String::new()).exit_code(), 5);
        assert_eq!(CliError::ConfirmationRequired(String::new()).exit_code(), 6);
        assert_eq!(CliError::Keychain(String::new()).exit_code(), 1);
        assert_eq!(CliError::Other(String::new()).exit_code(), 1);
    }

    #[test]
    fn error_json_shape() {
        let e = CliError::Auth("login required".into());
        let v = e.to_json();
        assert_eq!(v["error"]["code"], "auth");
        assert_eq!(v["error"]["message"], "login required");
    }

    fn every_kind() -> Vec<CliError> {
        vec![
            CliError::Usage("m".into()),
            CliError::Auth("m".into()),
            CliError::NotFound("m".into()),
            CliError::Upstream("m".into()),
            CliError::ConfirmationRequired("m".into()),
            CliError::Keychain("m".into()),
            CliError::Other("m".into()),
        ]
    }

    /// Scripts branch on `$?` and `error.code`, and a person reads the text: typing a
    /// cause changes none of them.
    #[test]
    fn a_reason_keeps_the_exit_code_slug_and_text_of_every_kind() {
        for plain in every_kind() {
            let typed = plain.clone().because(Reason::Forbidden);
            assert_eq!(typed.exit_code(), plain.exit_code(), "{plain:?}");
            assert_eq!(typed.code(), plain.code(), "{plain:?}");
            assert_eq!(typed.to_string(), plain.to_string(), "{plain:?}");
            assert_eq!(typed.to_json(), plain.to_json(), "{plain:?}");
            assert_eq!(typed.reason(), Some(Reason::Forbidden));
            assert_eq!(typed.base(), &plain);
            assert_eq!(typed.message(), "m", "{plain:?}");
            assert_eq!(plain.reason(), None);
            assert_eq!(plain.remedy(), None);
            assert_eq!(plain.message(), "m");
            assert_eq!(plain.base(), &plain);
        }
    }

    #[test]
    fn a_remedy_is_printed_for_a_terminal_and_kept_apart_from_the_message() {
        let e = CliError::NotFound("no account".into()).with_remedy("list them first");
        assert_eq!(
            e.to_string(),
            "not found: no account \u{2014} list them first"
        );
        assert_eq!(e.to_json()["error"]["message"], e.to_string());
        assert_eq!(e.to_json()["error"]["code"], "not_found");
        assert_eq!(e.message(), "no account");
        assert_eq!(e.remedy(), Some("list them first"));
        assert_eq!(e.reason(), None);
    }

    /// Detail on detail folds into one, so `base` is always a plain variant.
    #[test]
    fn a_reason_and_a_remedy_share_one_layer_in_either_order() {
        let a = CliError::Auth("m".into())
            .because(Reason::RenewalOff)
            .with_remedy("r");
        let b = CliError::Auth("m".into())
            .with_remedy("r")
            .because(Reason::RenewalOff);
        assert_eq!(a, b);
        assert_eq!(a.base(), &CliError::Auth("m".into()));
        assert_eq!(
            a.clone().because(Reason::NoCredential).reason(),
            Some(Reason::NoCredential)
        );
        assert_eq!(a.with_remedy("s").remedy(), Some("s"));
    }

    #[test]
    fn is_auth_sees_through_detail_and_only_auth_answers_it() {
        for plain in every_kind() {
            let auth = matches!(plain, CliError::Auth(_));
            assert_eq!(plain.is_auth(), auth);
            assert_eq!(plain.because(Reason::BadRequest).is_auth(), auth);
        }
    }

    #[test]
    fn a_cli_s_own_reason_compares_by_its_name() {
        const APP_REFUSED: Reason = Reason::Provider("app_refused");
        let e = CliError::Auth("m".into()).because(APP_REFUSED);
        assert_eq!(e.reason(), Some(APP_REFUSED));
        assert_ne!(e.reason(), Some(Reason::Provider("another")));
    }

    #[test]
    fn into_base_drops_the_detail() {
        let e = CliError::Upstream("m".into())
            .because(Reason::BadRequest)
            .with_remedy("r");
        assert_eq!(e.into_base(), CliError::Upstream("m".into()));
    }

    #[cfg(feature = "reqwest")]
    #[test]
    fn a_network_failure_names_the_origin_and_not_the_rest_of_the_url() {
        // Nothing listens on a port the OS just gave back.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("a free port")
            .port();
        let err = reqwest::blocking::get(format!(
            "http://127.0.0.1:{port}/accounts/acct-xyz?token=dummy-token"
        ))
        .unwrap_err();
        let message = CliError::from(err).to_string();
        assert!(message.contains(&format!("127.0.0.1:{port}")), "{message}");
        for leak in ["accounts", "acct-xyz", "dummy-token"] {
            assert!(!message.contains(leak), "{leak} printed: {message}");
        }
    }
}
