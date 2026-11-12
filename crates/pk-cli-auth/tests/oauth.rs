//! The OAuth sign-in end to end, offline: a browser stand-in on the loopback
//! listener, and a token endpoint stand-in behind the transport seam.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use pk_cli_auth::oauth::{
    AuthorizationCodeFlow, ClientAuth, ClientMetadata, OAuthClient, TokenRequest, TokenSet,
};
use pk_cli_auth::token::claims;
use pk_cli_core::transport::{HttpRequest, HttpResponse, Transport};
use pk_cli_core::{block_on, CliError, Reason};
use pk_cli_secrets::Secret;
use serde_json::Value;

const TOKEN_CODE_FILE: &str = include_str!("fixtures/oauth/token-code.json");
const TOKEN_REFRESH: &str = include_str!("fixtures/oauth/token-refresh.json");
const TOKEN_INVALID_GRANT: &str = include_str!("fixtures/oauth/token-error-invalid-grant.json");
const REGISTRATION: &str = include_str!("fixtures/oauth/registration.json");

/// The code exchange's answer with an unsigned `id_token` naming the example
/// patient, built here so no JWT-shaped string is committed.
fn token_code() -> String {
    let segment = |json: &str| base64url(json.as_bytes());
    let id_token = format!(
        "{}.{}.",
        segment(r#"{"alg":"none"}"#),
        segment(r#"{"fhirUser":"Patient/example-patient-1"}"#)
    );
    let mut answer: Value = serde_json::from_str(TOKEN_CODE_FILE).unwrap();
    answer["id_token"] = Value::from(id_token);
    answer.to_string()
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(TABLE[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Answers each request with the next canned response and keeps the requests.
struct Canned {
    answers: RefCell<Vec<HttpResponse>>,
    seen: RefCell<Vec<HttpRequest>>,
}

impl Canned {
    fn new(answers: Vec<HttpResponse>) -> Self {
        Canned {
            answers: RefCell::new(answers.into_iter().rev().collect()),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for Canned {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        self.seen.borrow_mut().push(request);
        Ok(self
            .answers
            .borrow_mut()
            .pop()
            .expect("an answer per request"))
    }
}

fn json(status: u16, body: &str) -> HttpResponse {
    HttpResponse::new(status, body).header("content-type", "application/json")
}

fn form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// The browser: follow the authorization URL's redirect_uri with a code and
/// the state the URL carried.
fn browser_returns(url: &str, code: &str) -> std::thread::JoinHandle<String> {
    let query = url.split_once('?').unwrap().1.to_string();
    let pairs = form(&query);
    let state = param(&pairs, "state").unwrap().to_string();
    let port: u16 = param(&pairs, "redirect_uri")
        .unwrap()
        .trim_start_matches("http%3A%2F%2F127.0.0.1%3A")
        .trim_end_matches("%2Fcallback")
        .parse()
        .unwrap();
    let code = code.to_string();
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET /callback?code={code}&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        )
        .unwrap();
        let mut page = String::new();
        stream.read_to_string(&mut page).unwrap();
        page
    })
}

#[test]
fn a_sign_in_runs_from_the_authorization_url_to_a_stored_token_set() {
    let pending = AuthorizationCodeFlow::new(
        "example-cli",
        "https://auth.example.test/authorize",
        "client-1",
    )
    .scope("openid fhirUser offline_access launch/patient")
    .param("aud", "https://fhir.example.test/r4")
    .start()
    .unwrap();
    let url = pending.url().to_string();
    let pairs = form(url.split_once('?').unwrap().1);
    assert_eq!(param(&pairs, "code_challenge_method"), Some("S256"));
    assert_eq!(
        param(&pairs, "aud"),
        Some("https%3A%2F%2Ffhir.example.test%2Fr4")
    );
    assert_eq!(param(&pairs, "state").unwrap().len(), 43);

    let browser = browser_returns(&url, "dummy-code-1");
    let code = pending.wait_for_code(Duration::from_secs(20)).unwrap();
    assert!(browser.join().unwrap().contains("example-cli: signed in"));

    let client = OAuthClient::new(
        Canned::new(vec![json(200, &token_code())]),
        "example-cli",
        "0.1.0",
    );
    let answer = block_on(client.token(
        "https://auth.example.test/token",
        &ClientAuth::public("client-1"),
        &pending.token_request(&code),
    ))
    .unwrap();
    let tokens = TokenSet::from_response(answer, 1_000, None);

    let seen = client.transport().seen.borrow();
    let exchange = &seen[0];
    assert_eq!(
        exchange.header_value("user-agent"),
        Some("example-cli/0.1.0")
    );
    let body = form(exchange.body.as_deref().unwrap());
    assert_eq!(param(&body, "grant_type"), Some("authorization_code"));
    assert_eq!(param(&body, "code"), Some("dummy-code-1"));
    assert_eq!(param(&body, "client_id"), Some("client-1"));
    assert_eq!(param(&body, "code_verifier").unwrap().len(), 43);
    assert_eq!(
        param(&body, "redirect_uri").unwrap(),
        param(&pairs, "redirect_uri").unwrap(),
        "the exchange names the redirect the request sent"
    );

    assert_eq!(tokens.expires_at, Some(4_600));
    assert_eq!(tokens.extra["patient"], "example-patient-1");
    let id_claims = claims(tokens.id_token.as_deref().unwrap()).unwrap();
    assert_eq!(id_claims["fhirUser"], "Patient/example-patient-1");
}

#[test]
fn a_renewal_keeps_the_unrotated_refresh_token_and_the_launch_context() {
    let first = TokenSet::from_response(
        pk_cli_auth::oauth::TokenResponse::parse(&token_code()).unwrap(),
        1_000,
        None,
    );
    let client = OAuthClient::new(
        Canned::new(vec![json(200, TOKEN_REFRESH)]),
        "example-cli",
        "0.1.0",
    );
    let answer = block_on(client.token(
        "https://auth.example.test/token",
        &ClientAuth::public("client-1"),
        &TokenRequest::refresh(&first.refresh_token().unwrap()),
    ))
    .unwrap();
    let renewed = TokenSet::from_response(answer, 2_000, Some(&first));
    assert_eq!(renewed.access_token, "dummy-access-token-2");
    assert_eq!(
        renewed.refresh_token.as_deref(),
        Some("dummy-refresh-token-1")
    );
    assert_eq!(renewed.expires_at, Some(5_600));
    assert_eq!(renewed.extra["patient"], "example-patient-1");
    let body = form(client.transport().seen.borrow()[0].body.as_deref().unwrap());
    assert_eq!(param(&body, "grant_type"), Some("refresh_token"));
    assert_eq!(param(&body, "refresh_token"), Some("dummy-refresh-token-1"));
}

#[test]
fn a_spent_refresh_token_is_exit_3_and_names_no_token() {
    let client = OAuthClient::new(Canned::new(vec![json(400, TOKEN_INVALID_GRANT)]), "x", "1");
    let err = block_on(client.token(
        "https://auth.example.test/token",
        &ClientAuth::public("client-1"),
        &TokenRequest::refresh(&Secret::new("dummy-refresh-token-1")),
    ))
    .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert_eq!(err.reason(), Some(Reason::CredentialRefused));
    let message = err.to_string();
    assert!(
        message.contains("invalid_grant: The refresh token has expired."),
        "{message}"
    );
    assert!(!message.contains("dummy-refresh"), "{message}");
}

#[test]
fn a_rate_limited_token_endpoint_is_exit_5() {
    let slow = json(429, "{}").header("retry-after", "12");
    let client = OAuthClient::new(Canned::new(vec![slow]), "x", "1");
    let err = block_on(client.token(
        "https://auth.example.test/token",
        &ClientAuth::public("client-1"),
        &TokenRequest::refresh(&Secret::new("r")),
    ))
    .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert!(err.to_string().contains("12 s"), "{err}");
}

#[test]
fn a_public_native_client_registers_and_reads_back_its_id() {
    let client = OAuthClient::new(
        Canned::new(vec![json(201, REGISTRATION)]),
        "example-cli",
        "0.1.0",
    );
    let metadata =
        ClientMetadata::native_public("example-cli", vec!["http://127.0.0.1/callback".into()]);
    let registered = block_on(client.register(
        "https://auth.example.test/register",
        &metadata,
        Some(&Secret::new("dummy-initial-token")),
    ))
    .unwrap();
    assert_eq!(registered.client_id, "dummy-registered-client-1");
    assert_eq!(registered.client_secret_expires_at, Some(0));
    assert!(registered.client_secret().is_none());
    let seen = client.transport().seen.borrow();
    assert_eq!(
        seen[0].header_value("authorization"),
        Some("Bearer dummy-initial-token")
    );
    let sent: Value = serde_json::from_str(seen[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(sent["token_endpoint_auth_method"], "none");
}

#[test]
fn a_registration_refusal_or_an_answer_without_an_id_is_upstream() {
    let metadata = ClientMetadata::native_public("x", vec!["http://127.0.0.1/callback".into()]);
    for answer in [
        json(400, r#"{"error":"invalid_redirect_uri"}"#),
        json(201, r#"{"client_name":"x"}"#),
        json(201, r#"{"client_id":""}"#),
    ] {
        let client = OAuthClient::new(Canned::new(vec![answer]), "x", "1");
        let err = block_on(client.register("https://auth.example.test/register", &metadata, None))
            .unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
    }
    let client = OAuthClient::new(Canned::new(vec![]), "x", "1");
    let err = block_on(client.register("http://auth.example.test/register", &metadata, None))
        .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert!(
        client.transport().seen.borrow().is_empty(),
        "nothing was sent"
    );
}

/// The fixtures carry no real value: tokens and ids keep their dummy shapes.
#[test]
fn the_fixtures_hold_only_dummy_values() {
    for (name, body) in [
        ("token-code", token_code().as_str()),
        ("token-refresh", TOKEN_REFRESH),
        ("registration", REGISTRATION),
    ] {
        let value: Value = serde_json::from_str(body).unwrap();
        for field in ["access_token", "refresh_token", "client_id"] {
            if let Some(text) = value.get(field).and_then(Value::as_str) {
                assert!(text.starts_with("dummy-"), "{name}.{field}");
            }
        }
        if let Some(patient) = value.get("patient").and_then(Value::as_str) {
            assert!(patient.starts_with("example-"), "{name}.patient");
        }
        if let Some(id_token) = value.get("id_token").and_then(Value::as_str) {
            let claims = claims(id_token).unwrap();
            let keys: Vec<&String> = claims.as_object().unwrap().keys().collect();
            assert_eq!(keys, ["fhirUser"], "{name}.id_token holds only fhirUser");
            assert!(claims["fhirUser"].as_str().unwrap().contains("/example-"));
        }
    }
}
