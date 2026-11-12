//! Token requests (RFC 6749 §4.1.3, §6), their answers, and the token set a
//! CLI keeps.

use std::fmt;

use pk_cli_core::transport::{percent_encode, HttpRequest, HttpResponse};
use pk_cli_core::{CliError, Reason};
use pk_cli_secrets::Secret;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroize;

use super::authorize::printable;
use super::pkce::{base64, Alphabet, Pkce};

/// How the client proves who it is to the token endpoint.
pub enum ClientAuth {
    /// A public client (RFC 8252 §8.4): `client_id` in the body and no secret.
    /// PKCE is what binds the code to this process.
    Public { client_id: String },
    /// `client_secret_basic` (RFC 6749 §2.3.1).
    SecretBasic { client_id: String, secret: Secret },
    /// `client_secret_post` (RFC 6749 §2.3.1).
    SecretPost { client_id: String, secret: Secret },
    /// `private_key_jwt` (RFC 7523 §2.2, OpenID Connect Core §9): an assertion
    /// the caller signed with its own key.
    PrivateKeyJwt {
        client_id: String,
        assertion: Secret,
    },
}

impl ClientAuth {
    pub fn public(client_id: impl Into<String>) -> Self {
        ClientAuth::Public {
            client_id: client_id.into(),
        }
    }

    pub fn client_id(&self) -> &str {
        match self {
            ClientAuth::Public { client_id }
            | ClientAuth::SecretBasic { client_id, .. }
            | ClientAuth::SecretPost { client_id, .. }
            | ClientAuth::PrivateKeyJwt { client_id, .. } => client_id,
        }
    }
}

impl fmt::Debug for ClientAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            ClientAuth::Public { .. } => "Public",
            ClientAuth::SecretBasic { .. } => "SecretBasic",
            ClientAuth::SecretPost { .. } => "SecretPost",
            ClientAuth::PrivateKeyJwt { .. } => "PrivateKeyJwt",
        };
        f.debug_struct(kind)
            .field("client_id", &self.client_id())
            .finish_non_exhaustive()
    }
}

/// Body parameters the request or the client authentication sets.
const RESERVED: [&str; 6] = [
    "grant_type",
    "client_id",
    "client_secret",
    "client_assertion",
    "client_assertion_type",
    "scope",
];

const JWT_BEARER_ASSERTION: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// One grant to the token endpoint. Every value is held as a [`Secret`]: a
/// code, a verifier and a refresh token are all credentials.
pub struct TokenRequest {
    grant_type: String,
    scope: Option<String>,
    params: Vec<(String, Secret)>,
}

impl TokenRequest {
    /// Exchange an authorization code (RFC 6749 §4.1.3) with its PKCE verifier.
    /// `redirect_uri` is the one the authorization request sent.
    pub fn authorization_code(code: &Secret, redirect_uri: &str, pkce: &Pkce) -> Self {
        Self::grant("authorization_code")
            .param("code", code.expose())
            .param("redirect_uri", redirect_uri)
            .param("code_verifier", pkce.verifier().expose())
    }

    /// Renew with a refresh token (RFC 6749 §6). Hold
    /// [`crate::refresh_lock::RefreshLock`] around it when the provider rotates
    /// refresh tokens.
    pub fn refresh(refresh_token: &Secret) -> Self {
        Self::grant("refresh_token").param("refresh_token", refresh_token.expose())
    }

    /// Any other grant: an extension grant such as RFC 7523's JWT bearer
    /// (`urn:ietf:params:oauth:grant-type:jwt-bearer` with an `assertion`).
    pub fn grant(grant_type: impl Into<String>) -> Self {
        TokenRequest {
            grant_type: grant_type.into(),
            scope: None,
            params: Vec::new(),
        }
    }

    /// Space-separated scopes. A refresh may only narrow them (RFC 6749 §6).
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// One more body parameter (an RFC 8707 `resource`, a JWT `assertion`).
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((name.into(), Secret::new(value.into())));
        self
    }

    pub fn grant_type(&self) -> &str {
        &self.grant_type
    }

    /// The form POST, before a `User-Agent`. An insecure endpoint is
    /// [`CliError::Upstream`] (it comes from a provider's metadata); a body
    /// parameter that would replace the request's own is [`CliError::Usage`].
    pub fn to_http(&self, endpoint: &str, client: &ClientAuth) -> Result<HttpRequest, CliError> {
        super::secure_endpoint(endpoint, "token")?;
        if let Some((name, _)) = self
            .params
            .iter()
            .find(|(name, _)| RESERVED.contains(&name.as_str()))
        {
            return Err(CliError::Usage(format!(
                "`{name}` is set by the token request itself, not as an extra parameter"
            )));
        }
        let mut form: Vec<(&str, &str)> = vec![("grant_type", &self.grant_type)];
        for (name, value) in &self.params {
            form.push((name, value.expose()));
        }
        if let Some(scope) = &self.scope {
            form.push(("scope", scope));
        }
        let mut request = HttpRequest::new("POST", endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json");
        match client {
            ClientAuth::Public { client_id } => form.push(("client_id", client_id)),
            ClientAuth::SecretPost { client_id, secret } => {
                form.push(("client_id", client_id));
                form.push(("client_secret", secret.expose()));
            }
            ClientAuth::SecretBasic { client_id, secret } => {
                let pair = format!(
                    "{}:{}",
                    percent_encode(client_id),
                    percent_encode(secret.expose())
                );
                request = request.header(
                    "Authorization",
                    format!("Basic {}", base64(pair.as_bytes(), Alphabet::Standard)),
                );
            }
            ClientAuth::PrivateKeyJwt {
                client_id,
                assertion,
            } => {
                form.push(("client_id", client_id));
                form.push(("client_assertion_type", JWT_BEARER_ASSERTION));
                form.push(("client_assertion", assertion.expose()));
            }
        }
        Ok(request.body(form_encode(&form)))
    }
}

impl fmt::Debug for TokenRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.params.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("TokenRequest")
            .field("grant_type", &self.grant_type)
            .field("scope", &self.scope)
            .field("params", &names)
            .finish()
    }
}

/// `application/x-www-form-urlencoded`, every byte but RFC 3986's unreserved
/// set escaped.
pub(crate) fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A successful token endpoint answer (RFC 6749 §5.1).
pub struct TokenResponse {
    pub access_token: Secret,
    /// `Bearer` when the server left it out.
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<Secret>,
    pub scope: Option<String>,
    /// OpenID Connect's ID token, unverified: read it with
    /// [`crate::token::claims`] for display, never to decide access.
    pub id_token: Option<Secret>,
    /// Every other member: SMART's `patient` and `need_patient_banner`, a
    /// provider's own.
    pub extra: Map<String, Value>,
}

impl TokenResponse {
    /// Parse a 2xx body. One with no `access_token` is
    /// [`CliError::Upstream`]: the server answered something else.
    pub fn parse(body: &str) -> Result<Self, CliError> {
        let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(body) else {
            return Err(CliError::Upstream(
                "the token endpoint's answer is not a JSON object".into(),
            ));
        };
        let mut take_string = |name: &str| match fields.remove(name) {
            Some(Value::String(s)) if !s.is_empty() => Some(s),
            _ => None,
        };
        let access_token = take_string("access_token").ok_or_else(|| {
            CliError::Upstream("the token endpoint's answer carried no access_token".into())
        })?;
        let token_type = take_string("token_type").unwrap_or_else(|| "Bearer".into());
        let refresh_token = take_string("refresh_token");
        let scope = take_string("scope");
        let id_token = take_string("id_token");
        let expires_in = match fields.remove("expires_in") {
            Some(Value::Number(n)) => n.as_u64(),
            // Some servers send it as a string.
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        };
        Ok(TokenResponse {
            access_token: Secret::new(access_token),
            token_type,
            expires_in,
            refresh_token: refresh_token.map(Secret::new),
            scope,
            id_token: id_token.map(Secret::new),
            extra: fields,
        })
    }
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &self.refresh_token.is_some())
            .field("scope", &self.scope)
            .field("id_token", &self.id_token.is_some())
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The tokens a CLI keeps, as one keychain item
/// (`CredentialStore::{get_json, set_json}`, DESIGN.md §1.7). `Debug` shows
/// no token; the strings are zeroed on drop.
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. `None` when the server gave no lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Unix seconds: when the newest grant answered.
    pub obtained_at: u64,
    /// The answer's other members, carried across renewals.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl TokenSet {
    /// The set after a grant answered at `now` (Unix seconds). With the set it
    /// renews, `previous`, what the answer leaves out is kept: the refresh
    /// token (a server that does not rotate sends none, RFC 6749 §6), the scope
    /// (§5.1: unchanged when omitted), the ID token, and every extra member a
    /// renewal does not repeat (SMART's `patient`).
    pub fn from_response(response: TokenResponse, now: u64, previous: Option<&TokenSet>) -> Self {
        let mut extra = previous.map(|p| p.extra.clone()).unwrap_or_default();
        extra.extend(response.extra);
        let kept = |new: Option<Secret>, old: Option<&String>| {
            new.map(|s| s.expose().to_string()).or_else(|| old.cloned())
        };
        TokenSet {
            access_token: response.access_token.expose().to_string(),
            token_type: response.token_type,
            refresh_token: kept(
                response.refresh_token,
                previous.and_then(|p| p.refresh_token.as_ref()),
            ),
            expires_at: response.expires_in.map(|s| now.saturating_add(s)),
            scope: response
                .scope
                .or_else(|| previous.and_then(|p| p.scope.clone())),
            id_token: kept(
                response.id_token,
                previous.and_then(|p| p.id_token.as_ref()),
            ),
            obtained_at: now,
            extra,
        }
    }

    /// Whether the access token is past its lifetime at `now`, less
    /// `skew_secs`. With no lifetime known it is not: the server decides.
    pub fn is_expired(&self, now: u64, skew_secs: u64) -> bool {
        self.expires_at
            .is_some_and(|at| at.saturating_sub(skew_secs) <= now)
    }

    /// `expires_at` as RFC 3339, for `auth-status/v1`.
    pub fn expires_at_rfc3339(&self) -> Option<String> {
        self.expires_at
            .map(|at| pk_cli_core::dates::fmt_rfc3339(at as i64))
    }

    pub fn refresh_token(&self) -> Option<Secret> {
        self.refresh_token.as_deref().map(Secret::new)
    }

    /// `Authorization: Bearer <token>`, for a request header.
    pub fn authorization(&self) -> Secret {
        Secret::new(format!("Bearer {}", self.access_token))
    }
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("token_type", &self.token_type)
            .field("refresh_token", &self.refresh_token.is_some())
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("id_token", &self.id_token.is_some())
            .field("obtained_at", &self.obtained_at)
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Drop for TokenSet {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(t) = self.refresh_token.as_mut() {
            t.zeroize();
        }
        if let Some(t) = self.id_token.as_mut() {
            t.zeroize();
        }
    }
}

/// A non-2xx answer from a token or registration endpoint as a family error.
/// `what` names the endpoint ("token", "registration"), `origin` its scheme
/// and host. The message quotes the OAuth `error` slug and its description cut
/// to printable text, never the body otherwise.
///
/// | Answer | Error |
/// |---|---|
/// | 429, 5xx, a 403 page, `temporarily_unavailable` | `Upstream` (exit 5), with any `Retry-After` |
/// | `invalid_grant`, `invalid_client`, `unauthorized_client`, `invalid_token`, a bare 401 | `Auth` (exit 3), `Reason::CredentialRefused` |
/// | `access_denied`, `invalid_scope`, a bare 403 | `Auth` (exit 3), `Reason::Forbidden` |
/// | any other 4xx (`invalid_request`, `unsupported_grant_type`, RFC 7591's `invalid_redirect_uri` and `invalid_client_metadata`) | `Upstream` (exit 5), `Reason::BadRequest` |
/// | a 3xx | `Upstream`: the transport follows no redirects, and a credential is not re-sent |
pub fn refusal(what: &str, origin: &str, response: &HttpResponse, now: i64) -> CliError {
    let status = response.status;
    let parsed: Option<Value> = serde_json::from_str(&response.body).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .map(printable)
    };
    let slug = field("error").unwrap_or_default();
    let detail = match (slug.is_empty(), field("error_description")) {
        (false, Some(d)) => format!(" ({slug}: {d})"),
        (false, None) => format!(" ({slug})"),
        (true, Some(d)) => format!(" ({d})"),
        (true, None) => String::new(),
    };
    let said = format!("the {what} endpoint at {origin} answered HTTP {status}{detail}");
    if (300..400).contains(&status) {
        return CliError::Upstream(format!("{said}: a redirect, which is not followed"));
    }
    if response.inconclusive(now).is_some() || slug == "temporarily_unavailable" {
        let wait = response
            .retry_after(now)
            .map(|d| format!("; it asked to wait {} s", d.as_secs()))
            .unwrap_or_default();
        return CliError::Upstream(format!("{said}{wait}. Try again later"));
    }
    match slug.as_str() {
        "invalid_grant" | "invalid_client" | "unauthorized_client" | "invalid_token" => {
            CliError::Auth(format!(
                "{said}: the grant or this client was refused. Sign in again"
            ))
            .because(Reason::CredentialRefused)
        }
        "access_denied" | "invalid_scope" => {
            CliError::Auth(format!("{said}: the access asked for was refused"))
                .because(Reason::Forbidden)
        }
        "" if status == 401 => {
            CliError::Auth(format!("{said}. Sign in again")).because(Reason::CredentialRefused)
        }
        "" if status == 403 => CliError::Auth(said).because(Reason::Forbidden),
        _ if (400..500).contains(&status) => CliError::Upstream(said).because(Reason::BadRequest),
        _ => CliError::Upstream(said),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkce() -> Pkce {
        Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap()
    }

    #[test]
    fn a_code_exchange_for_a_public_client_is_one_form_post() {
        let request = TokenRequest::authorization_code(
            &Secret::new("code/1"),
            "http://127.0.0.1:5000/callback",
            &pkce(),
        )
        .to_http(
            "https://auth.example.test/token",
            &ClientAuth::public("client-1"),
        )
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://auth.example.test/token");
        assert_eq!(
            request.header_value("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            request.body.as_deref(),
            Some(
                "grant_type=authorization_code&code=code%2F1\
                 &redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2Fcallback\
                 &code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk&client_id=client-1"
            )
        );
        assert!(request.header_value("authorization").is_none());
    }

    #[test]
    fn each_client_authentication_lands_where_its_rfc_puts_it() {
        let refresh = || TokenRequest::refresh(&Secret::new("r")).scope("a b");
        let basic = refresh()
            .to_http(
                "https://auth.example.test/token",
                &ClientAuth::SecretBasic {
                    client_id: "id:1".into(),
                    secret: Secret::new("s ecret"),
                },
            )
            .unwrap();
        // base64("id%3A1:s%20ecret")
        assert_eq!(
            basic.header_value("authorization"),
            Some("Basic aWQlM0ExOnMlMjBlY3JldA==")
        );
        assert_eq!(
            basic.body.as_deref(),
            Some("grant_type=refresh_token&refresh_token=r&scope=a%20b")
        );
        let post = refresh()
            .to_http(
                "https://auth.example.test/token",
                &ClientAuth::SecretPost {
                    client_id: "id".into(),
                    secret: Secret::new("s"),
                },
            )
            .unwrap();
        assert!(post
            .body
            .unwrap()
            .ends_with("&client_id=id&client_secret=s"));
        let jwt = refresh()
            .to_http(
                "https://auth.example.test/token",
                &ClientAuth::PrivateKeyJwt {
                    client_id: "id".into(),
                    assertion: Secret::new("a.b.c"),
                },
            )
            .unwrap();
        assert!(jwt.body.unwrap().ends_with(
            "&client_id=id&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3A\
             client-assertion-type%3Ajwt-bearer&client_assertion=a.b.c"
        ));
    }

    #[test]
    fn a_reserved_parameter_or_insecure_endpoint_is_refused() {
        for name in RESERVED {
            let err = TokenRequest::grant("x")
                .param(name, "v")
                .to_http("https://auth.example.test/token", &ClientAuth::public("c"))
                .unwrap_err();
            assert_eq!(err.exit_code(), 2, "{name}");
        }
        let err = TokenRequest::grant("x")
            .to_http("http://auth.example.test/token", &ClientAuth::public("c"))
            .unwrap_err();
        assert_eq!(err.exit_code(), 5);
    }

    #[test]
    fn debug_shows_no_value() {
        let request = TokenRequest::refresh(&Secret::new("refresh-secret"));
        let client = ClientAuth::SecretBasic {
            client_id: "id".into(),
            secret: Secret::new("client-secret"),
        };
        let shown = format!("{request:?} {client:?}");
        assert!(!shown.contains("secret\""), "{shown}");
        assert!(!shown.contains("refresh-secret") && !shown.contains("client-secret"));
    }

    #[test]
    fn a_renewal_keeps_what_the_answer_leaves_out() {
        let first = TokenResponse::parse(
            r#"{"access_token":"a1","token_type":"Bearer","expires_in":3600,
                "refresh_token":"r1","scope":"s1","id_token":"i1","patient":"p1"}"#,
        )
        .unwrap();
        let first = TokenSet::from_response(first, 1_000, None);
        assert_eq!(first.expires_at, Some(4_600));
        assert_eq!(first.extra.get("patient"), Some(&Value::from("p1")));
        let renewed = TokenResponse::parse(
            r#"{"access_token":"a2","token_type":"bearer","expires_in":"60"}"#,
        )
        .unwrap();
        let renewed = TokenSet::from_response(renewed, 2_000, Some(&first));
        assert_eq!(renewed.access_token, "a2");
        assert_eq!(renewed.token_type, "bearer");
        assert_eq!(renewed.refresh_token.as_deref(), Some("r1"));
        assert_eq!(renewed.scope.as_deref(), Some("s1"));
        assert_eq!(renewed.id_token.as_deref(), Some("i1"));
        assert_eq!(renewed.expires_at, Some(2_060));
        assert_eq!(renewed.obtained_at, 2_000);
        assert_eq!(renewed.extra.get("patient"), Some(&Value::from("p1")));
        let rotated =
            TokenResponse::parse(r#"{"access_token":"a3","refresh_token":"r2"}"#).unwrap();
        let rotated = TokenSet::from_response(rotated, 3_000, Some(&renewed));
        assert_eq!(rotated.refresh_token.as_deref(), Some("r2"));
        assert_eq!(rotated.expires_at, None);
        assert_eq!(rotated.token_type, "Bearer");
    }

    #[test]
    fn expiry_allows_skew_and_an_unknown_lifetime_is_not_expired() {
        let set = |expires_at| TokenSet {
            access_token: "a".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            expires_at,
            scope: None,
            id_token: None,
            obtained_at: 0,
            extra: Map::new(),
        };
        assert!(!set(Some(1_000)).is_expired(900, 60));
        assert!(set(Some(1_000)).is_expired(940, 60));
        assert!(set(Some(1_000)).is_expired(1_000, 0));
        assert!(!set(None).is_expired(u64::MAX, 0));
        assert_eq!(
            set(Some(0)).expires_at_rfc3339().as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
    }

    #[test]
    fn a_body_that_is_not_a_grant_is_upstream() {
        for body in [
            "",
            "[]",
            "<html>",
            r#"{"token_type":"Bearer"}"#,
            r#"{"access_token":""}"#,
        ] {
            assert_eq!(
                TokenResponse::parse(body).unwrap_err().exit_code(),
                5,
                "{body}"
            );
        }
    }

    #[test]
    fn the_token_set_round_trips_and_debug_hides_it() {
        let set = TokenSet::from_response(
            TokenResponse::parse(r#"{"access_token":"tok-a","refresh_token":"tok-r"}"#).unwrap(),
            5,
            None,
        );
        let json = serde_json::to_string(&set).unwrap();
        let back: TokenSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back.access_token, "tok-a");
        assert_eq!(back.authorization().expose(), "Bearer tok-a");
        let shown = format!("{set:?}");
        assert!(!shown.contains("tok-"), "{shown}");
    }

    fn answer(status: u16, body: &str) -> HttpResponse {
        HttpResponse::new(status, body)
    }

    #[test]
    fn refusals_map_onto_the_exit_codes() {
        let origin = "https://auth.example.test";
        for (status, body, code, reason) in [
            (
                400,
                r#"{"error":"invalid_grant"}"#,
                3,
                Some(Reason::CredentialRefused),
            ),
            (
                401,
                r#"{"error":"invalid_client"}"#,
                3,
                Some(Reason::CredentialRefused),
            ),
            (
                400,
                r#"{"error":"unauthorized_client"}"#,
                3,
                Some(Reason::CredentialRefused),
            ),
            (
                400,
                r#"{"error":"invalid_scope"}"#,
                3,
                Some(Reason::Forbidden),
            ),
            (
                400,
                r#"{"error":"access_denied"}"#,
                3,
                Some(Reason::Forbidden),
            ),
            (401, "", 3, Some(Reason::CredentialRefused)),
            (403, "{}", 3, Some(Reason::Forbidden)),
            (
                400,
                r#"{"error":"invalid_request"}"#,
                5,
                Some(Reason::BadRequest),
            ),
            (
                400,
                r#"{"error":"unsupported_grant_type"}"#,
                5,
                Some(Reason::BadRequest),
            ),
            (
                400,
                r#"{"error":"invalid_redirect_uri"}"#,
                5,
                Some(Reason::BadRequest),
            ),
            (503, r#"{"error":"temporarily_unavailable"}"#, 5, None),
            (400, r#"{"error":"temporarily_unavailable"}"#, 5, None),
            (429, "", 5, None),
            (500, "oops", 5, None),
            (302, "", 5, None),
        ] {
            let err = refusal("token", origin, &answer(status, body), 0);
            assert_eq!(err.exit_code(), code, "{status} {body}");
            assert_eq!(err.reason(), reason, "{status} {body}");
            assert!(err.to_string().contains(origin), "{err}");
        }
    }

    #[test]
    fn a_refusal_quotes_the_slug_and_description_but_nothing_else() {
        let body = r#"{"error":"invalid_grant","error_description":"code\u001b[31m expired","access_token":"leak"}"#;
        let message = refusal("token", "https://a.test", &answer(400, body), 0).to_string();
        assert!(
            message.contains("(invalid_grant: code [31m expired)"),
            "{message}"
        );
        assert!(
            !message.contains("leak") && !message.contains('\u{1b}'),
            "{message}"
        );
        let slow = answer(429, "").header("retry-after", "30");
        let message = refusal("token", "https://a.test", &slow, 0).to_string();
        assert!(message.contains("asked to wait 30 s"), "{message}");
    }
}
