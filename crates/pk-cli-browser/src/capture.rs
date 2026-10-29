//! What to read from the signed-in pages, and the wait for it.
//!
//! Each poll opens one short-lived connection to the browser target and:
//!
//! 1. lists the page targets (`Target.getTargets`, a browser-level call no
//!    page can observe);
//! 2. for each capture still waiting, picks the pages whose URL is `https`
//!    on one of its hosts. Pages on any other host (the provider's login
//!    host, where the bot checks run, included) are never touched;
//! 3. evaluates the capture's expression in those pages through a flattened
//!    session (`Target.attachToTarget`, `Runtime.evaluate`,
//!    `Target.detachFromTarget`; never `Runtime.enable` or `Network.enable`),
//!    and requires the answer to come from a document whose own
//!    `location.hostname` is allowed, not just a target whose URL was;
//! 4. reads the capture's cookies from the browser-wide jar
//!    (`Storage.getCookies`, which includes `HttpOnly` cookies), choosing per
//!    name the cookie the browser would send to the given URL;
//! 5. once everything required is there, closes every page on the capture's
//!    hosts (`Target.closeTarget`) before anything else happens, so the web
//!    app cannot renew, and so rotate, the token it just handed over.

use std::time::{Duration, Instant};

use pk_cli_core::CliError;
use pk_cli_secrets::Secret;
use serde_json::{json, Value};

use crate::cdp::{Cdp, CdpError, Transport, WsTransport};
use crate::chrome::{Browser, Launch};
use crate::host::{parse_url, HostRule, UrlParts};
use crate::interrupt;

/// How long a sign-in may take before the wait gives up: the family's
/// ten minutes, as `sfi`, `wfr` and `carta` have used.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// Between polls of the page list.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Bound on one CDP call, the in-page check included.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// A timeout this long is announced as no limit at all.
const NO_LIMIT: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// Consecutive polls finding no page at all, after pages were seen, that
/// mean the owner closed the window (on macOS Chrome keeps running with no
/// window, so the process exiting is not the only signal).
const CLOSED_POLLS: u32 = 2;

/// One session to read out of the sign-in window: the host(s) it may be read
/// from and what to read there.
///
/// It is ready when a page on one of its hosts is open, its
/// [`expression`](Capture::expression) (if any) answers with a non-empty
/// string in such a page, and every [`cookie`](Capture::cookie) is set.
/// Choose a signal that only exists once the owner is signed in (an access
/// token in the stored session, a cookie set only after login); the tab is
/// closed the moment it is read, so a premature read cannot be retried.
#[derive(Debug, Clone)]
pub struct Capture {
    label: String,
    hosts: Vec<HostRule>,
    expression: Option<String>,
    cookies: Vec<CookieRead>,
    hint: Option<String>,
}

#[derive(Debug, Clone)]
struct CookieRead {
    url: String,
    name: String,
    required: bool,
}

impl Capture {
    /// A capture named `label` (the provider's name, used in the status
    /// lines and errors) read from pages on `host`.
    pub fn new(label: impl Into<String>, host: HostRule) -> Self {
        Capture {
            label: label.into(),
            hosts: vec![host],
            expression: None,
            cookies: Vec::new(),
            hint: None,
        }
    }

    /// Another host the capture may be read from.
    pub fn host(mut self, host: HostRule) -> Self {
        self.hosts.push(host);
        self
    }

    /// A JavaScript expression evaluated in each page on the capture's
    /// hosts that answers with the session as a non-empty string once the
    /// owner is signed in, and with `null`, `undefined` or `""` before. It
    /// may return a promise. Write an expression, not statements (an arrow
    /// function called in place is the usual shape); it runs in the page's
    /// main world, so it can read `localStorage`, `sessionStorage` and
    /// non-`HttpOnly` cookies.
    pub fn expression(mut self, js: impl Into<String>) -> Self {
        self.expression = Some(js.into());
        self
    }

    /// A cookie the capture needs: the value the browser would send with a
    /// request to `url` (an `https` URL on one of the capture's hosts) for
    /// the cookie `name`. Read from the browser, so `HttpOnly` cookies work.
    /// The capture waits until it is set.
    pub fn cookie(mut self, url: impl Into<String>, name: impl Into<String>) -> Self {
        self.cookies.push(CookieRead {
            url: url.into(),
            name: name.into(),
            required: true,
        });
        self
    }

    /// As [`Capture::cookie`], but the capture does not wait for it: it is
    /// read when the rest is ready and included if set.
    pub fn optional_cookie(mut self, url: impl Into<String>, name: impl Into<String>) -> Self {
        self.cookies.push(CookieRead {
            url: url.into(),
            name: name.into(),
            required: false,
        });
        self
    }

    /// What signing in involves, for the status line
    /// (`"password and the texted code"`).
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Check the capture is well-formed: a label, something to read, a
    /// non-empty expression, and each cookie an `https` URL on one of the
    /// capture's hosts with a valid, distinct name. The waits call this
    /// before reading anything; a CLI can call it in a unit test.
    pub fn validate(&self) -> Result<(), CliError> {
        let bad = |why: String| Err(CliError::Other(format!("invalid capture: {why}")));
        if self.label.trim().is_empty() {
            return bad("the label is empty".into());
        }
        let label = &self.label;
        if self.expression.is_none() && self.cookies.is_empty() {
            return bad(format!("`{label}` names nothing to read"));
        }
        if self
            .expression
            .as_ref()
            .is_some_and(|e| e.trim().is_empty())
        {
            return bad(format!("`{label}` has an empty expression"));
        }
        for (i, cookie) in self.cookies.iter().enumerate() {
            let name = &cookie.name;
            if !cookie_name_ok(name) {
                return bad(format!("`{label}` has an invalid cookie name `{name}`"));
            }
            if self.cookies[..i].iter().any(|c| &c.name == name) {
                return bad(format!("`{label}` reads the cookie `{name}` twice"));
            }
            let on_host = parse_url(&cookie.url).is_some_and(|u| {
                u.is_https_default_port() && self.hosts.iter().any(|r| r.matches_host(&u.host))
            });
            if !on_host {
                return bad(format!(
                    "`{label}` reads `{name}` for {}, which is not an https URL on its hosts",
                    cookie.url
                ));
            }
        }
        Ok(())
    }
}

/// An RFC 6265 cookie-name token: printable ASCII without separators.
fn cookie_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_graphic()
                && !matches!(
                    b,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

/// Several captures in one window must be distinguishable and must not
/// share a host, or one capture would read and close the other's tabs.
fn validate_set(captures: &[Capture]) -> Result<(), CliError> {
    if captures.is_empty() {
        return Err(CliError::Other(
            "invalid capture: nothing to capture".into(),
        ));
    }
    for (i, a) in captures.iter().enumerate() {
        a.validate()?;
        for b in &captures[..i] {
            if a.label.eq_ignore_ascii_case(&b.label) {
                return Err(CliError::Other(format!(
                    "invalid capture: two captures are labelled `{}`",
                    a.label
                )));
            }
            if a.hosts
                .iter()
                .any(|x| b.hosts.iter().any(|y| x.overlaps(y)))
            {
                return Err(CliError::Other(format!(
                    "invalid capture: `{}` and `{}` read from overlapping hosts",
                    b.label, a.label
                )));
            }
        }
    }
    Ok(())
}

/// A session read out of the window. The values are [`Secret`]s: they
/// never print through `Debug` or `Display` and are zeroized when dropped.
#[derive(Debug)]
pub struct Captured {
    label: String,
    host: String,
    value: Option<Secret>,
    cookies: Vec<(String, Secret)>,
}

impl Captured {
    /// The capture's label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The host of the page it was read from.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The expression's answer; `None` for a capture without one.
    pub fn value(&self) -> Option<&Secret> {
        self.value.as_ref()
    }

    /// The expression's answer, by value.
    pub fn into_value(self) -> Option<Secret> {
        self.value
    }

    /// A cookie the capture named, if it was set.
    pub fn cookie(&self, name: &str) -> Option<&Secret> {
        self.cookies
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value)
    }
}

/// Launch, wait for one capture, and tear the window down: the whole
/// `auth login --browser` mechanism in one call. The capture is validated
/// before Chrome starts.
pub fn capture(launch: Launch, capture: &Capture, timeout: Duration) -> Result<Captured, CliError> {
    capture.validate()?;
    launch.spawn()?.wait(capture, timeout)
}

impl Browser {
    /// Wait up to `timeout` for `capture` to be ready, read it, and close
    /// its tab(s).
    ///
    /// Errors: timeout, or the window closed before the sign-in finished,
    /// is exit 3 naming what was (not) seen and the command to re-run;
    /// Ctrl-C is "browser login canceled" (exit 1).
    pub fn wait(&mut self, capture: &Capture, timeout: Duration) -> Result<Captured, CliError> {
        let mut out = None;
        self.wait_each(std::slice::from_ref(capture), timeout, |captured| {
            out = Some(captured);
            Ok(())
        })?;
        out.ok_or_else(|| CliError::Other("the browser wait ended without a capture".into()))
    }

    /// Wait for several captures in one window (one tab per provider),
    /// handing each to `on_capture` as soon as it is read, so one provider's
    /// session is never lost to another's timeout. The captures must not
    /// share hosts. An error from `on_capture` ends the wait.
    pub fn wait_each(
        &mut self,
        captures: &[Capture],
        timeout: Duration,
        mut on_capture: impl FnMut(Captured) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        let settings = WaitSettings {
            timeout,
            poll_interval: POLL_INTERVAL,
            call_timeout: CALL_TIMEOUT,
            login_hint: self
                .launch
                .login_hint
                .clone()
                .unwrap_or_else(|| format!("{} auth login --browser", self.launch.bin)),
        };
        let quiet = self.launch.quiet;
        let mut note = |line: &str| {
            if !quiet {
                eprintln!("{line}");
            }
        };
        run_wait(self, captures, &settings, &mut note, &mut on_capture)
    }
}

/// What the wait needs from a running browser; the in-memory fake in the
/// tests implements it too.
pub(crate) trait Driver {
    type T: Transport;
    fn connect(&mut self) -> Result<Self::T, String>;
    fn exited(&mut self) -> bool;
    fn canceled(&self) -> bool;
    fn cancel_check(&self) -> fn() -> bool;
}

impl Driver for Browser {
    type T = WsTransport;

    fn connect(&mut self) -> Result<WsTransport, String> {
        WsTransport::connect(self.port, &self.ws_path)
    }

    fn exited(&mut self) -> bool {
        Browser::exited(self)
    }

    fn canceled(&self) -> bool {
        interrupt::canceled()
    }

    fn cancel_check(&self) -> fn() -> bool {
        interrupt::canceled
    }
}

pub(crate) struct WaitSettings {
    pub timeout: Duration,
    pub poll_interval: Duration,
    pub call_timeout: Duration,
    pub login_hint: String,
}

/// Why a page did not yield the session on its last check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Check {
    /// `null`, `undefined`, or a blank string: not signed in yet.
    Empty,
    /// A value that is not a string (its JavaScript type).
    NotString(&'static str),
    /// The expression threw.
    Threw,
    /// The answering document was not on an allowed host.
    LeftHost,
    /// The answer was not the shape the wrapper returns.
    Unreadable,
    /// The page did not answer (attach failed, timed out, navigating).
    NoAnswer(String),
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Progress {
    saw_page: bool,
    /// The host of the page last checked, and why it was not ready.
    check: Option<(String, Check)>,
    missing_cookies: Vec<String>,
}

struct Waiting<'a> {
    capture: &'a Capture,
    progress: Progress,
    done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageTarget {
    pub id: String,
    pub url: String,
}

/// The page targets in a `Target.getTargets` result. Service workers,
/// iframes, extensions' background pages and the browser itself are not
/// pages and never read.
pub(crate) fn page_targets(result: &Value) -> Vec<PageTarget> {
    result
        .get("targetInfos")
        .and_then(Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                .filter_map(|t| {
                    Some(PageTarget {
                        id: t.get("targetId")?.as_str()?.to_string(),
                        url: t.get("url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The capture's expression, wrapped to also answer with the evaluating
/// document's own host. The newline lets an expression end in a `//`
/// comment.
pub(crate) fn wrap_expression(expression: &str) -> String {
    format!("(async () => [location.hostname, await ({expression}\n)])()")
}

pub(crate) enum Evaluation {
    Ready { host: String, value: Secret },
    NotReady(Check),
}

/// Read a `Runtime.evaluate` result of [`wrap_expression`]. The value is
/// moved into a [`Secret`] at once.
pub(crate) fn read_evaluation(result: &Value, hosts: &[HostRule]) -> Evaluation {
    if result.get("exceptionDetails").is_some() {
        return Evaluation::NotReady(Check::Threw);
    }
    let Some([host, value]) = result
        .pointer("/result/value")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    else {
        return Evaluation::NotReady(Check::Unreadable);
    };
    let Some(host) = host
        .as_str()
        .filter(|h| hosts.iter().any(|r| r.matches_host(h)))
    else {
        return Evaluation::NotReady(Check::LeftHost);
    };
    match value {
        Value::Null => Evaluation::NotReady(Check::Empty),
        Value::String(s) if s.trim().is_empty() => Evaluation::NotReady(Check::Empty),
        Value::String(s) => Evaluation::Ready {
            host: host.to_ascii_lowercase(),
            value: Secret::new(s.as_str()),
        },
        Value::Bool(_) => Evaluation::NotReady(Check::NotString("boolean")),
        Value::Number(_) => Evaluation::NotReady(Check::NotString("number")),
        Value::Array(_) | Value::Object(_) => Evaluation::NotReady(Check::NotString("object")),
    }
}

/// The value of cookie `name` that the browser would send with a request
/// to `url`, from a `Storage.getCookies` list: domain-matched (a host-only
/// cookie only on its own host, a domain cookie on the domain and its
/// subdomains), path-matched, `Secure` only over https, and a partitioned
/// cookie only in its own top-level site. Of several matches, the longest
/// path wins, then a host-only cookie, as the browser orders them. An empty
/// value counts as unset.
pub(crate) fn pick_cookie(cookies: &Value, url: &UrlParts, name: &str) -> Option<String> {
    let under = |host: &str, domain: &str| {
        host == domain
            || (host.len() > domain.len()
                && host.ends_with(domain)
                && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
    };
    cookies
        .as_array()?
        .iter()
        .filter(|c| c.get("name").and_then(Value::as_str) == Some(name))
        .filter_map(|c| {
            let value = c.get("value")?.as_str().filter(|v| !v.is_empty())?;
            let raw_domain = c.get("domain")?.as_str()?;
            let host_only = !raw_domain.starts_with('.');
            let domain = crate::host::normalize_host(raw_domain.trim_start_matches('.'))?;
            let domain_ok = if host_only {
                url.host == domain
            } else {
                under(&url.host, &domain)
            };
            let path = c.get("path").and_then(Value::as_str).unwrap_or("/");
            let secure = c.get("secure").and_then(Value::as_bool).unwrap_or(false);
            let partition_ok = match c.get("partitionKey") {
                None | Some(Value::Null) => true,
                Some(key) => {
                    let cross_site = key
                        .get("hasCrossSiteAncestor")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let site = key
                        .as_str()
                        .or_else(|| key.get("topLevelSite").and_then(Value::as_str))
                        .and_then(parse_url);
                    !cross_site && site.is_some_and(|s| under(&url.host, &s.host))
                }
            };
            (domain_ok
                && path_matches(&url.path, path)
                && (!secure || url.scheme == "https")
                && partition_ok)
                .then_some((path.len(), host_only, value))
        })
        .max_by_key(|(path_len, host_only, _)| (*path_len, *host_only))
        .map(|(_, _, value)| value.to_string())
}

/// RFC 6265 §5.1.4 path-match.
fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request[cookie.len()..].starts_with('/')))
}

/// One capture read in a poll, and how closing its tabs went.
struct Read {
    index: usize,
    captured: Captured,
    closed: Result<usize, String>,
}

struct Poll {
    pages: usize,
    /// Captures read (and their tabs closed) in this poll. Always handed
    /// over, even when the poll was cut short by a cancel: a read whose tab
    /// is closed cannot be read again.
    reads: Vec<Read>,
    canceled: bool,
}

fn poll_once<T: Transport>(
    cdp: &mut Cdp<T>,
    waiting: &mut [Waiting<'_>],
) -> Result<Poll, CdpError> {
    let pages = page_targets(&cdp.call("Target.getTargets", Value::Null, None)?);
    // The browser's cookie jar, read at most once per poll.
    let mut jar: Option<Result<Value, CdpError>> = None;
    let mut reads = Vec::new();
    let mut canceled = false;
    'captures: for (index, w) in waiting.iter_mut().enumerate() {
        if w.done {
            continue;
        }
        let matching: Vec<&PageTarget> = pages
            .iter()
            .filter(|p| w.capture.hosts.iter().any(|r| r.matches_url(&p.url)))
            .collect();
        let Some(first) = matching.first() else {
            continue;
        };
        w.progress.saw_page = true;

        let (host, value) = match &w.capture.expression {
            Some(expression) => {
                let mut ready = None;
                for page in &matching {
                    match evaluate(cdp, page, expression, &w.capture.hosts) {
                        Ok(Evaluation::Ready { host, value }) => {
                            w.progress.check = None;
                            ready = Some((host, Some(value)));
                            break;
                        }
                        Ok(Evaluation::NotReady(check)) => {
                            w.progress.check = Some((page_host(page), check));
                        }
                        Err(CdpError::Canceled) => {
                            canceled = true;
                            break 'captures;
                        }
                        Err(e) => {
                            w.progress.check =
                                Some((page_host(page), Check::NoAnswer(e.to_string())));
                        }
                    }
                }
                match ready {
                    Some(ready) => ready,
                    None => continue,
                }
            }
            None => (page_host(first), None),
        };

        let mut cookies = Vec::new();
        let mut missing = Vec::new();
        if !w.capture.cookies.is_empty() {
            let jar = jar.get_or_insert_with(|| {
                cdp.call("Storage.getCookies", Value::Null, None)
                    .map(|all| all.get("cookies").cloned().unwrap_or(Value::Null))
            });
            let jar = match jar {
                Ok(jar) => &*jar,
                Err(CdpError::Canceled) => {
                    canceled = true;
                    break 'captures;
                }
                // Not ready this poll; nothing was closed, so the next poll
                // reads it again.
                Err(e) => {
                    w.progress.check = Some((
                        host,
                        Check::NoAnswer(format!("reading the cookies failed: {e}")),
                    ));
                    continue;
                }
            };
            for read in &w.capture.cookies {
                let found = parse_url(&read.url).and_then(|u| pick_cookie(jar, &u, &read.name));
                match found {
                    Some(v) => cookies.push((read.name.clone(), Secret::new(v))),
                    None if read.required => missing.push(read.name.clone()),
                    None => {}
                }
            }
        }
        w.progress.missing_cookies = missing;
        if !w.progress.missing_cookies.is_empty() {
            continue;
        }

        let closed = close_pages(cdp, &matching);
        w.done = true;
        reads.push(Read {
            index,
            captured: Captured {
                label: w.capture.label.clone(),
                host,
                value,
                cookies,
            },
            closed,
        });
    }
    Ok(Poll {
        pages: pages.len(),
        reads,
        canceled,
    })
}

fn page_host(page: &PageTarget) -> String {
    parse_url(&page.url)
        .map(|u| u.host)
        .unwrap_or_else(|| "the page".into())
}

fn evaluate<T: Transport>(
    cdp: &mut Cdp<T>,
    page: &PageTarget,
    expression: &str,
    hosts: &[HostRule],
) -> Result<Evaluation, CdpError> {
    let attached = cdp.call(
        "Target.attachToTarget",
        json!({ "targetId": page.id, "flatten": true }),
        None,
    )?;
    let session = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| CdpError::Protocol {
            code: 0,
            message: "attaching to the page returned no session".into(),
        })?
        .to_string();
    let result = cdp.call(
        "Runtime.evaluate",
        json!({
            "expression": wrap_expression(expression),
            "returnByValue": true,
            "awaitPromise": true,
            "silent": true,
        }),
        Some(&session),
    );
    let evaluation = result.map(|r| read_evaluation(&r, hosts));
    if !matches!(evaluation, Err(CdpError::Canceled)) {
        let _ = cdp.call(
            "Target.detachFromTarget",
            json!({ "sessionId": session }),
            None,
        );
    }
    evaluation
}

/// Close every page on the capture's hosts. `Ok(n)` when at least one
/// closed.
fn close_pages<T: Transport>(cdp: &mut Cdp<T>, pages: &[&PageTarget]) -> Result<usize, String> {
    let mut closed = 0;
    let mut first_error = None;
    for page in pages {
        match cdp.call("Target.closeTarget", json!({ "targetId": page.id }), None) {
            Ok(_) => closed += 1,
            Err(e) => {
                first_error.get_or_insert_with(|| e.to_string());
            }
        }
    }
    match (closed, first_error) {
        (0, Some(e)) => Err(e),
        (n, _) => Ok(n),
    }
}

pub(crate) fn run_wait<D: Driver>(
    driver: &mut D,
    captures: &[Capture],
    settings: &WaitSettings,
    note: &mut dyn FnMut(&str),
    on_capture: &mut dyn FnMut(Captured) -> Result<(), CliError>,
) -> Result<(), CliError> {
    validate_set(captures)?;
    let mut waiting: Vec<Waiting> = captures
        .iter()
        .map(|capture| Waiting {
            capture,
            progress: Progress::default(),
            done: false,
        })
        .collect();
    note(&announce(captures, settings.timeout));

    // `None`: a timeout too large to add to the clock, i.e. no deadline.
    let deadline = Instant::now().checked_add(settings.timeout);
    let mut saw_pages = false;
    let mut empty_polls = 0;
    loop {
        if driver.canceled() {
            return Err(canceled());
        }
        if driver.exited() {
            return Err(window_closed(&settings.login_hint, &waiting));
        }
        let poll = match driver.connect() {
            Err(e) => Err(e),
            Ok(transport) => {
                let mut cdp = Cdp::new(transport, settings.call_timeout, driver.cancel_check());
                match poll_once(&mut cdp, &mut waiting) {
                    Err(CdpError::Canceled) => return Err(canceled()),
                    Err(e) => Err(e.to_string()),
                    Ok(poll) => Ok(poll),
                }
            }
        };
        // The last poll's failure to reach the debug port, for the timeout.
        let port_error = match poll {
            Err(e) => Some(e),
            Ok(poll) => {
                if poll.pages > 0 {
                    saw_pages = true;
                    empty_polls = 0;
                } else if saw_pages {
                    empty_polls += 1;
                }
                for read in poll.reads {
                    note(&captured_note(
                        &waiting[read.index].capture.label,
                        &read.closed,
                    ));
                    on_capture(read.captured)?;
                }
                if poll.canceled {
                    return Err(canceled());
                }
                if waiting.iter().all(|w| w.done) {
                    return Ok(());
                }
                if empty_polls >= CLOSED_POLLS {
                    return Err(window_closed(&settings.login_hint, &waiting));
                }
                None
            }
        };
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(timed_out(
                &settings.login_hint,
                settings.timeout,
                &waiting,
                port_error.as_deref(),
            ));
        }
        pause(driver, settings.poll_interval, deadline)?;
    }
}

/// Sleep until the next poll (or the deadline), waking to notice a cancel.
fn pause<D: Driver>(
    driver: &D,
    interval: Duration,
    deadline: Option<Instant>,
) -> Result<(), CliError> {
    let next = Instant::now() + interval;
    let until = deadline.map_or(next, |d| next.min(d));
    loop {
        if driver.canceled() {
            return Err(canceled());
        }
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        std::thread::sleep((until - now).min(Duration::from_millis(100)));
    }
}

fn canceled() -> CliError {
    CliError::Other("browser login canceled".into())
}

fn duration_words(d: Duration) -> String {
    let secs = d.as_secs();
    let (n, unit) = if secs >= 60 && secs % 60 == 0 {
        (secs / 60, "minute")
    } else {
        (secs, "second")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The status line printed when the wait starts.
pub(crate) fn announce(captures: &[Capture], timeout: Duration) -> String {
    let limit = if timeout >= NO_LIMIT {
        "no time limit; Ctrl-C cancels".to_string()
    } else {
        format!("up to {}; Ctrl-C cancels", duration_words(timeout))
    };
    match captures {
        [one] => {
            let hint = one
                .hint
                .as_ref()
                .map(|h| format!(" ({h})"))
                .unwrap_or_default();
            format!(
                "A Chrome window opened at the {} sign-in. Sign in there{hint}; this waits for \
                 the session and captures it ({limit}).",
                one.label
            )
        }
        many => {
            let labels: Vec<&str> = many.iter().map(|c| c.label.as_str()).collect();
            let hints: Vec<String> = many
                .iter()
                .filter_map(|c| c.hint.as_ref().map(|h| format!("{}: {h}", c.label)))
                .collect();
            let hints = if hints.is_empty() {
                String::new()
            } else {
                format!(" ({})", hints.join("; "))
            };
            format!(
                "A Chrome window opened with {}. Sign in to each there{hints}; this waits for \
                 the sessions and captures each as it is ready ({limit}).",
                and_list(&labels)
            )
        }
    }
}

/// The status line printed as a capture is read.
pub(crate) fn captured_note(label: &str, closed: &Result<usize, String>) -> String {
    match closed {
        Ok(1) => format!("{label}: session captured, tab closed."),
        Ok(n) => format!("{label}: session captured, {n} tabs closed."),
        Err(e) => format!(
            "{label}: session captured; its tab could not be closed ({e}), so it closes with \
             the window."
        ),
    }
}

fn pending_labels<'a>(waiting: &'a [Waiting<'_>]) -> Vec<&'a str> {
    waiting
        .iter()
        .filter(|w| !w.done)
        .map(|w| w.capture.label.as_str())
        .collect()
}

fn sign_ins(labels: &[&str]) -> String {
    let s = if labels.len() == 1 { "" } else { "s" };
    format!("{} sign-in{s}", and_list(labels))
}

fn window_closed(login_hint: &str, waiting: &[Waiting<'_>]) -> CliError {
    CliError::Auth(format!(
        "the Chrome window was closed before the {} finished. Re-run `{login_hint}` and \
         finish signing in.",
        sign_ins(&pending_labels(waiting))
    ))
}

/// What a capture's progress says about why it never became ready. Names
/// hosts and cookie names only, never a value.
pub(crate) fn diagnose(capture: &Capture, progress: &Progress) -> String {
    if !progress.saw_page {
        let hosts: Vec<String> = capture.hosts.iter().map(ToString::to_string).collect();
        return format!("no page on {} was open", hosts.join(" or "));
    }
    if !progress.missing_cookies.is_empty() {
        let names: Vec<&str> = progress
            .missing_cookies
            .iter()
            .map(String::as_str)
            .collect();
        let s = if names.len() == 1 { " was" } else { "s were" };
        return format!("the {} cookie{s} not set", and_list(&names));
    }
    match &progress.check {
        Some((host, Check::Empty)) => format!("{host} was open but held no session yet"),
        Some((host, Check::NotString(kind))) => {
            format!("the session check on {host} returned a {kind}, not a string")
        }
        Some((host, Check::Threw)) => format!("the session check on {host} threw an exception"),
        Some((host, Check::LeftHost)) => {
            format!("the page on {host} navigated away during the session check")
        }
        Some((host, Check::Unreadable)) => {
            format!("the session check on {host} returned an unreadable answer")
        }
        Some((host, Check::NoAnswer(e))) => {
            format!("the session check on {host} got no answer ({e})")
        }
        None => "a page was open, but the session was not ready".into(),
    }
}

fn timed_out(
    login_hint: &str,
    timeout: Duration,
    waiting: &[Waiting<'_>],
    port_error: Option<&str>,
) -> CliError {
    let pending: Vec<&Waiting> = waiting.iter().filter(|w| !w.done).collect();
    let mut why = match pending.as_slice() {
        [one] => diagnose(one.capture, &one.progress),
        many => many
            .iter()
            .map(|w| format!("{}: {}", w.capture.label, diagnose(w.capture, &w.progress)))
            .collect::<Vec<_>>()
            .join("; "),
    };
    if let Some(e) = port_error {
        why.push_str(&format!("; Chrome's debug port: {e}"));
    }
    CliError::Auth(format!(
        "timed out after {}: the {} not completed in the browser window ({why}). Re-run \
         `{login_hint}` and finish signing in.",
        duration_words(timeout),
        {
            let labels = pending_labels(waiting);
            let verb = if labels.len() == 1 { "was" } else { "were" };
            format!("{} {verb}", sign_ins(&labels))
        }
    ))
}

#[cfg(test)]
mod tests;
