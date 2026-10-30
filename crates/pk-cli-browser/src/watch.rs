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
//! header, body or query at all.
//!
//! [`Capture::request`]: crate::Capture::request
//! [`Capture::optional_request`]: crate::Capture::optional_request
//! [`Capture::request_log`]: crate::Capture::request_log

use std::collections::HashMap;
use std::fmt;

use pk_cli_secrets::Secret;
use serde_json::{json, Map, Value};

use crate::capture::PageTarget;
use crate::cdp::{Cdp, CdpError, Transport};
use crate::host::{parse_url, HostRule, UrlParts};
use crate::jar::path_matches;

/// Distinct endpoints one request log keeps; later ones are dropped.
const MAX_LOGGED: usize = 500;
/// Requests a log remembers while it waits for their responses.
const MAX_PENDING: usize = 2_000;

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

/// One message read from the watch.
#[derive(Debug)]
pub(crate) enum Next {
    /// Nothing arrived within one read slice.
    Quiet,
    /// An event the wait does not act on.
    Ignored,
    Request(SeenRequest),
    Response(SeenResponse),
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
    let Some(&(_, capture)) = event
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(|s| sessions.get(s))
    else {
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
        _ => Next::Ignored,
    }
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
