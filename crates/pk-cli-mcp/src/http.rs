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
//!   item `mcp-http-token`, written by `--new-token`, which will not replace
//!   a saved one without `--overwrite`), is at least 32 visible ASCII
//!   characters, and the server refuses to start without one.
//! - **Host and Origin (DNS rebinding):** `Host` must name 127.0.0.1 or
//!   localhost (on this port, if it names one), and a request that carries
//!   an `Origin` header at all is refused: no browser is ever a client.
//! - **No CORS:** no `Access-Control-*` header is ever sent; `OPTIONS`, like
//!   every method but `POST`, is 405.
//! - **Read-only by default:** only tools annotated read-only are listed or
//!   callable, unless `--allow-writes`.
//! - **Stateless:** no MCP sessions; each `POST /mcp` is answered on its own,
//!   as JSON.
//! - **Bounded calls:** a call still running after [`CALL_LIMIT`] is
//!   answered with an error, inside Claude's own limit for a tool call: a
//!   read may be retried, a write's outcome is reported as unknown.
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
use pk_cli_secrets::{
    CredentialStore, KeychainRead, Secret, SecretResolver, SecretSpec, SourceKind, SourceOrder,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::command::McpArgs;
use crate::deadline::CALL_LIMIT;
use crate::error::ToolError;
use crate::result::ToolOutput;
use crate::tool::ToolDef;
use crate::toolset::{Server, Toolset};

/// The keychain item (`piekstra.<bin>`) that holds the bearer token. `pub` for a CLI's
/// `auth logout --forget`, which removes it by this name so that forgetting a CLI leaves
/// no credential of its behind.
pub const HTTP_TOKEN_ACCOUNT: &str = "mcp-http-token";

/// The shortest token the server accepts. `--new-token` makes 64 hex
/// characters (256 bits).
const MIN_TOKEN_LEN: usize = 32;

/// The largest request body accepted: a `tools/call` is a few hundred bytes.
const MAX_BODY: usize = 256 * 1024;

/// The environment variable that carries the token: `<BIN>_MCP_TOKEN`, the
/// binary's name upper-cased with anything but letters and digits as `_`
/// (`m1f` → `M1F_MCP_TOKEN`).
fn token_env_var(bin: &str) -> String {
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

/// The address `--http` listens on: loopback, and nothing else.
fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// `--http` or `--new-token`. Everything checkable without the keychain is
/// checked first (SPEC v1 §1.2).
pub(crate) fn run<T: Toolset + Send + Sync + 'static>(
    args: &McpArgs,
    toolset: T,
) -> Result<(), CliError> {
    let server = toolset.server();
    let store = CredentialStore::for_binary(&server.bin);
    if args.new_token {
        let token = save_new_token(&store, &server.bin, args.overwrite, fresh_token()?)?;
        eprintln!(
            "Saved a new token for `{bin} mcp --http` in the keychain ({service}, \
             {HTTP_TOKEN_ACCOUNT}). Set the same value as the bearer-token secret in your Worker, \
             then (re)start `{bin} mcp --http`. It is shown once:",
            bin = server.bin,
            service = store.service(),
        );
        println!("{}", token.expose());
        return Ok(());
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
    let token = resolve_token(&store, &server.bin)?;
    serve(
        Scoped {
            inner: toolset,
            allow_writes: args.allow_writes,
        },
        port,
        token,
    )
}

/// The token to require, through the family's resolver (DESIGN.md §1.7):
/// `<BIN>_MCP_TOKEN` when set, else the keychain item. A bad value is an
/// error, never a fall back to the next source; none anywhere is an error
/// too: the server never starts open.
fn resolve_token<K: KeychainRead + ?Sized>(store: &K, bin: &str) -> Result<Secret, CliError> {
    let var = token_env_var(bin);
    let order =
        SourceOrder::new(&[SourceKind::Env, SourceKind::Keychain]).map_err(CliError::Other)?;
    let replace = format!("`{bin} mcp --new-token --overwrite`");
    let spec = SecretSpec::new(HTTP_TOKEN_ACCOUNT)
        .env(var.clone())
        .remedy(replace.clone());
    let resolved = SecretResolver::new(store).order(order).resolve(&spec)?;
    let Some(resolved) = resolved else {
        return Err(CliError::Auth(format!(
            "`{bin} mcp --http` will not start without a bearer token: run `{bin} mcp \
             --new-token` (it saves one in the keychain and prints it for your Worker), or set \
             {var}"
        )));
    };
    check_token(resolved.secret.expose()).map_err(|why| {
        CliError::Auth(match resolved.source {
            SourceKind::Env => format!("{var} {why}"),
            _ => format!(
                "the token in the keychain ({}, {HTTP_TOKEN_ACCOUNT}) {why}; make a new one with \
                 {replace}",
                store.service()
            ),
        })
    })?;
    Ok(resolved.secret)
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

/// 256 bits from the OS's CSPRNG, as 64 lower-case hex characters.
fn fresh_token() -> Result<Secret, CliError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| CliError::Other(format!("reading the OS random source: {e}")))?;
    let token = Secret::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    bytes.fill(0);
    Ok(token)
}

/// Where `--new-token` keeps the token: the keychain, or a map in tests.
trait TokenStore: KeychainRead {
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError>;
}

impl TokenStore for CredentialStore {
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
        CredentialStore::set(self, account, secret)
    }
}

/// `--new-token`: save `token`, refusing to replace a saved one without
/// `--overwrite` (the Worker holds the old one), then read it back, so the
/// token printed is the one the server will require.
fn save_new_token<S: TokenStore + ?Sized>(
    store: &S,
    bin: &str,
    overwrite: bool,
    token: Secret,
) -> Result<Secret, CliError> {
    if !overwrite && store.get(HTTP_TOKEN_ACCOUNT)?.is_some() {
        return Err(CliError::Usage(format!(
            "a token for `{bin} mcp --http` is already saved ({}, {HTTP_TOKEN_ACCOUNT}); pass \
             --overwrite to replace it, then update the Worker's secret to the new one",
            store.service()
        )));
    }
    store.set(HTTP_TOKEN_ACCOUNT, &token)?;
    match store.get(HTTP_TOKEN_ACCOUNT)? {
        Some(kept) if kept.expose() == token.expose() => Ok(token),
        _ => Err(CliError::Keychain(format!(
            "the keychain did not keep the new token ({}, {HTTP_TOKEN_ACCOUNT}); nothing was printed, \
             try `{bin} mcp --new-token --overwrite` again",
            store.service()
        ))),
    }
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

/// What a running server prints once. It names the version because the server
/// outlives `self-update` (see [`restart_note`]), so its log is where to check
/// which code is running.
fn start_line(server: &Server, addr: SocketAddr, tools: usize, writes: usize) -> String {
    format!(
        "{bin} {version} mcp: serving {tools} tool{s} ({kind}) at http://{addr}/mcp to holders \
         of the bearer token",
        bin = server.bin,
        version = server.version,
        s = if tools == 1 { "" } else { "s" },
        kind = if writes == 0 {
            "read-only".to_string()
        } else {
            format!("{writes} of them can change data")
        },
    )
}

/// The note for `pk_cli_selfupdate::Updater::run_noting`: `self-update` swaps
/// the binary on disk, but `<bin> mcp --http` runs until it is stopped, so it
/// keeps serving the old code until someone restarts it, and a Claude Desktop
/// extension runs the copy its `.mcpb` bundled. On macOS the note adds the command
/// for the launchd agent `connector/`'s README installs, whose label it names;
/// elsewhere there is no one command to give.
pub fn restart_note(bin: &str) -> String {
    note_for(bin, cfg!(target_os = "macos"))
}

fn note_for(bin: &str, launchd: bool) -> String {
    let server = format!(
        "note: a running `{bin} mcp --http` keeps serving the old version until it is restarted."
    );
    let restart = if launchd {
        format!(
            " Under the launchd agent from cli-common's connector/ README:\n  \
             launchctl kickstart -k gui/$(id -u)/com.piekstra.{bin}-mcp-http"
        )
    } else {
        String::new()
    };
    format!(
        "{server}{restart}\n\
         note: a Claude Desktop extension runs its own copy: install the new release's `.mcpb` to \
         replace it."
    )
}

/// The note for `auth logout --forget`, once it has removed [`HTTP_TOKEN_ACCOUNT`]: the delete
/// stops the next server reusing that token and does nothing to this one, which read it at its
/// start, or to the variable. So a logout that reads as the end of access still leaves a way in
/// until someone stops the server. On macOS the note adds the command for the launchd agent
/// `connector/`'s README installs, whose label it names; elsewhere there is no one command to give.
pub fn stop_note(bin: &str) -> String {
    stop_note_for(bin, cfg!(target_os = "macos"))
}

fn stop_note_for(bin: &str, launchd: bool) -> String {
    let var = token_env_var(bin);
    let server = format!(
        "note: a running `{bin} mcp --http` keeps accepting the token it started with until it \
         stops, and ${var} is beyond this command: unset it if it is set."
    );
    let stop = if launchd {
        format!(
            " Under the launchd agent from cli-common's connector/ README:\n  \
             launchctl bootout gui/$(id -u)/com.piekstra.{bin}-mcp-http"
        )
    } else {
        String::new()
    };
    format!(
        "{server}{stop}\n\
         note: a connector still in use needs `{bin} mcp --new-token`."
    )
}

/// The note for an `auth logout --forget` that got an answer about [`HTTP_TOKEN_ACCOUNT`], which
/// every such logout owes whoever ran it. A removed item gets [`stop_note`]. With no item a server
/// can still be running on `$<BIN>_MCP_TOKEN`, which the logout cannot see, so this claims no
/// removal and says what would end that server.
pub fn forget_note(bin: &str, token_removed: bool) -> String {
    forget_note_for(bin, token_removed, cfg!(target_os = "macos"))
}

fn forget_note_for(bin: &str, token_removed: bool, launchd: bool) -> String {
    if token_removed {
        return stop_note_for(bin, launchd);
    }
    let var = token_env_var(bin);
    let stop = if launchd {
        format!(
            " Under the launchd agent from cli-common's connector/ README:\n  \
             launchctl bootout gui/$(id -u)/com.piekstra.{bin}-mcp-http"
        )
    } else {
        String::new()
    };
    format!(
        "note: the keychain held no `{bin} mcp --http` token. A running `{bin} mcp --http` with \
         one in ${var} keeps accepting it until it stops: stop it, and unset that variable.{stop}"
    )
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
    let adapter = Adapter::new(toolset, Some(CALL_LIMIT))?;
    // One thread for the connections; each call runs on Tokio's blocking
    // pool (the adapter's), so calls still run side by side.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Other(format!("starting the MCP runtime: {e}")))?;
    runtime.block_on(async move {
        let addr = bind_addr(port);
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
        eprintln!("{}", start_line(&server, addr, tools.len(), writes));
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
    fn the_start_line_names_the_version() {
        let line = start_line(
            &Server::new("svc", "1.2.3", "Service", "svc"),
            bind_addr(8731),
            2,
            0,
        );
        assert_eq!(
            line,
            "svc 1.2.3 mcp: serving 2 tools (read-only) at http://127.0.0.1:8731/mcp to holders \
             of the bearer token"
        );
    }

    #[test]
    fn the_note_names_the_extensions_own_copy() {
        for launchd in [true, false] {
            assert!(note_for("m1f", launchd).contains("`.mcpb`"));
        }
    }

    #[test]
    fn without_launchd_the_note_gives_no_launchctl_command() {
        let note = note_for("m1f", false);
        assert!(note.contains("`m1f mcp --http`"), "{note}");
        assert!(!note.contains("launchctl"), "{note}");
    }

    /// The label is the one the connector's README installs and restarts.
    #[test]
    fn the_restart_note_uses_the_connectors_launchd_label() {
        let kickstart = "launchctl kickstart -k gui/$(id -u)/com.piekstra.m1f-mcp-http";
        assert!(note_for("m1f", true).contains(kickstart));
        let readme =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../connector/README.md");
        let readme = std::fs::read_to_string(readme).unwrap();
        assert!(readme.contains(kickstart));
        assert!(readme.contains("<string>com.piekstra.m1f-mcp-http</string>"));
    }

    #[test]
    fn the_stop_note_says_what_a_removed_token_does_not_end() {
        let note = stop_note_for("m1f", false);
        assert!(note.contains("`m1f mcp --http`"), "{note}");
        assert!(note.contains("$M1F_MCP_TOKEN"), "{note}");
        assert!(note.contains("`m1f mcp --new-token`"), "{note}");
        assert!(!note.contains("launchctl"), "{note}");
    }

    /// The same label the restart note and the connector's README use.
    #[test]
    fn the_stop_note_uses_the_connectors_launchd_label() {
        let bootout = "launchctl bootout gui/$(id -u)/com.piekstra.m1f-mcp-http";
        assert!(stop_note_for("m1f", true).contains(bootout));
        let readme =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../connector/README.md");
        let readme = std::fs::read_to_string(readme).unwrap();
        assert!(readme.contains(bootout));
    }

    #[test]
    fn a_forget_that_removed_the_token_owes_the_stop_note() {
        assert_eq!(
            forget_note_for("m1f", true, false),
            stop_note_for("m1f", false)
        );
    }

    /// A server on the variable never touched the keychain item, so it is still running whatever
    /// the delete found.
    #[test]
    fn a_forget_with_no_token_item_still_speaks_to_a_server_on_the_variable() {
        let note = forget_note_for("m1f", false, false);
        assert!(note.contains("held no `m1f mcp --http` token"), "{note}");
        assert!(!note.contains("removed"), "{note}");
        assert!(note.contains("$M1F_MCP_TOKEN"), "{note}");
        assert!(note.contains("stop it, and unset"), "{note}");
        assert!(!note.contains("launchctl"), "{note}");
        let bootout = "launchctl bootout gui/$(id -u)/com.piekstra.m1f-mcp-http";
        assert!(forget_note_for("m1f", false, true).contains(bootout));
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

    /// The keychain, as a map. `writes_lost` drops every write, as a
    /// keychain that failed silently would.
    #[derive(Default)]
    struct MemStore {
        items: std::cell::RefCell<std::collections::HashMap<String, String>>,
        reads: std::cell::Cell<usize>,
        writes_lost: bool,
    }

    impl MemStore {
        fn holding(token: &str) -> Self {
            let store = MemStore::default();
            store
                .items
                .borrow_mut()
                .insert(HTTP_TOKEN_ACCOUNT.into(), token.into());
            store
        }
    }

    impl KeychainRead for MemStore {
        fn service(&self) -> &str {
            "piekstra.test"
        }
        fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.items.borrow().get(account).map(Secret::new))
        }
    }

    impl TokenStore for MemStore {
        fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
            if !self.writes_lost {
                self.items
                    .borrow_mut()
                    .insert(account.into(), secret.expose().into());
            }
            Ok(())
        }
    }

    // A binary name whose `<BIN>_MCP_TOKEN` no test environment sets, so
    // these reach the keychain side. The environment side runs end to end
    // in example-cli's tests/mcp_http.rs, which sets the variable.
    const BIN: &str = "pk-mcp-unit-test";

    #[test]
    fn the_keychain_token_is_used_when_the_environment_has_none() {
        assert!(std::env::var(token_env_var(BIN)).is_err());
        let got = resolve_token(&MemStore::holding(TOKEN), BIN).unwrap();
        assert_eq!(got.expose(), TOKEN);
    }

    #[test]
    fn without_a_token_anywhere_the_server_refuses_to_start() {
        let e = resolve_token(&MemStore::default(), BIN).unwrap_err();
        assert_eq!(e.exit_code(), 3);
        let text = e.to_string();
        assert!(text.contains("will not start"), "{text}");
        assert!(text.contains("pk-mcp-unit-test mcp --new-token"), "{text}");
        assert!(text.contains("PK_MCP_UNIT_TEST_MCP_TOKEN"), "{text}");
    }

    #[test]
    fn an_unusable_keychain_token_names_the_item_and_the_remedy() {
        for bad in ["", "short", "0123456789abcdef 0123456789abcdef0123"] {
            let e = resolve_token(&MemStore::holding(bad), BIN).unwrap_err();
            assert_eq!(e.exit_code(), 3, "{bad:?}");
            let text = e.to_string();
            assert!(
                text.contains("mcp-http-token") && text.contains("piekstra.test"),
                "{text}"
            );
            assert!(text.contains("--new-token --overwrite"), "{text}");
            assert!(!text.contains("auth login"), "{text}");
        }
    }

    #[test]
    fn a_new_token_is_saved_read_back_and_returned() {
        let store = MemStore::default();
        let got = save_new_token(&store, BIN, false, Secret::new(TOKEN)).unwrap();
        assert_eq!(got.expose(), TOKEN);
        assert_eq!(store.items.borrow()[HTTP_TOKEN_ACCOUNT], TOKEN);
    }

    #[test]
    fn a_saved_token_is_replaced_only_with_overwrite() {
        let store = MemStore::holding("an-earlier-token-the-worker-already-holds");
        let e = save_new_token(&store, BIN, false, Secret::new(TOKEN)).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--overwrite"), "{e}");
        assert_eq!(
            store.items.borrow()[HTTP_TOKEN_ACCOUNT],
            "an-earlier-token-the-worker-already-holds",
            "refused before anything was written"
        );

        save_new_token(&store, BIN, true, Secret::new(TOKEN)).unwrap();
        assert_eq!(store.items.borrow()[HTTP_TOKEN_ACCOUNT], TOKEN);
    }

    #[test]
    fn a_token_the_keychain_did_not_keep_is_never_printed() {
        let store = MemStore {
            writes_lost: true,
            ..MemStore::default()
        };
        let e = save_new_token(&store, BIN, false, Secret::new(TOKEN)).unwrap_err();
        assert!(e.to_string().contains("did not keep"), "{e}");
        assert!(store.reads.get() >= 2, "it read the item back");
    }

    #[test]
    fn a_fresh_token_is_64_hex_characters_and_never_repeats() {
        let a = fresh_token().unwrap();
        let b = fresh_token().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert!(a
            .expose()
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a.expose(), b.expose());
        assert_eq!(check_token(a.expose()), Ok(()));
    }

    #[test]
    fn it_binds_loopback_only() {
        let addr = bind_addr(8731);
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(addr.port(), 8731);
    }

    #[test]
    fn equal_bytes_compare_equal_and_any_difference_does_not() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }
}
