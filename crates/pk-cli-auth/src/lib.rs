//! Standard auth surface (SPEC v1 §1.2/§1.4): the `auth-status/v1` DTO, the
//! shared `auth login` / `auth set-credential` argument structs, and text
//! rendering for status. Each CLI supplies its own credential verification;
//! this crate owns the shapes so drivers can treat every CLI identically.

pub mod otp;
pub mod reauth;
pub mod refresh_lock;
pub mod token;

use pk_cli_core::output;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use pk_cli_secrets::SecretSourceArgs;

/// How a CLI authenticates to its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    /// Username + password verified against the provider.
    Password,
    /// A session captured from a logged-in browser (bot-protected portals).
    BrowserSession,
    /// No credential needed (anonymous/guest reads).
    None,
}

/// The canonical `auth status --json` DTO (`auth-status/v1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatus {
    pub schema: String,
    /// Whether this CLI needs a credential at all.
    pub required: bool,
    /// Usable now: credential present (and session valid, where checked).
    pub authenticated: bool,
    pub method: AuthMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_in_keychain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_valid: Option<bool>,
    /// RFC 3339, when the session's lifetime is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Whether the CLI keeps a browser profile between `auth login --browser`
    /// sign-ins (DESIGN.md §2.2). A kept profile, not a signed-in one. Left
    /// out by a CLI with no browser sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_profile: Option<bool>,
}

impl AuthStatus {
    pub fn new(required: bool, authenticated: bool, method: AuthMethod) -> Self {
        AuthStatus {
            schema: "auth-status/v1".into(),
            required,
            authenticated,
            method,
            username: None,
            account: None,
            credential_in_keychain: None,
            session_valid: None,
            expires_at: None,
            browser_profile: None,
        }
    }

    /// For credential-free CLIs: `required: false`, always usable.
    pub fn not_required() -> Self {
        AuthStatus::new(false, true, AuthMethod::None)
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Standard text rendering (`Key: value` block).
    pub fn render(&self) {
        println!(
            "Authenticated: {}",
            if self.authenticated { "yes" } else { "no" }
        );
        let method = match self.method {
            AuthMethod::Password => "password",
            AuthMethod::BrowserSession => "browser-session",
            AuthMethod::None => "none (no credential needed)",
        };
        println!("Method:        {method}");
        if let Some(u) = &self.username {
            println!("Username:      {u}");
        }
        if let Some(a) = &self.account {
            println!("Account:       {a}");
        }
        if let Some(k) = self.credential_in_keychain {
            println!(
                "Keychain:      {}",
                if k { "credential stored" } else { "empty" }
            );
        }
        if let Some(s) = self.session_valid {
            println!(
                "Session:       {}",
                if s { "valid" } else { "invalid/expired" }
            );
        }
        if let Some(e) = &self.expires_at {
            println!("Expires:       {e}");
        }
        if let Some(b) = self.browser_profile {
            println!(
                "Browser:       {}",
                if b { "profile kept" } else { "no profile kept" }
            );
        }
    }

    /// Emit per the output contract: DTO in json mode, block otherwise.
    pub fn emit(&self, json_mode: bool) {
        if json_mode {
            output::json(&self.to_json());
        } else {
            self.render();
        }
    }
}

/// The standard `auth login` flags. Secrets enter via `--stdin` /
/// `--from-env` (or an interactive no-echo prompt) — never argv.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct LoginArgs {
    #[command(flatten)]
    pub source: SecretSourceArgs,
    /// Store the credential without a live verification check.
    #[arg(long)]
    pub no_verify: bool,
    /// Replace an existing stored credential instead of failing.
    #[arg(long)]
    pub overwrite: bool,
    /// Never prompt; fail if a required input is missing.
    #[arg(long)]
    pub non_interactive: bool,
}

/// The standard `auth set-credential` flags (raw keychain write for
/// rotation / headless setup; requires an explicit source).
#[derive(clap::Args, Debug, Default, Clone)]
pub struct SetCredentialArgs {
    #[command(flatten)]
    pub source: SecretSourceArgs,
    /// Replace an existing entry instead of failing.
    #[arg(long)]
    pub overwrite: bool,
}

/// The standard `auth logout` flags.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct LogoutArgs {
    /// Also clear the stored credential and saved identity (username/account).
    #[arg(long)]
    pub forget: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_dto_shape() {
        let mut s = AuthStatus::new(true, true, AuthMethod::Password);
        s.username = Some("user@example.com".into());
        s.credential_in_keychain = Some(true);
        let v = s.to_json();
        assert_eq!(v["schema"], "auth-status/v1");
        assert_eq!(v["method"], "password");
        assert_eq!(v["credential_in_keychain"], true);
        assert!(v.get("expires_at").is_none());
        assert!(v.get("browser_profile").is_none());
    }

    /// An optional field added within `auth-status/v1`: left off the wire by
    /// a CLI that does not set it, and missing from an older payload.
    #[test]
    fn the_browser_profile_is_reported_only_when_set() {
        let mut s = AuthStatus::new(true, true, AuthMethod::BrowserSession);
        s.browser_profile = Some(false);
        assert_eq!(s.to_json()["browser_profile"], false);
        s.browser_profile = Some(true);
        assert_eq!(s.to_json()["browser_profile"], true);

        let older = serde_json::json!({
            "schema": "auth-status/v1",
            "required": true,
            "authenticated": true,
            "method": "browser-session",
        });
        let read: AuthStatus = serde_json::from_value(older).unwrap();
        assert_eq!(read.browser_profile, None);
    }

    #[test]
    fn not_required_is_authenticated() {
        let s = AuthStatus::not_required();
        assert!(s.authenticated);
        assert!(!s.required);
        assert_eq!(s.to_json()["method"], "none");
    }
}
