//! Where the Google backend sends its requests.

use pk_cli_core::CliError;

/// Google's OAuth and Drive endpoints, or a fake Drive server's on this
/// machine for a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub revoke: String,
    /// The Drive API v3 base: `…/drive/v3`.
    pub api: String,
    /// The Drive upload base: `…/upload/drive/v3`.
    pub upload: String,
}

impl Endpoints {
    /// Google's own. The client JSON's `auth_uri` and `token_uri` are never
    /// used: a planted file cannot send the client secret elsewhere.
    pub fn google() -> Self {
        Endpoints {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            revoke: "https://oauth2.googleapis.com/revoke".into(),
            api: "https://www.googleapis.com/drive/v3".into(),
            upload: "https://www.googleapis.com/upload/drive/v3".into(),
        }
    }

    /// A fake Drive server at `base` (`http://127.0.0.1:<port>`), laid out
    /// as [`crate::google::fake::FakeDrive`] serves. Anything but plain
    /// `http` to `127.0.0.1` or `localhost` is refused (exit 2), so a token
    /// can never travel to another machine this way.
    pub fn local(base: &str) -> Result<Self, CliError> {
        let base = base.trim_end_matches('/');
        let host = base
            .strip_prefix("http://")
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .and_then(|authority| authority.rsplit_once(':'))
            .filter(|(host, port)| {
                matches!(*host, "127.0.0.1" | "localhost") && port.parse::<u16>().is_ok()
            });
        if host.is_none() || base.contains('@') || base.matches('/').count() != 2 {
            return Err(CliError::Usage(
                "a test endpoint must be http://127.0.0.1:<port> (or localhost) with no path"
                    .into(),
            ));
        }
        Ok(Endpoints {
            authorize: format!("{base}/o/oauth2/v2/auth"),
            token: format!("{base}/token"),
            revoke: format!("{base}/revoke"),
            api: format!("{base}/drive/v3"),
            upload: format!("{base}/upload/drive/v3"),
        })
    }

    /// Google's, unless `<PREFIX>_DRIVE_TEST_ENDPOINT` names a fake server
    /// on this machine ([`Endpoints::local`]). Test-only: a CLI's
    /// black-box tests point the binary at a [`crate::google::fake::FakeDrive`]
    /// with it.
    pub fn from_env(bin: &str) -> Result<Self, CliError> {
        let var = format!("{}_DRIVE_TEST_ENDPOINT", crate::cache::env_prefix(bin));
        match std::env::var(&var) {
            Ok(base) if !base.is_empty() => Endpoints::local(&base),
            _ => Ok(Endpoints::google()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_loopback_http_base_is_a_test_endpoint() {
        let e = Endpoints::local("http://127.0.0.1:4321/").unwrap();
        assert_eq!(e.api, "http://127.0.0.1:4321/drive/v3");
        assert_eq!(e.upload, "http://127.0.0.1:4321/upload/drive/v3");
        assert!(Endpoints::local("http://localhost:80").is_ok());
        for bad in [
            "https://127.0.0.1:4321",
            "http://example.test:4321",
            "http://127.0.0.1",
            "http://127.0.0.1:4321/x",
            "http://user@127.0.0.1:4321",
            "http://127.0.0.1.example.test:4321",
        ] {
            assert_eq!(Endpoints::local(bad).unwrap_err().exit_code(), 2, "{bad}");
        }
    }
}
