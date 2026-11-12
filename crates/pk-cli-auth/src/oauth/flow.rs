//! One sign-in from start to code: PKCE, `state`, the listener and the URL,
//! kept together so they cannot drift apart.

use std::time::Duration;

use pk_cli_core::CliError;
use pk_cli_secrets::Secret;

use super::authorize::AuthorizationRequest;
use super::grant::TokenRequest;
use super::loopback::LoopbackListener;
use super::pkce::{random_token, Pkce};

/// What a CLI knows before a sign-in: where to send the person, as whom, and
/// what to ask for.
#[derive(Debug, Clone)]
pub struct AuthorizationCodeFlow {
    app: String,
    authorization_endpoint: String,
    client_id: String,
    scope: Option<String>,
    params: Vec<(String, String)>,
    ports: Vec<u16>,
    issuer: Option<String>,
}

impl AuthorizationCodeFlow {
    /// `app` names the CLI on the page the browser shows at the end.
    pub fn new(
        app: impl Into<String>,
        authorization_endpoint: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        AuthorizationCodeFlow {
            app: app.into(),
            authorization_endpoint: authorization_endpoint.into(),
            client_id: client_id.into(),
            scope: None,
            params: Vec::new(),
            ports: Vec::new(),
            issuer: None,
        }
    }

    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// An extra authorization parameter (SMART's `aud`, RFC 8707's `resource`).
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((name.into(), value.into()));
        self
    }

    /// Listen on a fixed port instead of one the system picks.
    pub fn port(self, port: u16) -> Self {
        self.ports(&[port])
    }

    /// Listen on the first free one of these ports, for a provider that
    /// matches the redirect exactly against the ones registered.
    pub fn ports(mut self, ports: &[u16]) -> Self {
        self.ports = ports.to_vec();
        self
    }

    /// The issuer the answer's `iss` must name when it has one (RFC 9207).
    pub fn issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Bind the listener and build the address to open. Nothing has been sent
    /// anywhere yet.
    pub fn start(&self) -> Result<PendingAuthorization, CliError> {
        let listener = LoopbackListener::bind_first(&self.app, &self.ports)?;
        let pkce = Pkce::generate()?;
        let state = random_token(32)?;
        let redirect_uri = listener.redirect_uri();
        let mut request = AuthorizationRequest::new(
            &self.authorization_endpoint,
            &self.client_id,
            &redirect_uri,
            &pkce,
            &state,
        );
        if let Some(scope) = &self.scope {
            request = request.scope(scope);
        }
        for (name, value) in &self.params {
            request = request.param(name, value);
        }
        let url = request.url()?;
        Ok(PendingAuthorization {
            listener,
            pkce,
            state: Secret::new(state),
            redirect_uri,
            url,
            issuer: self.issuer.clone(),
        })
    }
}

/// A sign-in under way: the browser has somewhere to come back to.
pub struct PendingAuthorization {
    listener: LoopbackListener,
    pkce: Pkce,
    state: Secret,
    redirect_uri: String,
    url: String,
    issuer: Option<String>,
}

impl PendingAuthorization {
    /// The address to open (`super::open_browser`) and to print, so a person
    /// can paste it when no browser opens.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the code ([`LoopbackListener::wait_for_code`]).
    pub fn wait_for_code(&self, timeout: Duration) -> Result<Secret, CliError> {
        self.listener
            .wait_for_code(self.state.expose(), self.issuer.as_deref(), timeout)
    }

    /// The exchange for `code`, with this sign-in's redirect and verifier.
    pub fn token_request(&self, code: &Secret) -> TokenRequest {
        TokenRequest::authorization_code(code, &self.redirect_uri, &self.pkce)
    }
}

impl std::fmt::Debug for PendingAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAuthorization")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}
