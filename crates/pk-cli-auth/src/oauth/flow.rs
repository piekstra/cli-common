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
    redirect_uri: Option<String>,
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
            redirect_uri: None,
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

    /// Send `uri` as the redirect instead of the listener's
    /// `http://127.0.0.1:<port>/callback`, for a provider that accepts only
    /// `https` redirects in production. A page at `uri` sends the browser on to
    /// the listener with the answer's query unchanged. The listener checks
    /// `state` and `iss` as for a direct redirect, and the exchange names `uri`
    /// with the PKCE verifier, which never leaves this machine. The page knows
    /// one port, so [`ports`](Self::ports) must name it. `start` refuses, as
    /// exit 2, a `uri` that is not `https`, one with a fragment (RFC 6749
    /// §3.1.2), or no fixed port.
    pub fn redirect_uri(mut self, uri: impl Into<String>) -> Self {
        self.redirect_uri = Some(uri.into());
        self
    }

    /// Bind the listener and build the address to open. Nothing has been sent
    /// anywhere yet.
    pub fn start(&self) -> Result<PendingAuthorization, CliError> {
        if let Some(uri) = &self.redirect_uri {
            check_forwarded(uri, &self.ports)?;
        }
        let listener = LoopbackListener::bind_first(&self.app, &self.ports)?;
        let pkce = Pkce::generate()?;
        let state = random_token(32)?;
        let redirect_uri = self
            .redirect_uri
            .clone()
            .unwrap_or_else(|| listener.redirect_uri());
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

/// A redirect a page forwards to the listener: `https`, with a host, no
/// fragment, and a fixed port to forward to. The message names no part of
/// `uri`, which the CLI's configuration holds.
fn check_forwarded(uri: &str, ports: &[u16]) -> Result<(), CliError> {
    let host = uri
        .split_once("://")
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("https"))
        .map(|(_, rest)| rest.split(['/', '?', '#', '\\']).next().unwrap_or_default());
    if !host.is_some_and(|h| !h.is_empty()) {
        return Err(CliError::Usage(
            "a redirect URI other than the listener's must be https, with a host".into(),
        ));
    }
    if uri.contains('#') {
        return Err(CliError::Usage(
            "the redirect URI carries a fragment, which RFC 6749 §3.1.2 forbids".into(),
        ));
    }
    if ports.is_empty() {
        return Err(CliError::Usage(
            "a forwarded redirect needs the fixed port its page forwards to".into(),
        ));
    }
    Ok(())
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

    /// The redirect the address sent and the exchange names: the listener's,
    /// or the page [`AuthorizationCodeFlow::redirect_uri`] set.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Where the answer must arrive: `http://127.0.0.1:<port>/callback`.
    pub fn listener_uri(&self) -> String {
        self.listener.redirect_uri()
    }

    /// Wait for the code ([`LoopbackListener::wait_for_code`]).
    pub fn wait_for_code(&self, timeout: Duration) -> Result<Secret, CliError> {
        self.wait_for_code_with(timeout, |_| {})
    }

    /// Wait for the code, told of each refused answer
    /// ([`LoopbackListener::wait_for_code_with`]).
    pub fn wait_for_code_with(
        &self,
        timeout: Duration,
        on_refused: impl FnMut(&str),
    ) -> Result<Secret, CliError> {
        self.listener.wait_for_code_with(
            self.state.expose(),
            self.issuer.as_deref(),
            timeout,
            on_refused,
        )
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
