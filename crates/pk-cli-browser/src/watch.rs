//! Watching a capture's own pages for the requests they make.
//!
//! A capture that asks for a request ([`Capture::request`],
//! [`Capture::optional_request`]) or a request log
//! ([`Capture::request_log`]) needs the network events of its pages, which
//! only reach a DevTools connection that stays attached between polls. So
//! the wait keeps one more connection open for those captures, the watch:
//! it attaches (flattened) to each page on the capture's hosts, enables
//! `Network` in that page and in no other, and detaches from a page as soon
//! as the page leaves those hosts or the capture is read. A page on the
//! login host is never attached to. Nothing here drives a page or sends a
//! request; the events are only read.
//!
//! What comes out is typed and narrow: one request's method, URL (query and
//! fragment cut) and headers (values as [`Secret`]s), and, for a log, each
//! endpoint's method, host, path, kind, status and content type, with no
//! header, body or query at all. A capture that keeps responses
//! ([`Capture::responses`]) also gets the body of each response its pages
//! loaded on the paths it names: once the load finishes, the watch reads the
//! body the browser already holds (`Network.getResponseBody`), which sends
//! nothing to the site and which no page can observe. A load whose
//! transferred size already exceeds the body bound is not read at all; a
//! read the browser does not answer in time counts as a skipped load and
//! leaves the watch up; and a watch that has to reconnect counts the loads
//! still in progress as skipped, since their bodies went with the session.
//!
//! [`Capture::request`]: crate::Capture::request
//! [`Capture::optional_request`]: crate::Capture::optional_request
//! [`Capture::request_log`]: crate::Capture::request_log
//! [`Capture::responses`]: crate::Capture::responses

use std::collections::HashMap;
use std::fmt;

use pk_cli_secrets::Secret;
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use crate::capture::PageTarget;
use crate::cdp::{Cdp, CdpError, Transport};
#[cfg(feature = "test-util")]
use crate::host::normalize_host;
use crate::host::{parse_url, HostRule, UrlParts};
use crate::jar::path_matches;

/// Distinct endpoints one request log keeps; later ones are dropped.
const MAX_LOGGED: usize = 500;
/// Requests a log remembers while it waits for their responses.
const MAX_PENDING: usize = 2_000;
/// Responses one capture keeps; later ones are counted as skipped.
pub(crate) const MAX_RESPONSES: usize = 500;
/// The largest body kept; a larger one is counted as skipped. Base64 and
/// the CDP envelope put a body this size under the websocket's 16 MiB frame
/// limit, so reading one never breaks the watch connection.
pub(crate) const MAX_BODY: usize = 10 * 1024 * 1024;
/// The most body bytes one capture keeps in all.
pub(crate) const MAX_BODIES: usize = 200 * 1024 * 1024;

/// The bounds a [`ResponseLog`] keeps within.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub responses: usize,
    pub body: usize,
    pub total: usize,
}

const LIMITS: Limits = Limits {
    responses: MAX_RESPONSES,
    body: MAX_BODY,
    total: MAX_BODIES,
};

/// The first request a capture's pages made to the URL it watches for.
///
/// The headers are the ones the page set, as `Network.requestWillBeSent`
/// reports them; the `Cookie` header and the others the network stack adds
/// on its own are not among them (read the jar for the cookies).
#[derive(Debug)]
pub struct CapturedRequest {
    method: String,
    url: String,
    headers: Vec<(String, Secret)>,
}

impl CapturedRequest {
    pub(crate) fn new(method: &str, url: &UrlParts, headers: &Map<String, Value>) -> Self {
        let mut headers: Vec<(String, Secret)> = headers
            .iter()
            .filter_map(|(name, value)| {
                Some((name.to_ascii_lowercase(), Secret::new(value.as_str()?)))
            })
            .collect();
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        CapturedRequest {
            method: method.to_ascii_uppercase(),
            url: format!("https://{}{}", url.host, url.path),
            headers,
        }
    }

    /// The method, uppercase (`POST`).
    pub fn method(&self) -> &str {
        &self.method
    }

    /// `https://<host><path>`: the query and fragment are cut, so a token
    /// in them is never handed over.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The headers the page set, names lowercased, sorted by name.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &Secret)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    /// One header by name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&Secret> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }
}

#[cfg(feature = "test-util")]
impl CapturedRequest {
    /// A watched request as a capture hands it over, for a CLI's unit tests
    /// (the `test-util` feature, a dev-dependency only; see the crate docs).
    /// It goes through the constructor a request the crate saw does: the
    /// method uppercased, `url` cut to `https://<host><path>`, header names
    /// lowercased and sorted, and each value wrapped in a [`Secret`]. Of two
    /// headers spelled exactly alike, the later is kept.
    ///
    /// # Panics
    ///
    /// If `url` is not an `https` URL on the default port, the only kind a
    /// capture watches for.
    pub fn for_test(method: &str, url: &str, headers: &[(&str, &str)]) -> Self {
        let Some(parts) = parse_url(url).filter(UrlParts::is_https_default_port) else {
            panic!("CapturedRequest::for_test: the URL is not https on the default port");
        };
        let headers: Map<String, Value> = headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), Value::from(*value)))
            .collect();
        CapturedRequest::new(method, &parts, &headers)
    }
}

/// What kind of request a [`LoggedRequest`] was, as the browser classifies
/// it. Only these three are logged: a page load, and the app's own data
/// calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RequestKind {
    /// A page (or frame) load.
    Document,
    /// An `XMLHttpRequest`.
    Xhr,
    /// A `fetch()`.
    Fetch,
}

impl RequestKind {
    /// The browser's own name for it: `Document`, `XHR` or `Fetch`.
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestKind::Document => "Document",
            RequestKind::Xhr => "XHR",
            RequestKind::Fetch => "Fetch",
        }
    }

    fn from_cdp(kind: &str) -> Option<Self> {
        match kind {
            "Document" => Some(RequestKind::Document),
            "XHR" => Some(RequestKind::Xhr),
            "Fetch" => Some(RequestKind::Fetch),
            _ => None,
        }
    }
}

impl fmt::Display for RequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One endpoint a capture's pages called, for a [`Capture::request_log`].
/// It carries no header, no body and no query string, so nothing in it is
/// secret. Requests that differ only in their query are one entry.
///
/// [`Capture::request_log`]: crate::Capture::request_log
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedRequest {
    method: String,
    host: String,
    path: String,
    kind: RequestKind,
    status: Option<u16>,
    mime: Option<String>,
}

impl LoggedRequest {
    /// The method, uppercase.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The host it went to, lowercase.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The path, without the query or fragment.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn kind(&self) -> RequestKind {
        self.kind
    }

    /// The first response's status, if one arrived while the log was kept.
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// The first response's content type, if one arrived.
    pub fn mime(&self) -> Option<&str> {
        self.mime.as_deref()
    }
}

#[cfg(feature = "test-util")]
impl LoggedRequest {
    /// A request-log entry as a capture hands it over, for a CLI's unit
    /// tests (the `test-util` feature, a dev-dependency only; see the crate
    /// docs). Normalized as a logged one is: the method uppercased, the host
    /// lowercased, and the path cut at its query or fragment. `status` and
    /// `mime` are the first response's, `None` for a request whose response
    /// never came.
    ///
    /// # Panics
    ///
    /// If `host` is not a host name, or `path` does not start with `/`.
    pub fn for_test(
        method: &str,
        host: &str,
        path: &str,
        kind: RequestKind,
        status: Option<u16>,
        mime: Option<&str>,
    ) -> Self {
        let Some(normalized) = normalize_host(host) else {
            panic!("LoggedRequest::for_test: `{host}` is not a host name");
        };
        assert!(
            path.starts_with('/'),
            "LoggedRequest::for_test: the path `{path}` does not start with `/`"
        );
        LoggedRequest {
            method: method.to_ascii_uppercase(),
            host: normalized,
            path: path.split(['?', '#']).next().unwrap_or("/").to_string(),
            kind,
            status,
            mime: mime.map(str::to_string),
        }
    }
}

/// The URL a capture watches for: an `https` host on the default port and a
/// path, matched as a cookie path is (the path itself, or anything under it
/// on a `/` boundary). Any query matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestRule {
    host: String,
    path: String,
}

impl RequestRule {
    /// `None` unless `url` is `https` on the default port with no query or
    /// fragment.
    pub fn parse(url: &str) -> Option<Self> {
        if url.contains(['?', '#']) {
            return None;
        }
        let u = parse_url(url).filter(UrlParts::is_https_default_port)?;
        Some(RequestRule {
            host: u.host,
            path: u.path,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn matches(&self, url: &UrlParts) -> bool {
        url.is_https_default_port() && url.host == self.host && path_matches(&url.path, &self.path)
    }
}

impl fmt::Display for RequestRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.host, self.path)
    }
}

/// The endpoints a capture's pages called, deduplicated.
#[derive(Debug)]
pub(crate) struct RequestLog {
    rule: HostRule,
    entries: Vec<LoggedRequest>,
    /// The browser's request id → the entry its response fills in.
    pending: HashMap<String, usize>,
}

impl RequestLog {
    pub fn new(rule: HostRule) -> Self {
        RequestLog {
            rule,
            entries: Vec::new(),
            pending: HashMap::new(),
        }
    }

    /// The domain it logs, normalized.
    pub fn domain(&self) -> &str {
        self.rule.host()
    }

    pub fn entries(&self) -> &[LoggedRequest] {
        &self.entries
    }

    /// Log a request if it is a page load or a data call to an `https`
    /// host on the log's domain.
    pub fn observe(&mut self, id: &str, method: &str, url: &UrlParts, kind: &str) {
        let Some(kind) = RequestKind::from_cdp(kind) else {
            return;
        };
        if !url.is_https_default_port() || !self.rule.matches_host(&url.host) {
            return;
        }
        let method = method.to_ascii_uppercase();
        let index = match self
            .entries
            .iter()
            .position(|e| e.method == method && e.host == url.host && e.path == url.path)
        {
            Some(i) => i,
            None if self.entries.len() < MAX_LOGGED => {
                self.entries.push(LoggedRequest {
                    method,
                    host: url.host.clone(),
                    path: url.path.clone(),
                    kind,
                    status: None,
                    mime: None,
                });
                self.entries.len() - 1
            }
            None => return,
        };
        if self.pending.len() >= MAX_PENDING {
            self.pending.clear();
        }
        self.pending.insert(id.to_string(), index);
    }

    /// Fill in the status and content type of the entry `id` was logged
    /// under, if it has none yet.
    pub fn respond(&mut self, id: &str, status: Option<u16>, mime: Option<String>) {
        let Some(index) = self.pending.remove(id) else {
            return;
        };
        if let Some(entry) = self.entries.get_mut(index) {
            entry.status = entry.status.or(status);
            if entry.mime.is_none() {
                entry.mime = mime;
            }
        }
    }
}

/// One response a capture's pages loaded, body and all, for a
/// [`Capture::responses`]: a page load, an `XMLHttpRequest` or a `fetch()`
/// on the paths the capture names. The URL comes without its query, as a
/// logged request's does. The body is the bytes the browser received
/// (decoded, as the page saw them), handed over in memory and zeroized when
/// dropped; `Debug` shows its length only. The zeroizing is best effort: the
/// websocket message it arrived in and the CDP client's parse of it are
/// ordinary buffers, though the text the body is decoded from is zeroized.
/// It is whatever the site sent, which on a signed-in app can include a
/// token, so a CLI names the paths that serve the data it needs, and writes
/// nothing it did not ask for.
///
/// [`Capture::responses`]: crate::Capture::responses
pub struct CapturedResponse {
    method: String,
    host: String,
    path: String,
    kind: RequestKind,
    status: Option<u16>,
    mime: Option<String>,
    body: Zeroizing<Vec<u8>>,
}

impl CapturedResponse {
    /// The method, uppercase.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The host it came from, lowercase.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The path, without the query or fragment.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn kind(&self) -> RequestKind {
        self.kind
    }

    /// The response's status, if the browser reported one.
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// The response's content type, if the browser reported one.
    pub fn mime(&self) -> Option<&str> {
        self.mime.as_deref()
    }

    /// The body, as the page received it.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for CapturedResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedResponse")
            .field("method", &self.method)
            .field("host", &self.host)
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("status", &self.status)
            .field("mime", &self.mime)
            .field("body_len", &self.body.len())
            .finish()
    }
}

#[cfg(feature = "test-util")]
impl CapturedResponse {
    /// A kept response as a capture hands it over, for a CLI's unit tests
    /// (the `test-util` feature, a dev-dependency only; see the crate docs).
    /// Normalized as a kept one is: the method uppercased, the host
    /// lowercased, and the path cut at its query or fragment.
    ///
    /// # Panics
    ///
    /// If `host` is not a host name, or `path` does not start with `/`.
    pub fn for_test(
        method: &str,
        host: &str,
        path: &str,
        kind: RequestKind,
        status: Option<u16>,
        mime: Option<&str>,
        body: &[u8],
    ) -> Self {
        let Some(normalized) = normalize_host(host) else {
            panic!("CapturedResponse::for_test: `{host}` is not a host name");
        };
        assert!(
            path.starts_with('/'),
            "CapturedResponse::for_test: the path `{path}` does not start with `/`"
        );
        CapturedResponse {
            method: method.to_ascii_uppercase(),
            host: normalized,
            path: path.split(['?', '#']).next().unwrap_or("/").to_string(),
            kind,
            status,
            mime: mime.map(str::to_string),
            body: Zeroizing::new(body.to_vec()),
        }
    }
}

/// A request whose response a capture keeps, until its load finishes.
#[derive(Debug)]
struct PendingResponse {
    method: String,
    host: String,
    path: String,
    kind: RequestKind,
    status: Option<u16>,
    mime: Option<String>,
}

/// The responses a capture keeps: the requests on its domain and paths
/// still loading, and the bodies of those that finished.
#[derive(Debug)]
pub(crate) struct ResponseLog {
    rule: HostRule,
    paths: Vec<String>,
    limits: Limits,
    pending: HashMap<String, PendingResponse>,
    kept: Vec<CapturedResponse>,
    bytes: usize,
    skipped: usize,
}

impl ResponseLog {
    pub fn new(rule: HostRule, paths: Vec<String>) -> Self {
        Self::with_limits(rule, paths, LIMITS)
    }

    pub fn with_limits(rule: HostRule, paths: Vec<String>, limits: Limits) -> Self {
        ResponseLog {
            rule,
            paths,
            limits,
            pending: HashMap::new(),
            kept: Vec::new(),
            bytes: 0,
            skipped: 0,
        }
    }

    /// The domain it keeps responses from, normalized.
    pub fn domain(&self) -> &str {
        self.rule.host()
    }

    /// Note a request if it is a page load or a data call to an `https`
    /// host on the domain, on one of the paths. A request under an id seen
    /// before (a redirect) replaces it, and one that no longer matches
    /// drops it.
    pub fn observe(&mut self, id: &str, method: &str, url: &UrlParts, kind: &str) {
        self.pending.remove(id);
        let Some(kind) = RequestKind::from_cdp(kind) else {
            return;
        };
        if !url.is_https_default_port()
            || !self.rule.matches_host(&url.host)
            || !self.paths.iter().any(|p| path_matches(&url.path, p))
        {
            return;
        }
        if self.pending.len() >= MAX_PENDING {
            // Loads that never finish (a page that left the hosts mid-load)
            // would otherwise pile up; they count as skipped, not lost.
            self.abandon_pending();
        }
        self.pending.insert(
            id.to_string(),
            PendingResponse {
                method: method.to_ascii_uppercase(),
                host: url.host.clone(),
                path: url.path.clone(),
                kind,
                status: None,
                mime: None,
            },
        );
    }

    /// Record the status and content type of a noted request's response.
    pub fn respond(&mut self, id: &str, status: Option<u16>, mime: Option<String>) {
        if let Some(pending) = self.pending.get_mut(id) {
            pending.status = status;
            pending.mime = mime;
        }
    }

    /// Whether the load `id` is one it waits on.
    pub fn is_pending(&self, id: &str) -> bool {
        self.pending.contains_key(id)
    }

    /// Whether another body could still be kept, so it is worth reading.
    pub fn has_room(&self) -> bool {
        self.kept.len() < self.limits.responses && self.bytes < self.limits.total
    }

    /// Whether the finished load `id` is worth reading: one it waits on,
    /// with room left, and not already larger on the wire (`encoded`, the
    /// browser's `encodedDataLength`) than a body it would keep. A body
    /// decoded larger than it was sent is still checked once read; one that
    /// decodes past the websocket's 16 MiB frame (a compressed body of some
    /// 12 MiB or more) breaks the watch connection instead, which then
    /// reconnects and counts every load in progress as skipped, with a
    /// status line.
    pub fn worth_reading(&self, id: &str, encoded: Option<u64>) -> bool {
        self.is_pending(id)
            && self.has_room()
            && encoded.is_none_or(|n| n <= self.limits.body as u64)
    }

    /// Count every load still in progress as skipped: the watch lost its
    /// session with their pages, and their bodies with it. Returns how many.
    pub fn abandon_pending(&mut self) -> usize {
        let n = self.pending.len();
        self.skipped += n;
        self.pending.clear();
        n
    }

    /// Drop a noted request whose load failed: there is no body to keep.
    pub fn forget(&mut self, id: &str) {
        self.pending.remove(id);
    }

    /// Keep the body of the finished load `id`, or count it as skipped
    /// when there is none (the browser no longer held it) or no room.
    pub fn keep(&mut self, id: &str, body: Option<Vec<u8>>) {
        let Some(p) = self.pending.remove(id) else {
            return;
        };
        let limits = self.limits;
        let fits = |body: &Vec<u8>| {
            self.kept.len() < limits.responses
                && body.len() <= limits.body
                && self.bytes + body.len() <= limits.total
        };
        match body {
            Some(body) if fits(&body) => {
                self.bytes += body.len();
                self.kept.push(CapturedResponse {
                    method: p.method,
                    host: p.host,
                    path: p.path,
                    kind: p.kind,
                    status: p.status,
                    mime: p.mime,
                    body: Zeroizing::new(body),
                });
            }
            Some(body) => {
                drop(Zeroizing::new(body));
                self.skipped += 1;
            }
            None => self.skipped += 1,
        }
    }

    /// The kept responses, in the order their loads finished, and how many
    /// were skipped, counting the loads still in progress; the log is empty
    /// afterwards.
    pub fn take(&mut self) -> (Vec<CapturedResponse>, usize) {
        self.abandon_pending();
        self.bytes = 0;
        let skipped = std::mem::take(&mut self.skipped);
        (std::mem::take(&mut self.kept), skipped)
    }
}

/// A request one watched page made.
#[derive(Debug)]
pub(crate) struct SeenRequest {
    /// The index of the capture whose page made it.
    pub capture: usize,
    pub id: String,
    /// The URL of the document that made it, which must still be on the
    /// capture's hosts for the request to count.
    pub document_url: String,
    pub url: String,
    pub method: String,
    pub kind: String,
    pub headers: Map<String, Value>,
}

/// The response to a request one watched page made.
#[derive(Debug)]
pub(crate) struct SeenResponse {
    pub capture: usize,
    pub id: String,
    pub status: Option<u16>,
    pub mime: Option<String>,
}

/// A load one watched page finished, or that failed.
#[derive(Debug)]
pub(crate) struct SeenLoad {
    pub capture: usize,
    pub id: String,
    /// The watch's session with the page, to read the body in.
    pub session: String,
    /// The bytes the load took on the wire, as the browser reports them.
    pub encoded: Option<u64>,
}

/// One message read from the watch.
#[derive(Debug)]
pub(crate) enum Next {
    /// Nothing arrived within one read slice.
    Quiet,
    /// An event the wait does not act on.
    Ignored,
    Request(SeenRequest),
    Response(SeenResponse),
    Finished(SeenLoad),
    Failed(SeenLoad),
}

/// Session id → (target id, the index of the capture it is watched for).
type Sessions = HashMap<String, (String, usize)>;

/// Turn one event from the watch connection into what the wait acts on.
/// Only events from a session the watch attached count, so a page the
/// watch is not attached to can never feed a capture.
pub(crate) fn decode(event: &Value, sessions: &mut Sessions) -> Next {
    let method = event.get("method").and_then(Value::as_str).unwrap_or("");
    let params = event.get("params").unwrap_or(&Value::Null);
    if method == "Target.detachedFromTarget" {
        if let Some(session) = params.get("sessionId").and_then(Value::as_str) {
            sessions.remove(session);
        }
        return Next::Ignored;
    }
    let session = event.get("sessionId").and_then(Value::as_str);
    let Some(&(_, capture)) = session.and_then(|s| sessions.get(s)) else {
        return Next::Ignored;
    };
    let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    match method {
        "Network.requestWillBeSent" => {
            let request = params.get("request");
            let (Some(id), Some(document_url), Some(url), Some(method)) = (
                text(params.get("requestId")),
                text(params.get("documentURL")),
                text(request.and_then(|r| r.get("url"))),
                text(request.and_then(|r| r.get("method"))),
            ) else {
                return Next::Ignored;
            };
            Next::Request(SeenRequest {
                capture,
                id,
                document_url,
                url,
                method,
                kind: text(params.get("type")).unwrap_or_default(),
                headers: request
                    .and_then(|r| r.get("headers"))
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
            })
        }
        "Network.responseReceived" => {
            let Some(id) = text(params.get("requestId")) else {
                return Next::Ignored;
            };
            let response = params.get("response");
            Next::Response(SeenResponse {
                capture,
                id,
                status: response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_f64)
                    .filter(|s| (100.0..1000.0).contains(s))
                    .map(|s| s as u16),
                mime: text(response.and_then(|r| r.get("mimeType"))),
            })
        }
        "Network.loadingFinished" | "Network.loadingFailed" => {
            let (Some(id), Some(session)) = (text(params.get("requestId")), session) else {
                return Next::Ignored;
            };
            let load = SeenLoad {
                capture,
                id,
                session: session.to_string(),
                encoded: params
                    .get("encodedDataLength")
                    .and_then(Value::as_f64)
                    .filter(|n| *n >= 0.0)
                    .map(|n| n as u64),
            };
            if method == "Network.loadingFinished" {
                Next::Finished(load)
            } else {
                Next::Failed(load)
            }
        }
        _ => Next::Ignored,
    }
}

/// A `Network.getResponseBody` result as bytes: base64 decoded when the
/// browser sent it so, else the text's own bytes. The text is taken out of
/// the result and zeroized once decoded.
pub(crate) fn body_bytes(mut result: Value) -> Option<Vec<u8>> {
    let base64 = result
        .get("base64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Value::String(text) = result.get_mut("body")?.take() else {
        return None;
    };
    let text = Zeroizing::new(text);
    if base64 {
        base64_decode(&text)
    } else {
        Some(text.as_bytes().to_vec())
    }
}

/// Standard base64 with padding, as DevTools sends a binary body; `None`
/// for anything else.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.len() / 4;
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i + 1 == chunks;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = (n << 6) | value(c)?;
        }
        n <<= 6 * pad as u32;
        let three = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&three[..3 - pad]);
    }
    Some(out)
}

/// The watch connection: the pages it is attached to, and the events they
/// send.
pub(crate) struct Watch<T: Transport> {
    cdp: Cdp<T>,
    sessions: Sessions,
}

impl<T: Transport> Watch<T> {
    /// A watch over `cdp`, which must keep the events its calls read past
    /// ([`Cdp::keeping_events`]).
    pub fn new(cdp: Cdp<T>) -> Self {
        Watch {
            cdp,
            sessions: HashMap::new(),
        }
    }

    /// Watch exactly the pages in `wanted` (each with the index of the
    /// capture it is for): detach from every watched page no longer in it,
    /// then attach to each new one and enable `Network` there. A page that
    /// refuses (it closed, it is navigating) is skipped until the next
    /// poll; a broken connection, a timeout or a cancel is returned, and the
    /// caller drops the watch (or ends the wait).
    pub fn sync(&mut self, wanted: &[(usize, &PageTarget)]) -> Result<(), CdpError> {
        let stale: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, (target, capture))| {
                !wanted
                    .iter()
                    .any(|(c, page)| c == capture && &page.id == target)
            })
            .map(|(session, _)| session.clone())
            .collect();
        for session in stale {
            self.sessions.remove(&session);
            self.detach(&session)?;
        }
        for (capture, page) in wanted {
            let watched = self
                .sessions
                .values()
                .any(|(target, c)| c == capture && target == &page.id);
            if watched {
                continue;
            }
            let attached = match self.cdp.call(
                "Target.attachToTarget",
                json!({ "targetId": page.id, "flatten": true }),
                None,
            ) {
                Ok(attached) => attached,
                Err(CdpError::Protocol { .. }) => continue,
                Err(e) => return Err(e),
            };
            let Some(session) = attached.get("sessionId").and_then(Value::as_str) else {
                continue;
            };
            // No request body is ever reported (`maxPostDataSize: 0`): the
            // log needs none, and a body can carry a password.
            match self.cdp.call(
                "Network.enable",
                json!({ "maxPostDataSize": 0 }),
                Some(session),
            ) {
                Ok(_) => {
                    self.sessions
                        .insert(session.to_string(), (page.id.clone(), *capture));
                }
                Err(CdpError::Protocol { .. }) => self.detach(session)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn detach(&mut self, session: &str) -> Result<(), CdpError> {
        match self.cdp.call(
            "Target.detachFromTarget",
            json!({ "sessionId": session }),
            None,
        ) {
            // Already gone: the page closed, which detached it.
            Ok(_) | Err(CdpError::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The body of the finished load `id`, from the browser's own buffer
    /// (`Network.getResponseBody` in the page's session, which sends
    /// nothing to the site). `None` when the browser no longer holds it,
    /// the page has gone, or the browser did not answer within the call's
    /// bound: that one load is skipped and the watch stays up. A late
    /// answer is read past as a stale reply.
    pub fn response_body(&mut self, session: &str, id: &str) -> Result<Option<Vec<u8>>, CdpError> {
        match self.cdp.call(
            "Network.getResponseBody",
            json!({ "requestId": id }),
            Some(session),
        ) {
            Ok(result) => Ok(body_bytes(result)),
            Err(CdpError::Protocol { .. } | CdpError::Timeout(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The next message the watched pages sent.
    pub fn next(&mut self) -> Result<Next, CdpError> {
        Ok(match self.cdp.next_event()? {
            None => Next::Quiet,
            Some(event) => decode(&event, &mut self.sessions),
        })
    }

    #[cfg(test)]
    pub fn sessions(&self) -> Vec<(String, String, usize)> {
        let mut out: Vec<_> = self
            .sessions
            .iter()
            .map(|(s, (t, c))| (s.clone(), t.clone(), *c))
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    fn url(u: &str) -> UrlParts {
        parse_url(u).unwrap()
    }

    #[test]
    fn request_rules_take_an_https_url_without_a_query() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "https://app.example.com/bff/graphql",
                Some("app.example.com/bff/graphql"),
            ),
            ("https://App.Example.com", Some("app.example.com/")),
            (
                "https://app.example.com:443/api/",
                Some("app.example.com/api/"),
            ),
            ("http://app.example.com/api", None),
            ("https://app.example.com:8443/api", None),
            ("https://app.example.com/api?x=1", None),
            ("https://app.example.com/api#frag", None),
            ("https://user@app.example.com/api", None),
            ("app.example.com/api", None),
            ("", None),
        ];
        for (input, want) in cases {
            let got = RequestRule::parse(input).map(|r| r.to_string());
            assert_eq!(got.as_deref(), *want, "{input}");
        }
    }

    #[test]
    fn a_request_matches_on_its_host_and_path_never_a_substring() {
        let rule = RequestRule::parse("https://app.example.com/bff/graphql").unwrap();
        let cases: &[(&str, bool)] = &[
            ("https://app.example.com/bff/graphql", true),
            ("https://app.example.com/bff/graphql?op=Accounts", true),
            ("https://app.example.com/bff/graphql/batch", true),
            ("https://APP.example.com/bff/graphql", true),
            ("https://app.example.com:443/bff/graphql", true),
            // A path only matches on a boundary.
            ("https://app.example.com/bff/graphqlx", false),
            ("https://app.example.com/bff", false),
            ("https://app.example.com/other/bff/graphql", false),
            // Another host, scheme or port is another origin.
            ("https://www.example.com/bff/graphql", false),
            ("https://example.com/bff/graphql", false),
            ("https://app.example.com.evil.example/bff/graphql", false),
            ("https://evil.example/bff/graphql?h=app.example.com", false),
            ("http://app.example.com/bff/graphql", false),
            ("https://app.example.com:8443/bff/graphql", false),
            ("wss://app.example.com/bff/graphql", false),
        ];
        for (u, want) in cases {
            assert_eq!(rule.matches(&url(u)), *want, "{u}");
        }
    }

    #[test]
    fn a_captured_request_keeps_no_query_and_hides_its_header_values() {
        let headers = json!({
            "X-CSRF-Token": "dummy-csrf",
            "content-type": "application/json",
            "Accept": "*/*",
            "weird": 7
        });
        let r = CapturedRequest::new(
            "post",
            &url("https://app.example.com/bff/graphql?token=dummy-query#f"),
            headers.as_object().unwrap(),
        );
        assert_eq!(r.method(), "POST");
        assert_eq!(r.url(), "https://app.example.com/bff/graphql");
        let names: Vec<&str> = r.headers().map(|(n, _)| n).collect();
        assert_eq!(names, ["accept", "content-type", "x-csrf-token"]);
        assert_eq!(r.header("x-csrf-token").unwrap().expose(), "dummy-csrf");
        assert_eq!(r.header("X-Csrf-Token").unwrap().expose(), "dummy-csrf");
        assert!(r.header("cookie").is_none());
        let debug = format!("{r:?}");
        assert!(!debug.contains("dummy-csrf"), "{debug}");
        assert!(!debug.contains("dummy-query"), "{debug}");
    }

    #[test]
    fn a_log_keeps_page_loads_and_data_calls_on_its_domain_once_each() {
        let mut log = RequestLog::new(HostRule::domain("example.com").unwrap());
        log.observe(
            "1",
            "get",
            &url("https://app.example.com/api/x/?page=1"),
            "XHR",
        );
        log.observe(
            "2",
            "GET",
            &url("https://app.example.com/api/x/?page=2"),
            "XHR",
        );
        log.observe("3", "POST", &url("https://app.example.com/api/x/"), "Fetch");
        log.observe("4", "GET", &url("https://www.example.com/home"), "Document");
        // Not logged: other kinds, other domains, look-alikes, plain http.
        log.observe("5", "GET", &url("https://app.example.com/app.js"), "Script");
        log.observe("6", "GET", &url("https://cdn.example.org/x"), "XHR");
        log.observe(
            "7",
            "GET",
            &url("https://example.com.evil.example/x"),
            "XHR",
        );
        log.observe("8", "GET", &url("http://app.example.com/api/y"), "XHR");
        log.observe(
            "9",
            "GET",
            &url("https://app.example.com:8443/api/y"),
            "XHR",
        );
        log.respond("2", Some(200), Some("application/json".into()));
        log.respond("1", Some(500), Some("text/html".into()));
        log.respond("4", Some(200), None);
        log.respond("unknown", Some(404), None);
        let got: Vec<String> = log
            .entries()
            .iter()
            .map(|e| {
                format!(
                    "{} {}{} {} {:?} {:?}",
                    e.method(),
                    e.host(),
                    e.path(),
                    e.kind(),
                    e.status(),
                    e.mime()
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                "GET app.example.com/api/x/ XHR Some(200) Some(\"application/json\")",
                "POST app.example.com/api/x/ Fetch None None",
                "GET www.example.com/home Document Some(200) None",
            ]
        );
    }

    /// A built request and log entry are the ones the watch makes of the
    /// same request.
    #[cfg(feature = "test-util")]
    #[test]
    fn built_requests_are_the_ones_the_watch_makes() {
        let seen = CapturedRequest::new(
            "post",
            &url("https://App.Example.com/bff/graphql?op=dummy-query"),
            json!({"X-CSRF-Token": "dummy-csrf", "Accept": "*/*"})
                .as_object()
                .unwrap(),
        );
        let built = CapturedRequest::for_test(
            "post",
            "https://App.Example.com/bff/graphql?op=dummy-query",
            &[("X-CSRF-Token", "dummy-csrf"), ("Accept", "*/*")],
        );
        let parts = |r: &CapturedRequest| {
            let headers: Vec<(String, String)> = r
                .headers()
                .map(|(n, v)| (n.to_string(), v.expose().to_string()))
                .collect();
            (r.method().to_string(), r.url().to_string(), headers)
        };
        assert_eq!(parts(&seen), parts(&built));

        let mut log = RequestLog::new(HostRule::domain("example.com").unwrap());
        log.observe(
            "1",
            "get",
            &url("https://API.example.com/v1/x?page=2#top"),
            "Fetch",
        );
        log.respond("1", Some(200), Some("application/json".into()));
        log.observe("2", "post", &url("https://example.com/v1/y"), "XHR");
        assert_eq!(
            log.entries(),
            [
                LoggedRequest::for_test(
                    "get",
                    "API.example.com",
                    "/v1/x?page=2#top",
                    RequestKind::Fetch,
                    Some(200),
                    Some("application/json"),
                ),
                LoggedRequest::for_test(
                    "post",
                    "example.com",
                    "/v1/y",
                    RequestKind::Xhr,
                    None,
                    None
                ),
            ]
        );
    }

    #[test]
    fn a_log_is_bounded() {
        let mut log = RequestLog::new(HostRule::domain("example.com").unwrap());
        for i in 0..(MAX_LOGGED + 10) {
            log.observe(
                &i.to_string(),
                "GET",
                &url(&format!("https://example.com/item/{i}")),
                "XHR",
            );
        }
        assert_eq!(log.entries().len(), MAX_LOGGED);
        assert!(log.pending.len() <= MAX_PENDING);
    }

    fn sessions() -> Sessions {
        HashMap::from([("S1".to_string(), ("T1".to_string(), 2))])
    }

    #[test]
    fn events_count_only_from_a_watched_session() {
        let request = |session: Option<&str>| {
            let mut e = json!({"method": "Network.requestWillBeSent", "params": {
                "requestId": "R1", "documentURL": "https://app.example.com/home", "type": "XHR",
                "request": {"url": "https://app.example.com/api", "method": "GET",
                            "headers": {"x-a": "dummy"}}}});
            if let Some(s) = session {
                e["sessionId"] = json!(s);
            }
            e
        };
        let mut s = sessions();
        match decode(&request(Some("S1")), &mut s) {
            Next::Request(r) => {
                assert_eq!(r.capture, 2);
                assert_eq!(r.id, "R1");
                assert_eq!(r.document_url, "https://app.example.com/home");
                assert_eq!(r.url, "https://app.example.com/api");
                assert_eq!(r.method, "GET");
                assert_eq!(r.kind, "XHR");
                assert_eq!(r.headers["x-a"], "dummy");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            decode(&request(Some("S9")), &mut s),
            Next::Ignored
        ));
        assert!(matches!(decode(&request(None), &mut s), Next::Ignored));

        let response = json!({"method": "Network.responseReceived", "sessionId": "S1",
            "params": {"requestId": "R1", "response": {"status": 204, "mimeType": "text/plain"}}});
        match decode(&response, &mut s) {
            Next::Response(r) => {
                assert_eq!((r.capture, r.id.as_str()), (2, "R1"));
                assert_eq!(r.status, Some(204));
                assert_eq!(r.mime.as_deref(), Some("text/plain"));
            }
            other => panic!("{other:?}"),
        }
        // Malformed shapes and other events are ignored.
        for e in [
            json!({"method": "Network.requestWillBeSent", "sessionId": "S1", "params": {}}),
            json!({"method": "Network.responseReceived", "sessionId": "S1", "params": {}}),
            json!({"method": "Network.loadingFinished", "sessionId": "S1", "params": {}}),
            json!({"method": "Target.targetInfoChanged", "params": {}}),
            json!({}),
        ] {
            assert!(matches!(decode(&e, &mut s), Next::Ignored), "{e}");
        }
        // A detach forgets the session, so its later events are ignored.
        let detached =
            json!({"method": "Target.detachedFromTarget", "params": {"sessionId": "S1"}});
        assert!(matches!(decode(&detached, &mut s), Next::Ignored));
        assert!(s.is_empty());
        assert!(matches!(decode(&response, &mut s), Next::Ignored));
    }

    /// Answers attach, enable and detach; records what was sent.
    struct Script {
        sent: Vec<Value>,
        inbox: VecDeque<String>,
        refuse_attach: Vec<&'static str>,
        refuse_enable: bool,
        next_session: usize,
    }

    impl Transport for Script {
        fn send(&mut self, text: &str) -> Result<(), String> {
            let req: Value = serde_json::from_str(text).unwrap();
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap().to_string();
            self.sent.push(req.clone());
            // An event ahead of every reply, which the watch must keep.
            self.inbox.push_back(
                json!({"method": "Network.requestWillBeSent", "sessionId": "S-early",
                       "params": {}})
                .to_string(),
            );
            let reply = match method.as_str() {
                "Target.attachToTarget" => {
                    let target = req["params"]["targetId"].as_str().unwrap();
                    if self.refuse_attach.contains(&target) {
                        json!({"id": id, "error": {"code": -32602, "message": "No target"}})
                    } else {
                        self.next_session += 1;
                        json!({"id": id, "result": {"sessionId": format!("S{}", self.next_session)}})
                    }
                }
                "Network.enable" if self.refuse_enable => {
                    json!({"id": id, "error": {"code": -32000, "message": "Not allowed"}})
                }
                _ => json!({"id": id, "result": {}}),
            };
            self.inbox.push_back(reply.to_string());
            Ok(())
        }

        fn recv(&mut self) -> Result<Option<String>, String> {
            Ok(self.inbox.pop_front())
        }
    }

    fn never() -> bool {
        false
    }

    fn watch(script: Script) -> Watch<Script> {
        Watch::new(Cdp::new(script, Duration::from_secs(1), never).keeping_events())
    }

    fn script() -> Script {
        Script {
            sent: Vec::new(),
            inbox: VecDeque::new(),
            refuse_attach: Vec::new(),
            refuse_enable: false,
            next_session: 0,
        }
    }

    fn page(id: &str) -> PageTarget {
        PageTarget {
            id: id.into(),
            url: format!("https://app.example.com/{id}"),
        }
    }

    fn methods(w: &Watch<Script>) -> Vec<String> {
        w.cdp
            .transport()
            .sent
            .iter()
            .map(|r| {
                let target = r["params"]["targetId"]
                    .as_str()
                    .or(r["sessionId"].as_str())
                    .or(r["params"]["sessionId"].as_str())
                    .unwrap_or("");
                format!("{} {target}", r["method"].as_str().unwrap())
            })
            .collect()
    }

    #[test]
    fn the_watch_attaches_enables_and_detaches_to_follow_the_wanted_pages() {
        let mut w = watch(script());
        let (a, b) = (page("A"), page("B"));
        w.sync(&[(0, &a), (1, &b)]).unwrap();
        assert_eq!(
            methods(&w),
            [
                "Target.attachToTarget A",
                "Network.enable S1",
                "Target.attachToTarget B",
                "Network.enable S2",
            ]
        );
        assert_eq!(
            w.cdp.transport().sent[1]["params"],
            json!({"maxPostDataSize": 0})
        );
        assert_eq!(
            w.sessions(),
            [
                ("S1".to_string(), "A".to_string(), 0),
                ("S2".to_string(), "B".to_string(), 1)
            ]
        );
        // Unchanged: nothing sent. A gone: detached.
        w.sync(&[(0, &a), (1, &b)]).unwrap();
        assert_eq!(methods(&w).len(), 4);
        w.sync(&[(1, &b)]).unwrap();
        assert_eq!(methods(&w)[4], "Target.detachFromTarget S1");
        assert_eq!(w.sessions().len(), 1);
        // The events read past while calling were kept, in order.
        let mut kept = 0;
        while let Next::Ignored = w.next().unwrap() {
            kept += 1;
        }
        assert_eq!(kept, 5, "one per call");
        assert!(matches!(w.next().unwrap(), Next::Quiet));
    }

    #[test]
    fn a_page_that_refuses_is_skipped_until_the_next_sync() {
        let mut s = script();
        s.refuse_attach = vec!["A"];
        let mut w = watch(s);
        let (a, b) = (page("A"), page("B"));
        w.sync(&[(0, &a), (0, &b)]).unwrap();
        assert_eq!(w.sessions(), [("S1".to_string(), "B".to_string(), 0)]);

        let mut s = script();
        s.refuse_enable = true;
        let mut w = watch(s);
        w.sync(&[(0, &a)]).unwrap();
        assert!(w.sessions().is_empty());
        assert_eq!(
            methods(&w),
            [
                "Target.attachToTarget A",
                "Network.enable S1",
                "Target.detachFromTarget S1"
            ]
        );
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    fn url(u: &str) -> UrlParts {
        parse_url(u).unwrap()
    }

    fn log() -> ResponseLog {
        ResponseLog::new(
            HostRule::domain("example.com").unwrap(),
            vec!["/api/billing".into()],
        )
    }

    #[test]
    fn base64_bodies_decode_and_anything_else_is_refused() {
        let cases: &[(&str, Option<&[u8]>)] = &[
            ("", Some(b"")),
            ("Zg==", Some(b"f")),
            ("Zm8=", Some(b"fo")),
            ("Zm9v", Some(b"foo")),
            ("JVBERi0xLjQK", Some(b"%PDF-1.4\n")),
            ("+/+/", Some(&[0xfb, 0xff, 0xbf])),
            ("Zg=", None),
            ("Zg===", None),
            ("Z===", None),
            ("Zg==Zg==", None),
            ("Zm9v!", None),
            ("Zm 9v", None),
        ];
        for (text, want) in cases {
            assert_eq!(base64_decode(text).as_deref(), *want, "{text}");
        }
        assert_eq!(
            body_bytes(json!({"body": "Zm9v", "base64Encoded": true})).as_deref(),
            Some(&b"foo"[..])
        );
        assert_eq!(
            body_bytes(json!({"body": "Zm9v", "base64Encoded": false})).as_deref(),
            Some(&b"Zm9v"[..])
        );
        assert_eq!(body_bytes(json!({"base64Encoded": true})), None);
    }

    #[test]
    fn a_finished_load_is_read_from_its_own_session() {
        let mut sessions: Sessions = HashMap::new();
        sessions.insert("S1".into(), ("T-app".into(), 0));
        let finished = json!({"method": "Network.loadingFinished", "sessionId": "S1",
                              "params": {"requestId": "R1"}});
        match decode(&finished, &mut sessions) {
            Next::Finished(load) => {
                assert_eq!(
                    (load.capture, load.id.as_str(), load.session.as_str()),
                    (0, "R1", "S1")
                );
            }
            other => panic!("{other:?}"),
        }
        let failed = json!({"method": "Network.loadingFailed", "sessionId": "S1",
                            "params": {"requestId": "R2"}});
        assert!(matches!(decode(&failed, &mut sessions), Next::Failed(_)));
        // A session the watch did not attach never feeds a capture.
        let stranger = json!({"method": "Network.loadingFinished", "sessionId": "S9",
                              "params": {"requestId": "R1"}});
        assert!(matches!(decode(&stranger, &mut sessions), Next::Ignored));
    }

    #[test]
    fn only_page_loads_and_data_calls_on_the_paths_are_kept() {
        let mut log = log();
        let cases: &[(&str, &str, &str, bool)] = &[
            ("R1", "https://portal.example.com/api/billing", "XHR", true),
            (
                "R2",
                "https://portal.example.com/api/billing/statements?x=1",
                "Fetch",
                true,
            ),
            (
                "R3",
                "https://portal.example.com/api/billing/s/pdf",
                "Document",
                true,
            ),
            (
                "R4",
                "https://portal.example.com/api/billingx",
                "XHR",
                false,
            ),
            (
                "R5",
                "https://portal.example.com/api/billing/app.js",
                "Script",
                false,
            ),
            ("R6", "https://portal.example.org/api/billing", "XHR", false),
            ("R7", "http://portal.example.com/api/billing", "XHR", false),
            (
                "R8",
                "https://portal.example.com:8443/api/billing",
                "XHR",
                false,
            ),
        ];
        for (id, u, kind, want) in cases {
            log.observe(id, "get", &url(u), kind);
            assert_eq!(log.is_pending(id), *want, "{u} {kind}");
        }
        log.respond("R2", Some(200), Some("application/json".into()));
        log.keep("R2", Some(b"{}".to_vec()));
        let (kept, skipped) = log.take();
        assert_eq!(skipped, 2, "R1 and R3 were still loading at the read");
        assert_eq!(kept.len(), 1);
        let r = &kept[0];
        assert_eq!(
            (
                r.method(),
                r.host(),
                r.path(),
                r.kind(),
                r.status(),
                r.mime(),
                r.body()
            ),
            (
                "GET",
                "portal.example.com",
                "/api/billing/statements",
                RequestKind::Fetch,
                Some(200),
                Some("application/json"),
                &b"{}"[..]
            )
        );
        assert!(
            !format!("{r:?}").contains("{}"),
            "Debug shows the length only"
        );
    }

    #[test]
    fn a_redirect_off_the_paths_drops_the_request_and_failures_are_forgotten() {
        let mut log = log();
        log.observe(
            "R1",
            "GET",
            &url("https://portal.example.com/api/billing/a"),
            "XHR",
        );
        log.observe("R1", "GET", &url("https://portal.example.com/login"), "XHR");
        assert!(!log.is_pending("R1"));
        log.observe(
            "R2",
            "GET",
            &url("https://portal.example.com/api/billing/b"),
            "XHR",
        );
        log.forget("R2");
        log.keep("R2", Some(b"x".to_vec()));
        log.keep("R3", Some(b"x".to_vec()));
        let (kept, skipped) = log.take();
        assert!(kept.is_empty());
        assert_eq!(
            skipped, 0,
            "a load never noted, or forgotten, is not a skip"
        );
    }

    #[test]
    fn a_body_too_large_or_gone_is_skipped() {
        let mut log = log();
        let on_path = url("https://portal.example.com/api/billing/s");
        log.observe("R1", "GET", &on_path, "XHR");
        log.keep("R1", Some(vec![0; MAX_BODY + 1]));
        log.observe("R2", "GET", &on_path, "XHR");
        log.keep("R2", None);
        log.observe("R3", "GET", &on_path, "XHR");
        log.keep("R3", Some(vec![1; MAX_BODY]));
        let (kept, skipped) = log.take();
        assert_eq!((kept.len(), skipped), (1, 2));
        assert_eq!(kept[0].body().len(), MAX_BODY);
    }

    #[test]
    fn the_count_of_kept_responses_is_bounded() {
        let mut log = log();
        let on_path = url("https://portal.example.com/api/billing/s");
        for i in 0..MAX_RESPONSES + 2 {
            let id = format!("R{i}");
            log.observe(&id, "GET", &on_path, "XHR");
            log.keep(&id, Some(b"{}".to_vec()));
        }
        assert!(!log.has_room());
        let (kept, skipped) = log.take();
        assert_eq!((kept.len(), skipped), (MAX_RESPONSES, 2));
    }
}

#[cfg(test)]
mod response_limit_tests {
    use super::*;

    fn small() -> ResponseLog {
        ResponseLog::with_limits(
            HostRule::domain("example.com").unwrap(),
            vec!["/".into()],
            Limits {
                responses: 10,
                body: 4,
                total: 10,
            },
        )
    }

    fn note(log: &mut ResponseLog, id: &str) {
        let u = parse_url("https://portal.example.com/api/x").unwrap();
        log.observe(id, "GET", &u, "XHR");
    }

    #[test]
    fn the_total_bound_skips_what_would_pass_it() {
        let mut log = small();
        for (id, body) in [("R1", "aaaa"), ("R2", "bbbb"), ("R3", "ccc"), ("R4", "dd")] {
            note(&mut log, id);
            log.keep(id, Some(body.as_bytes().to_vec()));
        }
        // 4 + 4 kept; 3 more would pass 10; 2 more fits exactly.
        let (kept, skipped) = log.take();
        let bodies: Vec<&[u8]> = kept.iter().map(CapturedResponse::body).collect();
        assert_eq!(bodies, [&b"aaaa"[..], b"bbbb", b"dd"]);
        assert_eq!(skipped, 1);
        let mut log = small();
        for id in ["R1", "R2"] {
            note(&mut log, id);
            log.keep(id, Some(b"aaaa".to_vec()));
        }
        note(&mut log, "R3");
        log.keep("R3", Some(b"aa".to_vec()));
        assert!(!log.has_room(), "the total is reached");
        note(&mut log, "R4");
        assert!(!log.worth_reading("R4", Some(1)));
    }

    #[test]
    fn a_load_larger_on_the_wire_than_a_body_is_not_read() {
        let mut log = small();
        note(&mut log, "R1");
        assert!(log.worth_reading("R1", Some(4)));
        assert!(log.worth_reading("R1", None));
        assert!(!log.worth_reading("R1", Some(5)));
        assert!(!log.worth_reading("R9", Some(1)), "not a load it waits on");
    }

    #[test]
    fn loads_still_in_progress_count_as_skipped() {
        let mut log = small();
        note(&mut log, "R1");
        note(&mut log, "R2");
        assert_eq!(log.abandon_pending(), 2);
        assert!(!log.is_pending("R1"));
        note(&mut log, "R3");
        let (kept, skipped) = log.take();
        assert!(kept.is_empty());
        assert_eq!(skipped, 3, "two abandoned, one still loading at the read");
    }

    #[test]
    fn loads_that_never_finish_count_as_skipped_when_they_pile_up() {
        let mut log = small();
        let u = parse_url("https://portal.example.com/api/x").unwrap();
        for i in 0..=MAX_PENDING {
            log.observe(&format!("R{i}"), "GET", &u, "XHR");
        }
        assert!(log.is_pending(&format!("R{MAX_PENDING}")));
        assert!(!log.is_pending("R0"));
        let (_, skipped) = log.take();
        assert_eq!(
            skipped,
            MAX_PENDING + 1,
            "the ones dropped, and the one still loading"
        );
    }

    #[test]
    fn a_finished_load_carries_its_size_on_the_wire() {
        let mut sessions: Sessions = HashMap::new();
        sessions.insert("S1".into(), ("T-app".into(), 0));
        let event = json!({"method": "Network.loadingFinished", "sessionId": "S1",
                           "params": {"requestId": "R1", "encodedDataLength": 1234.0}});
        match decode(&event, &mut sessions) {
            Next::Finished(load) => assert_eq!(load.encoded, Some(1234)),
            other => panic!("{other:?}"),
        }
    }
}
