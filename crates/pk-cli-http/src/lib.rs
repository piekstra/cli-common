//! HTTP plumbing for the piekstra CLI family.
//!
//! A blocking `reqwest` client builder with the family defaults (UA from
//! binary+version, cookie store, timeout), [`BlockingTransport`] (that client
//! behind `pk_cli_core::transport::Transport`), plus the standard raw `api`
//! passthrough command (SPEC v1 §1.2). Provider session logic (login dances,
//! scrape flows) stays in each CLI.

use std::time::Duration;

use pk_cli_core::transport::{HttpRequest, HttpResponse, Transport};
use pk_cli_core::CliError;
use serde_json::Value;

/// Build the family-standard blocking client: `<bin>/<version>` user agent,
/// cookie store enabled, 60s timeout.
pub fn client(binary: &str, version: &str) -> Result<reqwest::blocking::Client, CliError> {
    builder(binary, version)
        .build()
        .map_err(|e| CliError::Other(format!("failed to build HTTP client: {e}")))
}

/// The same defaults as [`client`], returned as a builder for CLIs that need
/// extra headers or redirect policies.
pub fn builder(binary: &str, version: &str) -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .user_agent(format!("{binary}/{version}"))
        .cookie_store(true)
        .timeout(Duration::from_secs(60))
}

/// The native [`Transport`]: a blocking `reqwest` client, so a provider
/// client written against the transport seam runs in the terminal binary
/// unchanged. Its futures finish on their first poll; drive them with
/// `pk_cli_core::block_on`.
#[derive(Debug, Clone)]
pub struct BlockingTransport {
    client: reqwest::blocking::Client,
}

impl BlockingTransport {
    /// Over the family-standard [`client`].
    pub fn new(binary: &str, version: &str) -> Result<Self, CliError> {
        Ok(Self::with_client(client(binary, version)?))
    }

    /// Over a client the CLI built itself (extra headers, a redirect
    /// policy; start from [`builder`]).
    pub fn with_client(client: reqwest::blocking::Client) -> Self {
        Self { client }
    }

    fn send_blocking(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| CliError::Usage(format!("`{}` is not an HTTP method", request.method)))?;
        let mut rb = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            rb = rb.header(name.as_str(), value.as_str());
        }
        if let Some(body) = request.body {
            rb = rb.body(body);
        }
        let resp = rb.send().map_err(|e| CliError::Upstream(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .map_err(|e| CliError::Upstream(format!("reading the response body: {e}")))?;
        Ok(HttpResponse { status, body })
    }
}

impl Transport for BlockingTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        self.send_blocking(request)
    }
}

/// The standard `api <METHOD> <PATH> [--data JSON]` arguments.
#[derive(clap::Args, Debug, Clone)]
pub struct ApiArgs {
    /// HTTP method: GET, POST, PUT, or DELETE.
    pub method: String,
    /// Path (leading slash, relative to the provider base URL) or full URL.
    pub path: String,
    /// Request body as a JSON string (for POST/PUT).
    #[arg(long)]
    pub data: Option<String>,
}

impl ApiArgs {
    /// Resolve `path` against a base URL (absolute URLs pass through).
    pub fn url(&self, base: &str) -> String {
        if self.path.starts_with("http://") || self.path.starts_with("https://") {
            self.path.clone()
        } else {
            format!("{}{}", base.trim_end_matches('/'), self.path)
        }
    }

    pub fn parsed_method(&self) -> Result<reqwest::Method, CliError> {
        match self.method.to_uppercase().as_str() {
            "GET" => Ok(reqwest::Method::GET),
            "POST" => Ok(reqwest::Method::POST),
            "PUT" => Ok(reqwest::Method::PUT),
            "DELETE" => Ok(reqwest::Method::DELETE),
            other => Err(CliError::Usage(format!(
                "unsupported HTTP method `{other}` (use GET, POST, PUT, or DELETE)"
            ))),
        }
    }

    pub fn parsed_body(&self) -> Result<Option<Value>, CliError> {
        self.data
            .as_deref()
            .map(|d| {
                serde_json::from_str(d)
                    .map_err(|e| CliError::Usage(format!("--data is not valid JSON: {e}")))
            })
            .transpose()
    }
}

/// Map a response to the exit-code contract: 401/403 → auth (3), 404 → not
/// found (4), other non-2xx → upstream (5); then parse JSON.
pub fn json_response(resp: reqwest::blocking::Response) -> Result<Value, CliError> {
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(CliError::Auth(format!(
            "provider rejected the request (HTTP {}) — try logging in again",
            status.as_u16()
        )));
    }
    if status.as_u16() == 404 {
        return Err(CliError::NotFound("HTTP 404 from provider".into()));
    }
    if !status.is_success() {
        return Err(CliError::Upstream(format!(
            "provider returned HTTP {}",
            status.as_u16()
        )));
    }
    resp.json::<Value>()
        .map_err(|e| CliError::Upstream(format!("parsing provider JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(method: &str, path: &str, data: Option<&str>) -> ApiArgs {
        ApiArgs {
            method: method.into(),
            path: path.into(),
            data: data.map(String::from),
        }
    }

    #[test]
    fn url_resolution() {
        let a = args("GET", "/v1/x", None);
        assert_eq!(
            a.url("https://api.example.com/"),
            "https://api.example.com/v1/x"
        );
        let b = args("GET", "https://other.example.com/y", None);
        assert_eq!(
            b.url("https://api.example.com"),
            "https://other.example.com/y"
        );
    }

    /// One request over a loopback socket: the method, path, query, headers
    /// and body arrive as built, and an error status comes back as a
    /// response, not an error.
    #[test]
    fn the_blocking_transport_sends_the_request_as_built() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&seen).contains("{\"q\":1}") {
                let n = sock.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            sock.write_all(
                b"HTTP/1.1 418 I'm a teapot\r\ncontent-type: text/plain\r\ncontent-length: 5\r\nconnection: close\r\n\r\nshort",
            )
            .unwrap();
            String::from_utf8_lossy(&seen).into_owned()
        });
        let transport = BlockingTransport::new("pk-test", "0.0.0").unwrap();
        let req = HttpRequest::new("POST", format!("http://{addr}/v1/x"))
            .query(&[("a", "b c".into())])
            .header("X-Test", "yes")
            .body("{\"q\":1}");
        let resp = pk_cli_core::block_on(transport.send(req)).unwrap();
        assert_eq!(resp.status, 418);
        assert_eq!(resp.body, "short");
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /v1/x?a=b%20c HTTP/1.1"), "{seen}");
        assert!(seen.to_ascii_lowercase().contains("x-test: yes"), "{seen}");
        assert!(seen.contains("pk-test/0.0.0"), "{seen}");
        assert!(pk_cli_core::block_on(transport.sign_es256(b"k", b"m")).is_none());
    }

    #[test]
    fn a_bad_method_or_no_connection_is_an_error_not_a_panic() {
        let transport = BlockingTransport::new("pk-test", "0.0.0").unwrap();
        let bad = HttpRequest::new("NOT A METHOD", "http://127.0.0.1:9/");
        assert!(matches!(
            pk_cli_core::block_on(transport.send(bad)),
            Err(CliError::Usage(_))
        ));
        // Port 9 (discard) on loopback: nothing listens there in CI.
        let refused = HttpRequest::get("http://127.0.0.1:9/");
        assert!(matches!(
            pk_cli_core::block_on(transport.send(refused)),
            Err(CliError::Upstream(_))
        ));
    }

    #[test]
    fn method_and_body_validation() {
        assert!(args("patch", "/", None).parsed_method().is_err());
        assert_eq!(
            args("post", "/", None).parsed_method().unwrap(),
            reqwest::Method::POST
        );
        assert!(args("POST", "/", Some("{not json")).parsed_body().is_err());
        assert_eq!(
            args("POST", "/", Some(r#"{"a":1}"#)).parsed_body().unwrap(),
            Some(serde_json::json!({"a":1}))
        );
    }
}
