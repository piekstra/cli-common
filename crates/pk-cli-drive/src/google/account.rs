//! A Drive profile's access token: read from the vault, renewed with the
//! refresh token under the CLI's refresh lock, so two processes never spend
//! one refresh twice.

use std::sync::Arc;

use pk_cli_auth::oauth::{ClientAuth, OAuthClient, TokenRequest, TokenSet};
use pk_cli_auth::refresh_lock::RefreshLock;
use pk_cli_auth::token::DEFAULT_SKEW_SECS;
use pk_cli_auth::vault::Vault;
use pk_cli_config::profile::ProfileName;
use pk_cli_core::{block_on, CliError, Reason};
use pk_cli_secrets::Secret;

use super::api::Tokens;
use super::client::{DesktopClient, StoredClient};
use super::endpoints::Endpoints;
use super::profiles::{client_entry, token_entry};

/// One Drive profile's sign-in, as the Drive API's token source.
pub struct Account {
    pub(crate) bin: String,
    pub(crate) version: String,
    /// How the CLI mounts the Drive commands (`medrec drive`), for remedies.
    pub(crate) command: String,
    /// The flag naming a Drive profile on those commands (`--profile`).
    pub(crate) profile_flag: String,
    pub(crate) profile: ProfileName,
    pub(crate) vault: Arc<Vault>,
    pub(crate) lock: RefreshLock,
    pub(crate) endpoints: Endpoints,
}

impl Account {
    pub fn profile(&self) -> &ProfileName {
        &self.profile
    }

    /// `<cli> drive login --profile <name>`.
    pub fn login_command(&self) -> String {
        format!(
            "{} login {} {}",
            self.command, self.profile_flag, self.profile
        )
    }

    /// The profile's OAuth client; exit 2 (`no_client`) when none is stored.
    pub fn client(&self) -> Result<DesktopClient, CliError> {
        match self
            .vault
            .get_json::<StoredClient>(&client_entry(&self.profile))?
        {
            Some(stored) => Ok(DesktopClient::from_stored(&stored)),
            None => Err(CliError::Usage(format!(
                "Drive profile `{}` has no OAuth client",
                self.profile
            ))
            .because(crate::reason::NO_CLIENT)
            .with_remedy(format!(
                "import the Desktop app client's JSON with `{} client import {} {} --file <PATH>`, then `{}`",
                self.command,
                self.profile_flag,
                self.profile,
                self.login_command()
            ))),
        }
    }

    /// The stored token set; exit 3 when the profile is not signed in.
    pub fn stored(&self) -> Result<TokenSet, CliError> {
        self.vault
            .get_json::<TokenSet>(&token_entry(&self.profile))?
            .ok_or_else(|| {
                CliError::Auth(format!("Drive profile `{}` is not signed in", self.profile))
                    .because(Reason::NoCredential)
                    .with_remedy(format!("sign in with `{}`", self.login_command()))
            })
    }

    /// Spend the refresh token for a new access token and store the set.
    fn refresh(&self, latest: TokenSet) -> Result<TokenSet, CliError> {
        let refresh = latest.refresh_token().ok_or_else(|| {
            CliError::Auth(format!(
                "Drive profile `{}` has no refresh token",
                self.profile
            ))
            .because(Reason::NoCredential)
            .with_remedy(format!("sign in again with `{}`", self.login_command()))
        })?;
        let client = self.client()?;
        let auth: ClientAuth = client.auth();
        let oauth = OAuthClient::new(
            pk_cli_http::BlockingTransport::new()?,
            self.bin.clone(),
            self.version.clone(),
        );
        let answer = block_on(oauth.token(
            &self.endpoints.token,
            &auth,
            &TokenRequest::refresh(&refresh),
        ))
        .map_err(|e| {
            if e.is_auth() {
                e.with_remedy(format!(
                    "Google refused the stored sign-in (revoked, or unused for six months): sign in again with `{}`",
                    self.login_command()
                ))
            } else {
                e
            }
        })?;
        let fresh = TokenSet::from_response(answer, now(), Some(&latest));
        self.vault.put_json(&token_entry(&self.profile), &fresh)?;
        Ok(fresh)
    }
}

impl Tokens for Account {
    fn access(&self) -> Result<Secret, CliError> {
        let stored = self.stored()?;
        if !stored.is_expired(now(), DEFAULT_SKEW_SECS) {
            return Ok(Secret::new(stored.access_token.as_str()));
        }
        let fresh = self.lock.renew(
            || self.stored(),
            |latest| latest.is_expired(now(), DEFAULT_SKEW_SECS),
            |latest| self.refresh(latest),
        )?;
        Ok(Secret::new(fresh.access_token.as_str()))
    }

    fn renew(&self, refused: &Secret) -> Result<Secret, CliError> {
        let fresh = self.lock.renew(
            || self.stored(),
            |latest| latest.access_token == refused.expose(),
            |latest| self.refresh(latest),
        )?;
        Ok(Secret::new(fresh.access_token.as_str()))
    }

    fn relogin(&self) -> String {
        self.login_command()
    }
}

/// A fixed token, for a test against the fake server or a one-off script.
pub(crate) struct FixedToken(pub String);

impl Tokens for FixedToken {
    fn access(&self) -> Result<Secret, CliError> {
        Ok(Secret::new(self.0.as_str()))
    }
    fn renew(&self, _refused: &Secret) -> Result<Secret, CliError> {
        Err(CliError::Auth("the fixed token was refused".into()))
    }
    fn relogin(&self) -> String {
        "(a fixed token)".into()
    }
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
