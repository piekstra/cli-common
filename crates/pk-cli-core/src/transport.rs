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
//! The traits are async because the Worker's side has to be. A blocking
//! implementation's futures are ready on their first poll, so native code
//! drives them with [`crate::block_on()`] at no cost.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use crate::CliError;

/// One outgoing request, fully described: the transport adds nothing but a
/// connection. Build it with [`HttpRequest::get`] or [`HttpRequest::new`];
/// [`HttpRequest::query`] appends percent-encoded parameters to the URL.
///
/// `Debug` shows the method, the URL without its query, the header names and
/// the body's length: header values and bodies carry tokens and signatures.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
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

    /// The family's `User-Agent: <bin>/<version>`. Part of the request, not
    /// the transport, so the binary and the Worker send the same one.
    pub fn user_agent(self, binary: &str, version: &str) -> Self {
        self.header("User-Agent", format!("{binary}/{version}"))
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// The value of the first header called `name` (case-insensitive).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }

    /// Where the request goes, for messages: the scheme and host only, no
    /// userinfo. The path and query can carry account ids and tokens.
    pub fn origin(&self) -> String {
        origin_and_path(&self.url).0
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (origin, path) = origin_and_path(&self.url);
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &format!("{origin}{path}"))
            .field("headers", &names(&self.headers))
            .field("body_len", &self.body.as_ref().map(String::len))
            .finish()
    }
}

/// What came back: the status, the headers and the body as text. The body is
/// text, not parsed JSON, because providers answer some failures with a
/// plain-text body, and the client decides how to read it.
///
/// A redirect is a response like any other (see [`Transport`]): its
/// `Location` is in [`HttpResponse::header_value`].
///
/// `Debug` shows the status, the header names and the body's length.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpResponse {
    pub status: u16,
    /// Header name and value pairs, names lower-case, as `fetch` gives them:
    /// a name that repeats is one pair with its values joined by `, `, except
    /// `set-cookie`, one pair per cookie.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The value of the first header called `name` (case-insensitive).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Whether this answer says nothing about the session a read was made with
    /// ([`inconclusive_answer`]). `now` is Unix seconds, for a `Retry-After`
    /// given as a date: a Worker has no system clock.
    pub fn inconclusive(&self, now: i64) -> Option<Inconclusive> {
        inconclusive_answer(
            self.status,
            self.header_value("content-type"),
            self.header_value("retry-after"),
            now,
        )
    }

    /// How long this answer asks to wait ([`parse_retry_after`]). `now` is Unix
    /// seconds.
    pub fn retry_after(&self, now: i64) -> Option<Duration> {
        parse_retry_after(self.header_value("retry-after")?, now)
    }
}

/// An answer that says nothing about the session a read was made with: the
/// provider or its edge did not judge it. A browser sign-in's confirm hook answers
/// `Confirm::Undecided` on this. Any other answer is the CLI's to judge, because a
/// provider's refusal rule is its own, and a 2xx is not acceptance until its body
/// shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Inconclusive {
    /// The HTTP status, for the reason the CLI words.
    pub status: u16,
    /// How long the provider asked to wait ([`parse_retry_after`]).
    pub retry_after: Option<Duration>,
}

/// A 429, a 5xx, or a 403 with an HTML page, which is an edge block rather than a
/// provider's refusal. It takes the status and two header values, and leaves the
/// body to the caller. `now` is Unix seconds.
pub fn inconclusive_answer(
    status: u16,
    content_type: Option<&str>,
    retry_after: Option<&str>,
    now: i64,
) -> Option<Inconclusive> {
    let html = content_type.is_some_and(|value| {
        value
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("text/html")
    });
    let says_nothing = status == 429 || (500..600).contains(&status) || (status == 403 && html);
    says_nothing.then(|| Inconclusive {
        status,
        retry_after: retry_after.and_then(|value| parse_retry_after(value, now)),
    })
}

/// How long a `Retry-After` value asks to wait: delay-seconds, or an IMF-fixdate
/// (`Wed, 21 Oct 2015 07:28:00 GMT`), which counts from `now`, Unix seconds, and is
/// zero once past. `None` for any other form.
pub fn parse_retry_after(value: &str, now: i64) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = imf_fixdate_unix(value)?;
    Some(Duration::from_secs(at.saturating_sub(now).max(0) as u64))
}

/// `Wed, 21 Oct 2015 07:28:00 GMT` as Unix seconds.
fn imf_fixdate_unix(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    let [_weekday, day, month, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|name| name == month)? as i64 + 1;
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let mut clock = time.split(':').map(str::parse::<i64>);
    let (hours, minutes, seconds) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    if clock.next().is_some()
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hours)
        || !(0..60).contains(&minutes)
        || !(0..=60).contains(&seconds)
    {
        return None;
    }
    // Days from the civil date, by shifting the year to start in March.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &names(&self.headers))
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Something that can send an [`HttpRequest`]. Every implementation keeps
/// the same contract, so a client behaves the same in the binary and in the
/// Worker:
///
/// - **No redirects are followed.** A 3xx comes back as the response, with
///   its `Location` header: a signed request is never re-sent to wherever a
///   redirect points.
/// - **No state is kept between sends, and nothing identifies the
///   caller but the request.** No cookie jar, no session or identifying
///   headers (not even a `User-Agent`: [`HttpRequest::user_agent`] sets the
///   family's). The platform may add protocol defaults (`Host`,
///   `Content-Length`, connection handling, a default `Accept`), so a client
///   that depends on a header's value sets it.
/// - **Response headers have one shape**: see [`HttpResponse::headers`].
/// - **Only a request that got no HTTP answer is an error**, and it is
///   [`CliError::Upstream`] naming the origin only (no path, no query): no
///   connection, a timeout, or a request the platform refused to send (a
///   malformed method or URL). An HTTP error status is a response, for the
///   client to map.
///
/// No `Send` bound, as in `pk_cli_mcp::Toolset`: the Worker is
/// single-threaded and its futures hold JavaScript values, and the native
/// binary polls on the calling thread.
#[allow(async_fn_in_trait)] // deliberate: no `Send` bound, for wasm hosts
pub trait Transport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError>;
}

impl<T: Transport + ?Sized> Transport for &T {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        (**self).send(request).await
    }
}

/// A platform's own ECDSA P-256 signer, for a client that signs its requests
/// (a CDP or other JWT). Separate from [`Transport`] so a wrapper around a
/// transport cannot silently drop it: a client that signs is bounded on
/// both, and a type that implements only one does not compile there.
#[allow(async_fn_in_trait)] // deliberate: no `Send` bound, for wasm hosts
pub trait PlatformSigner {
    /// ECDSA P-256 with SHA-256 over `message`, as the fixed-width 64-byte
    /// `r || s` a JWS carries. `key` is the private key as PKCS#8 DER.
    ///
    /// `None` means the platform offers nothing faster than pure Rust, and
    /// the caller signs itself; so may a caller whose platform signer
    /// failed. A Worker's WebCrypto signs in native code, well under a tenth
    /// of the CPU a WebAssembly signature costs against the free plan's
    /// 10 ms per request.
    async fn sign_es256(&self, key: &[u8], message: &[u8]) -> Option<Result<Vec<u8>, CliError>>;
}

impl<T: PlatformSigner + ?Sized> PlatformSigner for &T {
    async fn sign_es256(&self, key: &[u8], message: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
        (**self).sign_es256(key, message).await
    }
}

/// `url`'s scheme and host (any userinfo dropped), and its path (no query or
/// fragment): what a message or a `Debug` may show of it.
fn origin_and_path(url: &str) -> (String, &str) {
    let (scheme, rest) = match url.find("://") {
        Some(i) => url.split_at(i + 3),
        None => ("", url),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, after) = rest.split_at(end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path = &after[..after.find(['?', '#']).unwrap_or(after.len())];
    (format!("{scheme}{host}"), path)
}

/// Refuse a URL a credential must not travel to: anything but `https`, except
/// plain `http` to this machine, which a stub or a local tunnel uses.
///
/// Call it before sending when the URL or host came from outside the binary:
/// a raw `api` passthrough, a host override, a URL in a provider's response.
/// The message names only the scheme and host, since the rest can carry an
/// account id or a token.
pub fn require_secure_url(url: &str) -> Result<(), CliError> {
    if is_secure_url(url) {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "refusing to send a request to {}: only https is allowed, or http to this machine",
        origin_and_path(url).0
    )))
}

/// The redirect policy to pair with [`require_secure_url`]: reqwest's default
/// limit of ten, and an error on a redirect that leaves `https`. reqwest keeps
/// a provider's own auth headers (anything but `Authorization` and `Cookie`)
/// across a redirect, and sends a jar's cookies that are not marked `Secure`.
#[cfg(feature = "reqwest")]
pub fn secure_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if is_secure_url(attempt.url().as_str()) {
            attempt.follow()
        } else {
            attempt.error("refusing a redirect that leaves https")
        }
    })
}

fn is_secure_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    // `\\` ends the authority too: reqwest's URL parser reads it as `/`, so
    // `http://example.com\\@127.0.0.1/` goes to example.com.
    let end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
    scheme.eq_ignore_ascii_case("http") && is_loopback_authority(&rest[..end])
}

/// Anything this cannot read as a loopback host is not one, so a spelling the
/// URL parser would resolve differently is refused rather than guessed at.
fn is_loopback_authority(authority: &str) -> bool {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if let Some(bracketed) = host.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .is_some_and(|(ip, _)| ip.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback()));
    }
    let name = host.split_once(':').map_or(host, |(name, _)| name);
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn names(headers: &[(String, String)]) -> Vec<&str> {
    headers.iter().map(|(n, _)| n.as_str()).collect()
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

    #[test]
    fn https_passes_and_http_passes_only_to_this_machine() {
        for url in [
            "https://www.example.com/x?token=dummy",
            "HTTPS://www.example.com/",
            "http://127.0.0.1:8080/x",
            "http://127.8.8.8/x",
            "http://localhost/x",
            "HTTP://LOCALHOST:9/x",
            "http://[::1]:8080/x",
            "http://user@127.0.0.1/x",
        ] {
            assert!(require_secure_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for url in [
            "http://www.example.com/x",
            "http://10.0.0.1/x",
            "http://0.0.0.0/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://[2001:db8::1]/x",
            // A loopback name only in the userinfo, the subdomain or the path.
            "http://127.0.0.1@example.com/x",
            "http://127.0.0.1:80@example.com/x",
            "http://localhost.example.com/x",
            "http://127.0.0.1.example.com/x",
            "http://example.com/127.0.0.1",
            "http://example.com?127.0.0.1",
            "http://example.com#@127.0.0.1",
            // The URL parser reads a backslash as a slash, and strips tabs.
            "http://example.com\\@127.0.0.1/",
            "http://127.0.0.1\t.example.com/",
            // Spellings of loopback this does not try to resolve.
            "http://0x7f.0.0.1/x",
            "http://2130706433/x",
            "ftp://127.0.0.1/x",
            "file:///etc/passwd",
            "www.example.com/x",
            "",
        ] {
            assert!(require_secure_url(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_host_and_not_the_rest_of_the_url() {
        let err = require_secure_url("http://user:dummy-pass@example.com/a/1234?token=dummy-token")
            .unwrap_err();
        let message = err.to_string();
        assert_eq!(err.exit_code(), 2);
        assert!(message.contains("http://example.com"), "{message}");
        for leak in ["dummy-pass", "user", "1234", "dummy-token"] {
            assert!(!message.contains(leak), "{leak} printed: {message}");
        }
    }

    /// A loopback server that answers one request with a redirect to `location`.
    #[cfg(feature = "reqwest")]
    fn redirecting_server(location: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut head = [0u8; 1024];
            let _ = sock.read(&mut head);
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = sock.write_all(reply.as_bytes());
        });
        base
    }

    #[cfg(feature = "reqwest")]
    #[test]
    fn a_redirect_off_https_is_an_error_before_the_next_hop() {
        let client = reqwest::blocking::Client::builder()
            .redirect(secure_redirects())
            .build()
            .expect("client");
        let base = redirecting_server("http://www.example.invalid/next");
        let err = client.get(format!("{base}/x")).send().unwrap_err();
        assert!(err.is_redirect(), "{err}");
        assert!(format!("{err:?}").contains("leaves https"), "{err:?}");
    }

    #[test]
    fn a_429_a_5xx_and_a_403_page_say_nothing_about_the_session() {
        let page = Some("text/html; charset=utf-8");
        let json = Some("application/json");
        for (status, kind, expected) in [
            (429, json, true),
            (500, json, true),
            (503, page, true),
            (403, page, true),
            (403, json, false),
            (401, page, false),
            (404, page, false),
            (200, page, false),
            (200, json, false),
        ] {
            let got = inconclusive_answer(status, kind, None, 0);
            assert_eq!(got.is_some(), expected, "{status} {kind:?}");
            if let Some(answer) = got {
                assert_eq!(answer.status, status);
            }
        }
    }

    #[test]
    fn a_retry_after_reads_in_seconds_or_as_a_date() {
        // Wed, 21 Oct 2015 07:28:00 GMT.
        let at = 1_445_412_480;
        for (value, expected) in [
            ("120", Some(120)),
            (" 0 ", Some(0)),
            ("Wed, 21 Oct 2015 07:28:00 GMT", Some(30)),
            ("Wed, 21 Oct 2015 07:27:00 GMT", Some(0)),
            ("Thu, 01 Jan 1970 00:00:00 GMT", Some(0)),
            ("soon", None),
            ("-5", None),
            ("Wed, 21 Oct 2015 07:28:00 PST", None),
            ("Wed, 21 Foo 2015 07:28:00 GMT", None),
            ("Wed, 32 Oct 2015 07:28:00 GMT", None),
            ("Wed, 21 Oct 2015 25:28:00 GMT", None),
        ] {
            let got = parse_retry_after(value, at - 30);
            assert_eq!(got, expected.map(Duration::from_secs), "{value}");
        }
    }

    #[test]
    fn an_imf_fixdate_lands_on_the_known_second() {
        for (value, expected) in [
            ("Thu, 01 Jan 1970 00:00:00 GMT", 0),
            ("Wed, 21 Oct 2015 07:28:00 GMT", 1_445_412_480),
            ("Sat, 29 Feb 2020 12:00:00 GMT", 1_582_977_600),
            ("Sun, 01 Mar 2020 00:00:00 GMT", 1_583_020_800),
        ] {
            assert_eq!(imf_fixdate_unix(value), Some(expected), "{value}");
        }
    }

    #[test]
    fn a_response_judges_itself_through_its_headers() {
        let mut answer = HttpResponse::new(429, "");
        answer.headers = vec![("Retry-After".into(), "120".into())];
        let said = answer.inconclusive(0).unwrap();
        assert_eq!(said.status, 429);
        assert_eq!(said.retry_after, Some(Duration::from_secs(120)));
        assert_eq!(answer.retry_after(0), Some(Duration::from_secs(120)));
        let mut block = HttpResponse::new(403, "");
        block.headers = vec![("content-type".into(), "text/html".into())];
        assert!(block.inconclusive(0).is_some());
        assert!(HttpResponse::new(403, "{}").inconclusive(0).is_none());
        assert!(HttpResponse::new(200, "").inconclusive(0).is_none());
    }
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
        let r = HttpRequest::get("https://x.test").user_agent("pk-test", "1.2.3");
        assert_eq!(r.header_value("user-agent"), Some("pk-test/1.2.3"));
        let resp = HttpResponse::new(302, "").header("location", "https://x.test/next");
        assert_eq!(resp.header_value("Location"), Some("https://x.test/next"));
    }

    #[test]
    fn the_origin_is_scheme_and_host_without_userinfo_path_or_query() {
        for (url, want, path) in [
            ("https://h.test/v2/a?x=1", "https://h.test", "/v2/a"),
            ("https://h.test?token=t", "https://h.test", ""),
            ("https://h.test#frag", "https://h.test", ""),
            ("https://user:token@h.test/a", "https://h.test", "/a"),
            ("https://h.test/a@b?c=@d", "https://h.test", "/a@b"),
            ("http://127.0.0.1:8788", "http://127.0.0.1:8788", ""),
            ("not a url/x", "not a url", "/x"),
        ] {
            assert_eq!(origin_and_path(url), (want.to_string(), path), "{url}");
            assert_eq!(HttpRequest::get(url).origin(), want, "{url}");
        }
    }

    #[test]
    fn debug_never_shows_a_header_value_query_body_or_userinfo() {
        let r = HttpRequest::get("https://u:secret-u@h.test/v2/a?token=secret-q")
            .header("Authorization", "Bearer secret-h")
            .body("secret-b");
        let resp = HttpResponse::new(200, "secret-r").header("set-cookie", "secret-c");
        let shown = format!("{r:?} {resp:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("https://h.test/v2/a"), "{shown}");
        assert!(
            shown.contains("Authorization") && shown.contains("set-cookie"),
            "{shown}"
        );
    }

    struct Echo;

    impl Transport for Echo {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
            Ok(HttpResponse::new(200, request.url))
        }
    }

    struct NoSigner;

    impl PlatformSigner for NoSigner {
        async fn sign_es256(&self, _: &[u8], _: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
            None
        }
    }

    #[test]
    fn references_delegate() {
        let echo = Echo;
        let by_ref = &echo;
        let resp = block_on(by_ref.send(HttpRequest::get("https://x.test/a"))).unwrap();
        assert!(resp.is_success());
        assert_eq!(resp.body, "https://x.test/a");
        assert!(!HttpResponse::new(404, "").is_success());
        let signer = &NoSigner;
        assert!(block_on(<&NoSigner as PlatformSigner>::sign_es256(
            &signer, b"k", b"m"
        ))
        .is_none());
    }
}
