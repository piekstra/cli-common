//! `<bin> mcp --http`: the same tools over stateless Streamable HTTP on
//! 127.0.0.1, for a tunnel from the user's own Cloudflare Worker (the
//! connector's "home" modules, cli-common `connector/`).
//!
//! A CLI that signs in through a browser keeps its session cookies on the
//! user's computer, and its provider refuses requests from datacenter
//! addresses, so its tools cannot run in a Worker. They run here instead,
//! in the CLI the user already signed in with; the Worker reaches this
//! server through a Cloudflare Tunnel (Workers VPC) and forwards Claude's
//! calls. Nothing listens beyond loopback, and every request must carry the
//! bearer token the Worker holds as a secret:
//!
//! - **Bind:** 127.0.0.1 only. There is no flag for another address.
//! - **Token:** `Authorization: Bearer <token>`, compared in constant time.
//!   It comes from `<BIN>_MCP_TOKEN` or the OS keychain (`piekstra.<bin>`,
//!   item `mcp-http-token`, written by `--new-token`), is at least 32
//!   visible ASCII characters, and the server refuses to start without one.
//! - **Host and Origin (DNS rebinding):** `Host` must name 127.0.0.1 or
//!   localhost (on this port, if it names one), and a request that carries
//!   an `Origin` header at all is refused: no browser is ever a client.
//! - **No CORS:** no `Access-Control-*` header is ever sent; `OPTIONS`, like
//!   every method but `POST`, is 405.
//! - **Read-only by default:** only tools annotated read-only are listed or
//!   callable, unless `--allow-writes`.
//! - **Stateless:** no MCP sessions; each `POST /mcp` is answered on its own,
//!   as JSON.
//! - **Bounded calls:** a call still running after [`CALL_TIMEOUT`] is
//!   answered with an error, inside Claude's own limit for a tool call.
//! - **Quiet:** stderr gets a start line and one line per refused request
//!   (the reason, never a header value or the arguments).

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use pk_cli_core::CliError;
use pk_cli_secrets::{CredentialStore, Secret};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::Value;

use crate::error::ToolError;
use crate::result::ToolOutput;
use crate::stdio::{Adapter, McpArgs};
use crate::tool::ToolDef;
use crate::toolset::{Server, Toolset};

/// The keychain item (`piekstra.<bin>`) that holds the bearer token.
pub const TOKEN_ACCOUNT: &str = "mcp-http-token";

/// The shortest token the server accepts. `--new-token` makes 64 hex
/// characters (256 bits).
pub const MIN_TOKEN_LEN: usize = 32;

/// How long a call may run before the Worker is answered with an error.
/// Claude gives a remote tool call 240 s in all.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// The largest request body accepted: a `tools/call` is a few hundred bytes.
const MAX_BODY: usize = 256 * 1024;

/// The environment variable that carries the token: `<BIN>_MCP_TOKEN`, the
/// binary's name upper-cased with anything but letters and digits as `_`
/// (`m1f` → `M1F_MCP_TOKEN`).
pub fn token_env_var(bin: &str) -> String {
    let name: String = bin
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{name}_MCP_TOKEN")
}

/// `--http` or `--new-token`. Everything checkable without the keychain is
/// checked first (SPEC v1 §1.2).
pub(crate) fn run<T: Toolset + Send + Sync + 'static>(
    args: &McpArgs,
    toolset: T,
) -> Result<(), CliError> {
    let server = toolset.server();
    if args.new_token {
        return new_token(&server.bin);
    }
    let port = match args.port {
        Some(0) => {
            return Err(CliError::Usage(
                "--port must be a port number from 1 to 65535".into(),
            ))
        }
        Some(port) => port,
        None => {
            return Err(CliError::Usage(format!(
                "`{} mcp --http` needs --port: the port your Worker's tunnel (VPC service) \
                 points at, e.g. --port 8731",
                server.bin
            )))
        }
    };
    let token = resolve_token(&server.bin, env_token(&server.bin), || {
        CredentialStore::for_binary(&server.bin).get(TOKEN_ACCOUNT)
    })?;
    serve(
        Scoped {
            inner: toolset,
            allow_writes: args.allow_writes,
        },
        port,
        token,
    )
}

/// The token from `<BIN>_MCP_TOKEN`, when it is set.
fn env_token(bin: &str) -> Result<Option<String>, CliError> {
    let var = token_env_var(bin);
    match std::env::var(&var) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CliError::Auth(format!(
            "{var} is set but is not valid UTF-8"
        ))),
    }
}

/// The token to require: the environment's when set (a bad one is an error,
/// never a fall back to the keychain), else the keychain's. None anywhere is
/// an error: the server never starts open.
fn resolve_token(
    bin: &str,
    env: Result<Option<String>, CliError>,
    keychain: impl FnOnce() -> Result<Option<Secret>, CliError>,
) -> Result<Secret, CliError> {
    let var = token_env_var(bin);
    if let Some(value) = env? {
        check_token(&value).map_err(|why| CliError::Auth(format!("{var} {why}")))?;
        return Ok(Secret::new(value));
    }
    match keychain()? {
        Some(secret) => {
            check_token(secret.expose()).map_err(|why| {
                CliError::Auth(format!(
                    "the token in the keychain (piekstra.{bin}, {TOKEN_ACCOUNT}) {why}; make a \
                     new one with `{bin} mcp --new-token`"
                ))
            })?;
            Ok(secret)
        }
        None => Err(CliError::Auth(format!(
            "`{bin} mcp --http` will not start without a bearer token: run `{bin} mcp \
             --new-token` (it saves one in the keychain and prints it for your Worker), or set \
             {var}"
        ))),
    }
}

/// Why a token is unusable, or `Ok`.
fn check_token(token: &str) -> Result<(), String> {
    if token.len() < MIN_TOKEN_LEN {
        return Err(format!(
            "is shorter than {MIN_TOKEN_LEN} characters; use a random one such as \
             `openssl rand -hex 32`"
        ));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("may hold only visible ASCII characters (no spaces)".into());
    }
    Ok(())
}

/// `--new-token`: 256 bits from the OS's CSPRNG, as 64 hex characters,
/// saved in the keychain and printed once on stdout (the instructions on
/// stderr), for the Worker's secret.
fn new_token(bin: &str) -> Result<(), CliError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| CliError::Other(format!("reading the OS random source: {e}")))?;
    let token = Secret::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    bytes.fill(0);
    CredentialStore::for_binary(bin).set(TOKEN_ACCOUNT, &token)?;
    eprintln!(
        "Saved a new token for `{bin} mcp --http` in the keychain (piekstra.{bin}, \
         {TOKEN_ACCOUNT}), replacing any earlier one. Set the same value as the bearer-token \
         secret in your Worker, then restart `{bin} mcp --http`. It is shown once:"
    );
    println!("{}", token.expose());
    Ok(())
}

/// The toolset as the HTTP server serves it: the read-only tools only,
/// unless writes were allowed. A tool left out is unknown to `tools/call`
/// as well as absent from `tools/list`.
struct Scoped<T> {
    inner: T,
    allow_writes: bool,
}

impl<T: Toolset> Toolset for Scoped<T> {
    fn server(&self) -> Server {
        self.inner.server()
    }

    fn tools(&self) -> Vec<ToolDef> {
        let tools = self.inner.tools();
        if self.allow_writes {
            tools
        } else {
            tools
                .into_iter()
                .filter(|t| t.annotations.read_only)
                .collect()
        }
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        self.inner.call(name, args).await
    }
}

/// Serve on 127.0.0.1:`port` until the process is stopped.
fn serve<T: Toolset + Send + Sync + 'static>(
    toolset: Scoped<T>,
    port: u16,
    token: Secret,
) -> Result<(), CliError> {
    let server = toolset.server();
    let tools = toolset.tools();
    let writes = tools.iter().filter(|t| !t.annotations.read_only).count();
    let adapter = Adapter::new(toolset, Some(CALL_TIMEOUT))?;
    // One thread for the connections; each call runs on Tokio's blocking
    // pool (the adapter's), so calls still run side by side.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Other(format!("starting the MCP runtime: {e}")))?;
    runtime.block_on(async move {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| CliError::Other(format!("listening on {addr}: {e}")))?;
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_allowed_hosts(LOOPBACK_NAMES)
            .enforce_origin_validation()
            .with_max_request_body_bytes(MAX_BODY);
        let mcp = StreamableHttpService::new(
            move || Ok(adapter.clone()),
            Arc::new(NeverSessionManager::default()),
            config,
        );
        let gate = Arc::new(Gate { token, port });
        eprintln!(
            "{bin} mcp: serving {n} tool{s} ({kind}) at http://{addr}/mcp to holders of the \
             bearer token",
            bin = server.bin,
            n = tools.len(),
            s = if tools.len() == 1 { "" } else { "s" },
            kind = if writes == 0 {
                "read-only".to_string()
            } else {
                format!("{writes} of them can change data")
            },
        );
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                // A connection that failed before it was accepted (reset,
                // out of descriptors for a moment) is not the server's end.
                Err(e) => {
                    eprintln!("{} mcp: accepting a connection: {e}", server.bin);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let gate = Arc::clone(&gate);
            let mcp = mcp.clone();
            let bin = server.bin.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let gate = Arc::clone(&gate);
                    let mcp = mcp.clone();
                    let bin = bin.clone();
                    async move {
                        Ok::<_, Infallible>(match gate.check(&request) {
                            Ok(()) => mcp.handle(request).await,
                            Err(refusal) => {
                                eprintln!("{bin} mcp: refused a request: {}", refusal.reason);
                                refusal.response()
                            }
                        })
                    }
                });
                // A client that hangs up mid-request ends only its own
                // connection.
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    })
}

/// The names `Host` may carry.
const LOOPBACK_NAMES: [&str; 2] = ["127.0.0.1", "localhost"];

/// What every request must pass before the MCP service sees it.
struct Gate {
    token: Secret,
    port: u16,
}

/// A refused request: its status and the reason (for stderr and the body).
#[derive(Debug, PartialEq, Eq)]
struct Refusal {
    status: StatusCode,
    reason: &'static str,
}

impl Refusal {
    fn new(status: StatusCode, reason: &'static str) -> Self {
        Self { status, reason }
    }

    fn response(&self) -> Response<BoxBody<Bytes, Infallible>> {
        let mut response = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store");
        if self.status == StatusCode::UNAUTHORIZED {
            response = response.header(header::WWW_AUTHENTICATE, "Bearer");
        }
        if self.status == StatusCode::METHOD_NOT_ALLOWED {
            response = response.header(header::ALLOW, "POST");
        }
        response
            .body(Full::new(Bytes::from(format!("{}\n", self.reason))).boxed())
            .expect("a static response is valid")
    }
}

impl Gate {
    /// Host, then Origin, then the token, then the path and method: a
    /// request without the token learns nothing about what is served.
    fn check<B>(&self, request: &Request<B>) -> Result<(), Refusal> {
        let headers = request.headers();
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
        if !host.is_some_and(|h| host_allowed(h, self.port)) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "the Host header must be 127.0.0.1 or localhost",
            ));
        }
        if headers.contains_key(header::ORIGIN) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "requests from a browser (with an Origin header) are refused",
            ));
        }
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        if !presented.is_some_and(|t| same(t.as_bytes(), self.token.expose().as_bytes())) {
            return Err(Refusal::new(
                StatusCode::UNAUTHORIZED,
                "a valid bearer token is required",
            ));
        }
        if request.uri().path() != "/mcp" {
            return Err(Refusal::new(
                StatusCode::NOT_FOUND,
                "the MCP endpoint is /mcp",
            ));
        }
        if request.method() != Method::POST {
            return Err(Refusal::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "this stateless server answers POST only",
            ));
        }
        Ok(())
    }
}

/// `Host` names loopback, and the port when it names one is this server's.
/// A DNS-rebinding page sends its own hostname, so it never passes.
fn host_allowed(host: &str, port: u16) -> bool {
    let (name, named_port) = match host.rsplit_once(':') {
        Some((name, p)) => match p.parse::<u16>() {
            Ok(p) => (name, Some(p)),
            Err(_) => return false,
        },
        None => (host, None),
    };
    LOOPBACK_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
        && named_port.is_none_or(|p| p == port)
}

/// Whether two byte strings are equal, taking the same time wherever they
/// differ. A length mismatch returns at once: the token's length is no
/// secret (`--new-token`'s is always 64).
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn gate() -> Gate {
        Gate {
            token: Secret::new(TOKEN),
            port: 8731,
        }
    }

    fn request(host: Option<&str>, auth: Option<&str>) -> http::request::Builder {
        let mut b = Request::builder().method(Method::POST).uri("/mcp");
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, a);
        }
        b
    }

    fn bearer() -> String {
        format!("Bearer {TOKEN}")
    }

    #[test]
    fn the_variable_is_named_after_the_binary() {
        assert_eq!(token_env_var("m1f"), "M1F_MCP_TOKEN");
        assert_eq!(token_env_var("example-cli"), "EXAMPLE_CLI_MCP_TOKEN");
    }

    #[test]
    fn a_request_with_the_token_on_loopback_passes() {
        for host in ["127.0.0.1:8731", "localhost:8731", "127.0.0.1", "LOCALHOST"] {
            let r = request(Some(host), Some(&bearer())).body(()).unwrap();
            assert_eq!(gate().check(&r), Ok(()), "{host}");
        }
    }

    #[test]
    fn a_foreign_or_missing_host_is_forbidden_before_the_token_is_looked_at() {
        for host in [
            Some("evil.example"),
            Some("evil.example:8731"),
            Some("127.0.0.1:9999"),
            Some("127.0.0.1.evil.example"),
            Some("localhost:x"),
            Some("[::1]:8731"),
            None,
        ] {
            let r = request(host, None).body(()).unwrap();
            let refusal = gate().check(&r).unwrap_err();
            assert_eq!(refusal.status, StatusCode::FORBIDDEN, "{host:?}");
        }
    }

    #[test]
    fn any_origin_is_forbidden() {
        for origin in ["http://127.0.0.1:8731", "https://claude.ai", "null"] {
            let r = request(Some("127.0.0.1:8731"), Some(&bearer()))
                .header(header::ORIGIN, origin)
                .body(())
                .unwrap();
            assert_eq!(gate().check(&r).unwrap_err().status, StatusCode::FORBIDDEN);
        }
    }

    #[test]
    fn a_missing_or_wrong_token_is_unauthorized() {
        let wrong = format!("Bearer {}", TOKEN.replace('0', "1"));
        let short = format!("Bearer {}", &TOKEN[..63]);
        let basic = format!("Basic {TOKEN}");
        let lower = format!("bearer {TOKEN}");
        for auth in [
            None,
            Some(wrong.as_str()),
            Some(short.as_str()),
            Some(basic.as_str()),
            Some(lower.as_str()),
            Some("Bearer "),
        ] {
            let r = request(Some("127.0.0.1:8731"), auth).body(()).unwrap();
            let refusal = gate().check(&r).unwrap_err();
            assert_eq!(refusal.status, StatusCode::UNAUTHORIZED, "{auth:?}");
            let response = refusal.response();
            assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
        }
    }

    #[test]
    fn only_post_to_mcp_is_served_and_nothing_is_cors() {
        let r = Request::builder()
            .method(Method::POST)
            .uri("/other")
            .header(header::HOST, "127.0.0.1:8731")
            .header(header::AUTHORIZATION, bearer())
            .body(())
            .unwrap();
        assert_eq!(gate().check(&r).unwrap_err().status, StatusCode::NOT_FOUND);
        for method in [Method::GET, Method::OPTIONS, Method::DELETE, Method::PUT] {
            let r = request(Some("127.0.0.1:8731"), Some(&bearer()))
                .method(method.clone())
                .body(())
                .unwrap();
            let refusal = gate().check(&r).unwrap_err();
            assert_eq!(refusal.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
            let response = refusal.response();
            assert!(
                !response
                    .headers()
                    .keys()
                    .any(|k| k.as_str().starts_with("access-control-")),
                "{method}"
            );
        }
    }

    #[test]
    fn the_environment_wins_and_a_bad_value_never_falls_back_to_the_keychain() {
        let never = || -> Result<Option<Secret>, CliError> {
            panic!("the keychain must not be read when the environment holds a token")
        };
        let got = resolve_token("m1f", Ok(Some(TOKEN.into())), never).unwrap();
        assert_eq!(got.expose(), TOKEN);
        for bad in ["", "short", &format!("{} x", &TOKEN[..40])] {
            let e = resolve_token("m1f", Ok(Some(bad.into())), never).unwrap_err();
            assert_eq!(e.exit_code(), 3, "{bad:?}");
            assert!(e.to_string().contains("M1F_MCP_TOKEN"), "{e}");
        }
    }

    #[test]
    fn without_a_token_anywhere_the_server_refuses_to_start() {
        let e = resolve_token("m1f", Ok(None), || Ok(None)).unwrap_err();
        assert_eq!(e.exit_code(), 3);
        let text = e.to_string();
        assert!(text.contains("will not start"), "{text}");
        assert!(text.contains("m1f mcp --new-token") && text.contains("M1F_MCP_TOKEN"));

        let e = resolve_token("m1f", Ok(None), || Ok(Some(Secret::new("short")))).unwrap_err();
        assert!(e.to_string().contains("keychain"), "{e}");

        let got = resolve_token("m1f", Ok(None), || Ok(Some(Secret::new(TOKEN)))).unwrap();
        assert_eq!(got.expose(), TOKEN);

        let e = resolve_token("m1f", Ok(None), || Err(CliError::Keychain("locked".into())))
            .unwrap_err();
        assert_eq!(e.exit_code(), 1);
    }

    #[test]
    fn equal_bytes_compare_equal_and_any_difference_does_not() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }
}
