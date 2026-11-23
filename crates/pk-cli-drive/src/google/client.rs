//! The OAuth client a Drive profile signs in with: Google's "Desktop app"
//! client JSON, imported from a file or stdin and kept in the vault.

use pk_cli_auth::oauth::ClientAuth;
use pk_cli_core::CliError;
use pk_cli_secrets::Secret;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

/// A Desktop-app OAuth client: its id, its secret (Google's installed-app
/// secret, which Google says is not confidential, kept as one anyway) and
/// the Cloud project it belongs to.
pub struct DesktopClient {
    client_id: String,
    secret: Secret,
    project_id: Option<String>,
}

/// The vault form. Never printed: `Debug` is not derived.
#[derive(Serialize, Deserialize)]
pub(crate) struct StoredClient {
    client_id: String,
    client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
}

impl Drop for StoredClient {
    fn drop(&mut self) {
        self.client_secret.zeroize();
    }
}

impl DesktopClient {
    /// Read Google's client JSON (`{"installed": {...}}`, the file the Cloud
    /// console's "Download JSON" gives for a Desktop app). A Web client's
    /// JSON is refused: it cannot use a loopback redirect on any port.
    /// Nothing from the file is echoed in an error.
    pub fn from_json(text: &str) -> Result<Self, CliError> {
        let refuse = |why: &str| {
            CliError::Usage(format!("the OAuth client JSON {why}"))
                .with_remedy("download it again from the Google Cloud console: APIs & Services, Credentials, the Desktop app client, Download JSON")
        };
        let value: Value =
            serde_json::from_str(text.trim()).map_err(|_| refuse("is not valid JSON"))?;
        if value.get("web").is_some() {
            return Err(refuse(
                "is a Web application client; Drive sign-in from a CLI needs a Desktop app client",
            ));
        }
        let installed = value.get("installed").ok_or_else(|| {
            refuse("has no \"installed\" client (it must be a Desktop app client)")
        })?;
        let field = |name: &str| {
            installed
                .get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let client_id = field("client_id").ok_or_else(|| refuse("has no client_id"))?;
        if !client_id.ends_with(".apps.googleusercontent.com") {
            return Err(refuse(
                "has a client_id that is not a Google OAuth client id",
            ));
        }
        let secret = field("client_secret").ok_or_else(|| refuse("has no client_secret"))?;
        Ok(DesktopClient {
            client_id: client_id.to_string(),
            secret: Secret::new(secret),
            project_id: field("project_id").map(str::to_string),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    /// How status and lists name the client without printing its id whole:
    /// `…` and the id's last four characters before
    /// `.apps.googleusercontent.com`.
    pub fn id_hint(&self) -> String {
        id_hint(&self.client_id)
    }

    /// The token endpoint's client authentication: Google's installed-app
    /// flow sends the secret in the body.
    pub(crate) fn auth(&self) -> ClientAuth {
        ClientAuth::SecretPost {
            client_id: self.client_id.clone(),
            secret: Secret::new(self.secret.expose()),
        }
    }

    pub(crate) fn to_stored(&self) -> StoredClient {
        StoredClient {
            client_id: self.client_id.clone(),
            client_secret: self.secret.expose().to_string(),
            project_id: self.project_id.clone(),
        }
    }

    pub(crate) fn from_stored(stored: &StoredClient) -> Self {
        DesktopClient {
            client_id: stored.client_id.clone(),
            secret: Secret::new(stored.client_secret.as_str()),
            project_id: stored.project_id.clone(),
        }
    }
}

impl std::fmt::Debug for DesktopClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopClient")
            .field("client_id", &self.id_hint())
            .field("project_id", &self.project_id)
            .finish_non_exhaustive()
    }
}

/// `…abcd` for a client id `<number>-<…abcd>.apps.googleusercontent.com`.
pub fn id_hint(client_id: &str) -> String {
    let stem = client_id
        .strip_suffix(".apps.googleusercontent.com")
        .unwrap_or(client_id);
    let tail: String = stem
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: &str = include_str!("../../tests/fixtures/google/desktop-client.json");

    #[test]
    fn a_desktop_client_json_reads_and_never_shows_its_secret() {
        let c = DesktopClient::from_json(DESKTOP).unwrap();
        assert_eq!(
            c.client_id(),
            "000000000000-exampleexampleexampleexample0abc.apps.googleusercontent.com"
        );
        assert_eq!(c.project_id(), Some("example-project"));
        assert_eq!(c.id_hint(), "…0abc");
        let shown = format!("{c:?}");
        assert!(!shown.contains("EXAMPLE-not-a-real-secret"), "{shown}");
        assert!(!shown.contains("exampleexample"), "{shown}");
        match c.auth() {
            ClientAuth::SecretPost { client_id, secret } => {
                assert_eq!(client_id, c.client_id());
                assert_eq!(secret.expose(), "GOCSPX-EXAMPLE-not-a-real-secret");
            }
            other => panic!("{other:?}"),
        }
        let round = DesktopClient::from_stored(&c.to_stored());
        assert_eq!(round.client_id(), c.client_id());
    }

    #[test]
    fn a_web_client_or_a_broken_file_is_refused_without_echoing_it() {
        let web =
            r#"{"web":{"client_id":"1-x.apps.googleusercontent.com","client_secret":"s3cr3t"}}"#;
        let err = DesktopClient::from_json(web).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("Desktop app"), "{err}");
        assert!(!err.to_string().contains("s3cr3t"));
        for bad in [
            "not json s3cr3t",
            r#"{"installed":{"client_secret":"s3cr3t"}}"#,
            r#"{"installed":{"client_id":"1-x.apps.googleusercontent.com"}}"#,
            r#"{"installed":{"client_id":"evil.example.test","client_secret":"s3cr3t"}}"#,
        ] {
            let err = DesktopClient::from_json(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}");
            assert!(!err.to_string().contains("s3cr3t"), "{err}");
        }
    }
}
