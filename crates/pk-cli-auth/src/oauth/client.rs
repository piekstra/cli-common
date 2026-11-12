//! The two POSTs a sign-in makes, over the family's transport seam.

use pk_cli_core::transport::{HttpRequest, Transport};
use pk_cli_core::CliError;
use pk_cli_secrets::Secret;

use super::grant::{refusal, ClientAuth, TokenRequest, TokenResponse};
use super::register::{ClientMetadata, RegisteredClient};
use super::{now_unix, secure_endpoint};

/// Token and registration requests over a [`Transport`], so the same client
/// runs in the binary (`pk_cli_http::BlockingTransport`, driven with
/// `pk_cli_core::block_on`) and anywhere else a transport does. Every request
/// carries `User-Agent: <binary>/<version>`.
#[derive(Debug, Clone)]
pub struct OAuthClient<T> {
    transport: T,
    binary: String,
    version: String,
}

impl<T: Transport> OAuthClient<T> {
    pub fn new(transport: T, binary: impl Into<String>, version: impl Into<String>) -> Self {
        OAuthClient {
            transport,
            binary: binary.into(),
            version: version.into(),
        }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Post `request` to the token endpoint. A refusal maps through
    /// [`super::refusal`]; a 2xx body without an access token is
    /// [`CliError::Upstream`].
    pub async fn token(
        &self,
        endpoint: &str,
        client: &ClientAuth,
        request: &TokenRequest,
    ) -> Result<TokenResponse, CliError> {
        let http = request
            .to_http(endpoint, client)?
            .user_agent(&self.binary, &self.version);
        let origin = http.origin();
        let response = self.transport.send(http).await?;
        if !response.is_success() {
            return Err(refusal("token", &origin, &response, now_unix() as i64));
        }
        TokenResponse::parse(&response.body)
    }

    /// Register a client (RFC 7591 §3.1). `initial_access_token` is the bearer
    /// a provider that limits registration hands out; an open endpoint takes
    /// none.
    pub async fn register(
        &self,
        endpoint: &str,
        metadata: &ClientMetadata,
        initial_access_token: Option<&Secret>,
    ) -> Result<RegisteredClient, CliError> {
        secure_endpoint(endpoint, "registration")?;
        let body = serde_json::to_string(metadata)
            .map_err(|e| CliError::Other(format!("serializing the client metadata: {e}")))?;
        let mut http = HttpRequest::new("POST", endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .user_agent(&self.binary, &self.version)
            .body(body);
        if let Some(token) = initial_access_token {
            http = http.header("Authorization", format!("Bearer {}", token.expose()));
        }
        let origin = http.origin();
        let response = self.transport.send(http).await?;
        if !response.is_success() {
            return Err(refusal(
                "registration",
                &origin,
                &response,
                now_unix() as i64,
            ));
        }
        let client: RegisteredClient = serde_json::from_str(&response.body).map_err(|_| {
            CliError::Upstream(format!(
                "the registration endpoint at {origin} answered with no client_id"
            ))
        })?;
        if client.client_id.is_empty() {
            return Err(CliError::Upstream(format!(
                "the registration endpoint at {origin} issued an empty client_id"
            )));
        }
        Ok(client)
    }
}
