//! Google's error answers, read into the family's exit codes.

use pk_cli_core::{CliError, Reason};
use serde_json::Value;

/// What a Google API refusal said: `{"error": {"code", "message",
/// "errors": [{"reason"}]}}`, or a body that is not that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoogleError {
    pub status: u16,
    pub message: String,
    pub reasons: Vec<String>,
}

impl GoogleError {
    pub fn parse(status: u16, body: &[u8]) -> Self {
        let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let error = value.get("error");
        // OAuth endpoints answer `{"error": "invalid_grant", "error_description"}`.
        if let Some(code) = error.and_then(Value::as_str) {
            let description = value
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("");
            return GoogleError {
                status,
                message: printable(&format!("{code}: {description}")),
                reasons: vec![code.to_string()],
            };
        }
        let message = error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(printable)
            .unwrap_or_else(|| format!("HTTP {status}"));
        let reasons = error
            .and_then(|e| e.get("errors"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|e| e.get("reason").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        GoogleError {
            status,
            message,
            reasons,
        }
    }

    pub fn has_reason(&self, reason: &str) -> bool {
        self.reasons.iter().any(|r| r == reason)
    }

    /// A rate limit Google asks clients to back off from and retry:
    /// HTTP 429, or 403 with a rate-limit reason.
    pub fn is_rate_limit(&self) -> bool {
        self.status == 429
            || (self.status == 403
                && (self.has_reason("rateLimitExceeded")
                    || self.has_reason("userRateLimitExceeded")))
    }

    /// The family's error for a refusal that will not go away by retrying.
    /// `what` names the call ("reading inbox/a.pdf"); `relogin` is the
    /// command that signs the profile in again.
    pub fn into_cli(self, what: &str, relogin: &str) -> CliError {
        let text = format!(
            "{what}: Google Drive answered {}: {}",
            self.status, self.message
        );
        match self.status {
            401 => CliError::Auth(text)
                .because(Reason::CredentialRefused)
                .with_remedy(format!("sign in again with `{relogin}`")),
            403 if self.has_reason("storageQuotaExceeded") => CliError::Upstream(text)
                .with_remedy("free space in the Drive account, or buy more storage"),
            403 if self.has_reason("insufficientPermissions")
                || self.has_reason("insufficientScopes")
                || self.has_reason("ACCESS_TOKEN_SCOPE_INSUFFICIENT") =>
            {
                CliError::Auth(text)
                    .because(Reason::Forbidden)
                    .with_remedy(format!(
                        "the profile's sign-in lacks a scope this needs: sign in again with `{relogin}` and allow every permission asked"
                    ))
            }
            403 => CliError::Auth(text).because(Reason::Forbidden),
            404 => CliError::NotFound(text),
            429 => CliError::Upstream(text),
            400 => CliError::Upstream(text).because(Reason::BadRequest),
            s if s >= 500 => CliError::Upstream(text),
            _ => CliError::Upstream(text),
        }
    }
}

/// Text safe to show: control characters dropped, at most 300 characters.
fn printable(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() > 300 {
        let cut: String = clean.chars().take(300).collect();
        format!("{cut}…")
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_and_refusals_read_as_the_family_codes() {
        let body = include_bytes!("../../tests/fixtures/google/error-user-rate-limit.json");
        let e = GoogleError::parse(403, body);
        assert!(e.is_rate_limit());
        assert_eq!(e.reasons, vec!["userRateLimitExceeded"]);

        let body =
            include_bytes!("../../tests/fixtures/google/error-insufficient-permissions.json");
        let e = GoogleError::parse(403, body);
        assert!(!e.is_rate_limit());
        let err = e.into_cli("listing inbox", "example drive login --profile p");
        assert_eq!(err.exit_code(), 3);
        assert!(err
            .remedy()
            .unwrap()
            .contains("example drive login --profile p"));

        let e = GoogleError::parse(400, br#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#);
        assert_eq!(e.reasons, vec!["invalid_grant"]);
        assert!(e.message.contains("expired or revoked"));

        let e = GoogleError::parse(502, b"<html>bad gateway</html>");
        assert_eq!(e.message, "HTTP 502");
        assert_eq!(e.into_cli("x", "y").exit_code(), 5);
        assert_eq!(
            GoogleError::parse(404, b"{}")
                .into_cli("x", "y")
                .exit_code(),
            4
        );
        let quota = GoogleError {
            status: 403,
            message: "The user's Drive storage quota has been exceeded.".into(),
            reasons: vec!["storageQuotaExceeded".into()],
        };
        assert_eq!(quota.into_cli("x", "y").exit_code(), 5);
    }
}
