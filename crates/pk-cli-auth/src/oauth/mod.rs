//! OAuth 2 sign-in for a native CLI: the authorization-code grant with PKCE
//! S256 and `state`, the redirect received on a loopback listener, the code
//! exchange, the refresh grant, and dynamic client registration (RFC 6749,
//! 7636, 8252, 7591; RFC 9207's `iss` when a server sends it). DESIGN.md
//! §2.4.
//!
//! This is mechanism. Which provider, which endpoints (from its metadata),
//! which scopes, which extra parameters (SMART's `aud`, RFC 8707's
//! `resource`), whether to register a client, and where the tokens live are
//! the CLI's, or a profile crate's.
//!
//! - [`AuthorizationCodeFlow`] binds [`LoopbackListener`] on `127.0.0.1` (a
//!   port the system picks, path `/callback`), draws the PKCE verifier and the
//!   `state`, and builds the address; [`PendingAuthorization`] waits for the
//!   code and builds the exchange. The listener answers only `GET /callback`
//!   with its own `Host`, on a page that holds nothing from the request.
//! - [`OAuthClient`] posts the grants and registrations over
//!   `pk_cli_core::transport::Transport`, and maps a refusal onto the family's
//!   exit codes ([`refusal`]): a spent grant or a refused client is 3, a 429,
//!   a 5xx or an unavailable server is 5.
//! - [`TokenSet`] is the one keychain item a CLI keeps
//!   (`CredentialStore::{get_json, set_json}`). A renewal keeps what the answer
//!   leaves out (a refresh token a server does not rotate, the scope, SMART's
//!   `patient`).
//! - Every endpoint must be `https`, or `http` to this machine for a test
//!   server; anything else is refused before a request is built, as
//!   [`CliError::Upstream`] since the address comes from a provider.
//!
//! A CLI that signs in this way reports `method: "browser-session"` in
//! `auth-status/v1`: a person has to be at a browser, as for a captured
//! session.
//!
//! ```no_run
//! use pk_cli_auth::oauth::{self, AuthorizationCodeFlow, ClientAuth, OAuthClient, TokenSet};
//! use pk_cli_auth::refresh_lock::RefreshLock;
//! use pk_cli_core::{block_on, CliError};
//! # fn store(_: &TokenSet) -> Result<(), CliError> { Ok(()) }
//! # fn now() -> u64 { 0 }
//! let lock = RefreshLock::for_binary("example")?;
//! lock.before_signin()?;
//! let pending = AuthorizationCodeFlow::new("example", "https://auth.example.test/authorize", "client-1")
//!     .scope("openid offline_access")
//!     .start()?;
//! eprintln!("Sign in at: {}", pending.url());
//! oauth::open_browser(pending.url());
//! // Say on the CLI's own output when a stray answer is refused, so a person
//! // who declined is not left waiting out the timeout.
//! let code = pending.wait_for_code_with(oauth::DEFAULT_TIMEOUT, |reason| {
//!     eprintln!("{reason}. Still waiting; press Ctrl-C to start again.")
//! })?;
//! let client = OAuthClient::new(pk_cli_http::BlockingTransport::new()?, "example", "0.1.0");
//! let answer = block_on(client.token(
//!     "https://auth.example.test/token",
//!     &ClientAuth::public("client-1"),
//!     &pending.token_request(&code),
//! ))?;
//! let tokens = TokenSet::from_response(answer, now(), None);
//! lock.store_signin(|| store(&tokens))?;
//! # Ok::<(), CliError>(())
//! ```
//!
//! A renewal holds [`crate::refresh_lock::RefreshLock`] from the read of the
//! stored set to the write of the new one, and posts nothing when another
//! process renewed meanwhile:
//!
//! ```no_run
//! # use pk_cli_auth::oauth::{ClientAuth, OAuthClient, TokenRequest, TokenSet};
//! # use pk_cli_auth::refresh_lock::RefreshLock;
//! # use pk_cli_core::{block_on, CliError};
//! # fn read() -> Result<TokenSet, CliError> { unimplemented!() }
//! # fn write(_: &TokenSet) -> Result<(), CliError> { Ok(()) }
//! # fn now() -> u64 { 0 }
//! # fn renew<T: pk_cli_core::transport::Transport>(client: &OAuthClient<T>, refused_access_token: &str) -> Result<TokenSet, CliError> {
//! RefreshLock::for_binary("example")?.renew(
//!     read,
//!     |latest| latest.access_token == refused_access_token,
//!     |latest| {
//!         let refresh = latest.refresh_token().ok_or_else(|| {
//!             CliError::Auth("no refresh token is stored. Sign in again".into())
//!         })?;
//!         let answer = block_on(client.token(
//!             "https://auth.example.test/token",
//!             &ClientAuth::public("client-1"),
//!             &TokenRequest::refresh(&refresh),
//!         ))?;
//!         let fresh = TokenSet::from_response(answer, now(), Some(&latest));
//!         write(&fresh)?;
//!         Ok(fresh)
//!     },
//! )
//! # }
//! ```

mod authorize;
mod client;
mod flow;
mod grant;
mod loopback;
mod pkce;
mod register;

pub use authorize::{code_from_callback, open_browser, AuthorizationRequest};
pub use client::OAuthClient;
pub use flow::{AuthorizationCodeFlow, PendingAuthorization};
pub use grant::{refusal, ClientAuth, TokenRequest, TokenResponse, TokenSet};
pub use loopback::{LoopbackListener, CALLBACK_PATH, DEFAULT_TIMEOUT};
pub use pkce::{random_token, s256_challenge, Pkce, S256};
pub use register::{ClientMetadata, RegisteredClient};

use pk_cli_core::transport::{require_secure_url, HttpRequest};
use pk_cli_core::CliError;

/// Refuse an endpoint a credential must not travel to. `what` names it in the
/// message, which shows the scheme and host only.
fn secure_endpoint(endpoint: &str, what: &str) -> Result<(), CliError> {
    require_secure_url(endpoint).map_err(|_| {
        CliError::Upstream(format!(
            "refusing the {what} endpoint {}: only https is allowed, or http to this machine",
            HttpRequest::get(endpoint).origin()
        ))
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
