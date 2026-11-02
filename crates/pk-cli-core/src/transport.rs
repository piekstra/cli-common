//! The seam between a CLI's provider client and whatever moves its bytes.
//!
//! A family CLI's provider client (sign a request, page a list, map statuses
//! onto exit codes) is the same code wherever it runs. What differs is the
//! last step: the terminal binary sends with a blocking `reqwest` client
//! (`pk_cli_http::BlockingTransport`), and a Cloudflare Worker, which has no
//! sockets and no threads, hands the request to its host's `fetch`
//! (`pk_cli_mcp::wasm::HostTransport`). Writing the client once against
//! [`Transport`] lets one MCP toolset serve both `<bin> mcp` and the
//! deploy-your-own Worker (DESIGN.md §2.3).
//!
//! The trait is async because the Worker's side has to be. A blocking
//! implementation's futures are ready on their first poll, so native code
//! drives them with [`crate::block_on`] at no cost.

use crate::CliError;

/// One outgoing request, fully described: the transport adds nothing but a
/// connection. Build it with [`HttpRequest::get`] or [`HttpRequest::new`];
/// [`HttpRequest::query`] appends percent-encoded parameters to the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Upper-case method: `GET`, `POST`, …
    pub method: String,
    /// Absolute URL, query string included.
    pub url: String,
    /// Header name and value pairs, in order.
    pub headers: Vec<(String, String)>,
    /// The body, for a method that carries one.
    pub body: Option<String>,
}

impl HttpRequest {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into().to_ascii_uppercase(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    /// Append `params` to the URL's query string, each name and value
    /// percent-encoded (everything but RFC 3986's unreserved characters).
    pub fn query(mut self, params: &[(&str, String)]) -> Self {
        for (name, value) in params {
            let sep = if self.url.contains('?') { '&' } else { '?' };
            self.url.push(sep);
            self.url.push_str(&percent_encode(name));
            self.url.push('=');
            self.url.push_str(&percent_encode(value));
        }
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// The value of the first header called `name` (case-insensitive).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What a client needs back: the status and the body as text. The body is
/// text, not parsed JSON, because providers answer some failures with a
/// plain-text body, and the client decides how to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Something that can send an [`HttpRequest`].
///
/// No `Send` bound, as in `pk_cli_mcp::Toolset`: the Worker is
/// single-threaded and its futures hold JavaScript values, and the native
/// binary polls on the calling thread.
///
/// A network failure (no connection, a timeout) is [`CliError::Upstream`];
/// an HTTP error status is not an error here but a response, for the client
/// to map.
#[allow(async_fn_in_trait)] // deliberate: no `Send` bound, for wasm hosts
pub trait Transport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError>;

    /// ECDSA P-256 with SHA-256 over `message`, by the platform's own crypto,
    /// as the fixed-width 64-byte `r || s` a JWS carries. `key` is the
    /// private key as PKCS#8 DER.
    ///
    /// `None` (the default) means the platform offers nothing faster than
    /// pure Rust, and the caller signs itself. A Worker's WebCrypto signs in
    /// native code, well under a tenth of the CPU a WebAssembly signature
    /// costs against the free plan's 10 ms per request.
    async fn sign_es256(&self, key: &[u8], message: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
        let _ = (key, message);
        None
    }
}

impl<T: Transport + ?Sized> Transport for &T {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        (**self).send(request).await
    }

    async fn sign_es256(&self, key: &[u8], message: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
        (**self).sign_es256(key, message).await
    }
}

/// Percent-encode everything but RFC 3986's unreserved characters
/// (`A-Z a-z 0-9 - . _ ~`), byte by byte over the UTF-8.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;

    #[test]
    fn query_parameters_are_appended_and_encoded() {
        let r = HttpRequest::get("https://api.example.com/v2/items")
            .query(&[("limit", "100".into()), ("after", "a b&c=d/é".into())]);
        assert_eq!(
            r.url,
            "https://api.example.com/v2/items?limit=100&after=a%20b%26c%3Dd%2F%C3%A9"
        );
        let r = HttpRequest::get("https://x.test/p?a=1").query(&[("b", "2".into())]);
        assert_eq!(r.url, "https://x.test/p?a=1&b=2");
        let r = HttpRequest::get("https://x.test/p").query(&[]);
        assert_eq!(r.url, "https://x.test/p");
    }

    #[test]
    fn unreserved_characters_pass_through() {
        assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
        assert_eq!(percent_encode("+"), "%2B");
    }

    #[test]
    fn headers_are_kept_in_order_and_read_case_insensitively() {
        let r = HttpRequest::new("post", "https://x.test")
            .header("Authorization", "Bearer t")
            .header("Accept", "application/json")
            .body("{}");
        assert_eq!(r.method, "POST");
        assert_eq!(r.header_value("authorization"), Some("Bearer t"));
        assert_eq!(r.headers[1].0, "Accept");
        assert_eq!(r.body.as_deref(), Some("{}"));
        assert_eq!(r.header_value("cookie"), None);
    }

    struct Echo;

    impl Transport for Echo {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
            Ok(HttpResponse {
                status: 200,
                body: request.url,
            })
        }
    }

    #[test]
    fn signing_defaults_to_the_caller_and_references_delegate() {
        let echo = Echo;
        let by_ref = &echo;
        assert!(block_on(by_ref.sign_es256(b"k", b"m")).is_none());
        let resp = block_on(by_ref.send(HttpRequest::get("https://x.test/a"))).unwrap();
        assert!(resp.is_success());
        assert_eq!(resp.body, "https://x.test/a");
        assert!(!HttpResponse {
            status: 404,
            body: String::new()
        }
        .is_success());
    }
}
