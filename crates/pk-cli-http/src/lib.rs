//! HTTP plumbing for the piekstra CLI family.
//!
//! A blocking `reqwest` client builder with the family defaults (UA from
//! binary+version, cookie store, timeout) for portal sessions,
//! [`BlockingTransport`] (a contract-keeping client behind
//! `pk_cli_core::transport::Transport`, for provider clients that also run in
//! the Worker), plus the standard raw `api` passthrough command (SPEC v1
//! §1.2). Provider session logic (login dances,
//! scrape flows) stays in each CLI.

use std::time::Duration;

use pk_cli_core::transport::{HttpRequest, HttpResponse, PlatformSigner, Transport};
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
/// `pk_cli_core::block_on`. It has no platform signer
/// ([`PlatformSigner::sign_es256`] is always `None`): the client signs in
/// Rust.
#[derive(Debug, Clone)]
pub struct BlockingTransport {
    client: reqwest::blocking::Client,
}

impl BlockingTransport {
    /// A client that keeps the [`Transport`] contract: no redirects followed,
    /// no cookie store, no default headers (the request sets its own
    /// `User-Agent`, `HttpRequest::user_agent`), and the family's 60 s
    /// timeout.
    pub fn new() -> Result<Self, CliError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CliError::Other(format!("failed to build HTTP client: {e}")))?;
        Ok(Self::with_client(client))
    }

    /// Over a client the CLI built itself (a proxy, say). It must keep the
    /// [`Transport`] contract: `.redirect(reqwest::redirect::Policy::none())`,
    /// no cookie store and no default headers, or the same provider client
    /// behaves differently here than in the Worker.
    pub fn with_client(client: reqwest::blocking::Client) -> Self {
        Self { client }
    }

    fn send_blocking(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        let origin = request.origin().to_string();
        let failed =
            |why: String| CliError::Upstream(format!("the request to {origin} failed: {why}"));
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| failed(format!("`{}` is not an HTTP method", request.method)))?;
        let mut rb = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            rb = rb.header(name.as_str(), value.as_str());
        }
        if let Some(body) = request.body {
            rb = rb.body(body);
        }
        // `without_url`: the path and query can carry ids, as in the Worker.
        let resp = rb.send().map_err(|e| failed(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let headers = fetch_shaped(resp.headers());
        let body = resp
            .text()
            .map_err(|e| failed(format!("reading the response body: {}", e.without_url())))?;
        let mut out = HttpResponse::new(status, body);
        out.headers = headers;
        Ok(out)
    }
}

/// Response headers in the shape `fetch` gives them (`HttpResponse::headers`):
/// names lower-case, a repeated name as one pair with its values joined by
/// `, `, except `set-cookie`, one pair per cookie.
fn fetch_shaped(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.as_str();
        let value = String::from_utf8_lossy(value.as_bytes());
        match out.iter_mut().find(|(n, _)| n == name) {
            Some((_, joined)) if name != "set-cookie" => {
                joined.push_str(", ");
                joined.push_str(&value);
            }
            _ => out.push((name.to_string(), value.into_owned())),
        }
    }
    out
}

impl Transport for BlockingTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
        self.send_blocking(request)
    }
}

impl PlatformSigner for BlockingTransport {
    async fn sign_es256(&self, _key: &[u8], _message: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
        None
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

/// An answer that says nothing about the session a read was made with: the
/// provider or its edge did not judge it. A confirm hook answers
/// `Confirm::Undecided` on this. Any other answer is the CLI's to judge,
/// because a provider's refusal rule is its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inconclusive {
    /// The HTTP status, for the reason the CLI words.
    pub status: u16,
    /// How long the provider asked to wait ([`retry_after`]).
    pub retry_after: Option<Duration>,
}

/// Whether this answer is [`Inconclusive`]: a 429, a 5xx, or a 403 with an HTML
/// page, which is an edge block rather than a provider's refusal. [`json_response`]
/// maps every 403 to exit 3, so a read that decides a session uses this first.
/// It reads the status and headers only, and leaves the body to the caller.
pub fn inconclusive(resp: &reqwest::blocking::Response) -> Option<Inconclusive> {
    inconclusive_answer(resp.status().as_u16(), resp.headers(), unix_now())
}

fn inconclusive_answer(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    now: i64,
) -> Option<Inconclusive> {
    let html = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/html")
        });
    let says_nothing = status == 429 || (500..600).contains(&status) || (status == 403 && html);
    says_nothing.then(|| Inconclusive {
        status,
        retry_after: retry_after_at(headers, now),
    })
}

/// How long a response asks to wait, from its `Retry-After`: delay-seconds, or
/// an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`), which counts from now and
/// is zero once past. `None` when the header is absent or in any other form.
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    retry_after_at(headers, unix_now())
}

fn retry_after_at(headers: &reqwest::header::HeaderMap, now: i64) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = imf_fixdate_unix(value)?;
    Some(Duration::from_secs(at.saturating_sub(now).max(0) as u64))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn a_429_a_5xx_and_a_403_page_say_nothing_about_the_session() {
        let page = headers(&[("content-type", "text/html; charset=utf-8")]);
        let json = headers(&[("content-type", "application/json")]);
        for (status, map, expected) in [
            (429, &json, true),
            (500, &json, true),
            (503, &page, true),
            (403, &page, true),
            (403, &json, false),
            (401, &page, false),
            (404, &page, false),
            (200, &page, false),
            (200, &json, false),
        ] {
            let got = inconclusive_answer(status, map, 0);
            assert_eq!(got.is_some(), expected, "{status} {map:?}");
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
            let got = retry_after_at(&headers(&[("retry-after", value)]), at - 30);
            assert_eq!(got, expected.map(Duration::from_secs), "{value}");
        }
        assert_eq!(retry_after_at(&headers(&[]), at), None);
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
    fn an_inconclusive_answer_carries_the_wait_it_was_given() {
        let map = headers(&[("retry-after", "120")]);
        assert_eq!(
            inconclusive_answer(429, &map, 0),
            Some(Inconclusive {
                status: 429,
                retry_after: Some(Duration::from_secs(120)),
            })
        );
    }

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

    /// Serve one canned HTTP answer on a loopback socket; returns the
    /// address and a handle yielding the raw request it read (up to the
    /// first blank line, plus `body_marker` if given).
    fn serve_once(
        answer: &'static [u8],
        body_marker: Option<&'static str>,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let text = String::from_utf8_lossy(&seen);
                let done = match body_marker {
                    Some(m) => text.contains(m),
                    None => text.contains("\r\n\r\n"),
                };
                if done {
                    break;
                }
                let n = sock.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            sock.write_all(answer).unwrap();
            String::from_utf8_lossy(&seen).into_owned()
        });
        (addr, server)
    }

    /// The method, path, query, headers and body arrive as built, and an
    /// error status comes back as a response, headers included, not an
    /// error.
    #[test]
    fn the_blocking_transport_sends_the_request_as_built() {
        let (addr, server) = serve_once(
            b"HTTP/1.1 418 I'm a teapot\r\ncontent-type: text/plain\r\nretry-after: 7\r\nlink: <a>\r\nset-cookie: a=1\r\nLink: <b>\r\nset-cookie: b=2\r\ncontent-length: 5\r\nconnection: close\r\n\r\nshort",
            Some("{\"q\":1}"),
        );
        let transport = BlockingTransport::new().unwrap();
        let req = HttpRequest::new("POST", format!("http://{addr}/v1/x"))
            .query(&[("a", "b c".into())])
            .header("X-Test", "yes")
            .user_agent("pk-test", "0.0.0")
            .body("{\"q\":1}");
        let resp = pk_cli_core::block_on(transport.send(req)).unwrap();
        assert_eq!(resp.status, 418);
        assert_eq!(resp.body, "short");
        assert_eq!(resp.header_value("Retry-After"), Some("7"));
        // Shaped as `fetch` gives them: a repeated name joined, cookies apart.
        assert_eq!(resp.header_value("link"), Some("<a>, <b>"));
        let cookies: Vec<&str> = resp
            .headers
            .iter()
            .filter(|(n, _)| n == "set-cookie")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(cookies, ["a=1", "b=2"]);
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /v1/x?a=b%20c HTTP/1.1"), "{seen}");
        assert!(seen.to_ascii_lowercase().contains("x-test: yes"), "{seen}");
        assert!(seen.contains("pk-test/0.0.0"), "{seen}");
        assert!(pk_cli_core::block_on(transport.sign_es256(b"k", b"m")).is_none());
    }

    /// The transport contract: a redirect is handed back, not followed (the
    /// signed request is never re-sent to wherever it points).
    #[test]
    fn a_redirect_comes_back_as_the_response() {
        let (addr, server) = serve_once(
            b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            None,
        );
        let transport = BlockingTransport::new().unwrap();
        let req = HttpRequest::get(format!("http://{addr}/start")).header("Authorization", "t");
        let resp = pk_cli_core::block_on(transport.send(req)).unwrap();
        // Nothing the request did not say: no User-Agent, no cookie.
        let seen = server.join().unwrap().to_ascii_lowercase();
        assert!(!seen.contains("user-agent"), "{seen}");
        assert!(!seen.contains("cookie"), "{seen}");
        assert_eq!(resp.status, 302);
        assert_eq!(
            resp.header_value("location"),
            Some("http://127.0.0.1:9/elsewhere")
        );
    }

    /// A request that got no HTTP answer is `Upstream`, naming the origin
    /// only, whether the platform refused it or nothing answered.
    #[test]
    fn a_bad_method_or_no_connection_is_upstream_naming_the_origin() {
        let transport = BlockingTransport::new().unwrap();
        let bad = HttpRequest::new("NOT A METHOD", "http://127.0.0.1:9/a?id=secret");
        // Port 9 (discard) on loopback: nothing listens there in CI.
        let refused = HttpRequest::get("http://127.0.0.1:9/a?id=secret");
        for req in [bad, refused] {
            match pk_cli_core::block_on(transport.send(req)) {
                Err(CliError::Upstream(m)) => {
                    assert!(
                        m.starts_with("the request to http://127.0.0.1:9 failed: "),
                        "{m}"
                    );
                    assert!(!m.contains("secret") && !m.contains("/a"), "{m}");
                }
                other => panic!("expected Upstream, got {other:?}"),
            }
        }
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
