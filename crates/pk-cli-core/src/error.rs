//! Application errors mapped to a stable exit-code contract so scripts and
//! agents can branch on `$?` (SPEC v1 §1.5) and, in `--json` mode, on the
//! `error.code` slug.

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
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            CliError::Auth(_) => 3,
            CliError::NotFound(_) => 4,
            CliError::Upstream(_) => 5,
            CliError::ConfirmationRequired(_) => 6,
            CliError::Keychain(_) | CliError::Other(_) => 1,
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
