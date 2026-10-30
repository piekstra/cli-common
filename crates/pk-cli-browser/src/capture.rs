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
//!    `Target.detachFromTarget`; never `Runtime.enable`), and requires the
//!    answer to come from a document whose own `location.hostname` is
//!    allowed, not just a target whose URL was;
//! 4. reads the capture's cookies, and its domain's whole jar, from the
//!    browser-wide jar (`Storage.getCookies`, which includes `HttpOnly`
//!    cookies), choosing per name the cookie the browser would send to the
//!    given URL;
//! 5. for a capture that watches requests, checks that the request it needs
//!    was seen, and holds the tab open for its window once the rest is
//!    ready (the pages' network events arrive on the watch, a longer-lived
//!    connection described in `watch.rs`);
//! 6. hands what it read to the CLI's confirm hook, if the capture has one,
//!    whose answer leaves the tab open for the next poll, lets the capture
//!    through, or ends the wait;
//! 7. once everything required is there, closes every page on the capture's
//!    hosts (`Target.closeTarget`) before anything else happens, so the web
//!    app cannot renew, and so rotate, the token it just handed over.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pk_cli_core::CliError;
use pk_cli_secrets::Secret;
use serde_json::{json, Value};

use crate::cdp::{Cdp, CdpError, Transport, WsTransport};
use crate::chrome::{Browser, Launch};
use crate::error::BrowserError;
use crate::host::{parse_url, HostRule};
use crate::interrupt;
use crate::jar::{cookie_header, jar_cookies, jar_names, pick_cookie, JarCookie};
use crate::watch::{
    CapturedRequest, LoggedRequest, Next, RequestLog, RequestRule, SeenRequest, Watch,
};

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
/// Messages one pass over the watch applies before the wait looks at the
/// clock again, so a page that never stops talking cannot hold up a poll.
const PUMP_BOUND: usize = 256;
/// Cookie names a timeout lists before counting the rest.
const NAMES_SHOWN: usize = 20;

/// One session to read out of the sign-in window: the host(s) it may be read
/// from and what to read there.
///
/// It is ready when a page on one of its hosts is open, its
/// [`expression`](Capture::expression) (if any) answers with a non-empty
/// string in such a page, every [`cookie`](Capture::cookie) is set, its
/// [`cookie_jar`](Capture::cookie_jar) (if any) holds a cookie, the
/// [`request`](Capture::request) it waits for (if any) was seen, its
/// watch window (if any) is over, and its [`confirm`](Capture::confirm)
/// hook (if any) says so. Choose a signal that only exists once the owner
/// is signed in (an access token in the stored session, a cookie set only
/// after login), or confirm with one provider read; the tab is closed the
/// moment it is read, so a premature read cannot be retried.
#[derive(Debug, Clone)]
pub struct Capture {
    label: String,
    hosts: Vec<HostRule>,
    expression: Option<String>,
    cookies: Vec<CookieRead>,
    jar: Option<String>,
    request: Option<RequestRead>,
    log: Option<LogRead>,
    user_agent: bool,
    confirm: Option<ConfirmHook>,
    hint: Option<String>,
}

#[derive(Debug, Clone)]
struct CookieRead {
    url: String,
    name: String,
    required: bool,
}

#[derive(Debug, Clone)]
struct RequestRead {
    url: String,
    /// `None`: the capture waits for it. `Some(d)`: once the rest is ready,
    /// the tab is held open at most `d` for it.
    within: Option<Duration>,
}

#[derive(Debug, Clone)]
struct LogRead {
    domain: String,
    during: Duration,
}

/// The CLI's check, shared by clones of the capture.
#[derive(Clone)]
struct ConfirmHook(Arc<dyn Fn(&Captured) -> Confirm + Send + Sync>);

impl fmt::Debug for ConfirmHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfirmHook(..)")
    }
}

/// What a capture's [`confirm`](Capture::confirm) hook decided about the
/// session it was shown.
#[derive(Debug)]
#[non_exhaustive]
pub enum Confirm {
    /// The session works: close the tab and hand the capture over.
    Ready,
    /// Not signed in yet (the provider answered as it does a visitor): keep
    /// the tab open and ask again on the next poll. The reason is shown in
    /// the timeout error if the wait runs out, so it names what was seen
    /// (`GET /me answered 401`), never a value.
    NotYet(String),
    /// Something waiting will not fix (the provider is down, the session is
    /// refused for another reason): end the wait with this error, as
    /// [`BrowserError::Handler`].
    Fail(CliError),
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
            jar: None,
            request: None,
            log: None,
            user_agent: false,
            confirm: None,
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

    /// Every cookie the browser would send to `domain` or to a host under
    /// it, for a site where no single cookie is known to carry the session,
    /// or one that splits it across hosts (`login.example.com` and
    /// `www.example.com`): host-only and domain cookies on those hosts, a
    /// domain cookie on a parent of `domain`, `HttpOnly` included, at every
    /// path. Matching is label for label, so `notexample.com` and
    /// `example.com.evil.example` are never in the jar of `example.com`.
    ///
    /// `domain` is a registrable domain or a host under one, and one of the
    /// capture's hosts must be on it or under it. The capture waits until
    /// the jar holds a cookie; it is read in the same poll as the rest and
    /// handed over as [`Captured::jar`] and [`Captured::cookie_header`]. A
    /// later call replaces an earlier one.
    pub fn cookie_jar(mut self, domain: impl Into<String>) -> Self {
        self.jar = Some(domain.into());
        self
    }

    /// Watch the capture's pages for a request to `url`, and wait until one
    /// is made. `url` is an `https` URL on one of the capture's hosts,
    /// without a query: a request matches on that host and that path or a
    /// path under it (on a `/` boundary), with any query. The first match
    /// is handed over as [`Captured::request`]: its method, its URL without
    /// the query, and the headers the page set, values as [`Secret`]s.
    ///
    /// Watching enables the `Network` domain in the capture's own pages, and
    /// in no other (never a page on the login host), from the moment one is
    /// open; see the crate docs. A later call, or one to
    /// [`Capture::optional_request`], replaces an earlier one.
    pub fn request(mut self, url: impl Into<String>) -> Self {
        self.request = Some(RequestRead {
            url: url.into(),
            within: None,
        });
        self
    }

    /// As [`Capture::request`], but the capture does not wait for it
    /// indefinitely: once everything else is ready, the tab is held open
    /// up to `within` for a matching request, and the capture goes ahead
    /// without one if none comes. A request made since the first page on
    /// the capture's hosts opened counts, so one the app makes as it loads
    /// is not missed.
    pub fn optional_request(mut self, url: impl Into<String>, within: Duration) -> Self {
        self.request = Some(RequestRead {
            url: url.into(),
            within: Some(within),
        });
        self
    }

    /// Log the endpoints the capture's pages call on `domain` or a host
    /// under it: each page load, `XMLHttpRequest` and `fetch()`, once per
    /// method, host and path, with the first response's status and content
    /// type. No header, body or query is kept, so nothing in the log is
    /// secret. For mapping an undocumented web app's API.
    ///
    /// Logging starts as soon as a page on the capture's hosts is open (it
    /// enables `Network` there, as [`Capture::request`] does). Once
    /// everything else is ready, the tab is held open `during` more so the
    /// owner can use the app, then read and closed as usual; the log is
    /// handed over as [`Captured::request_log`]. `domain` follows the rule
    /// of [`Capture::cookie_jar`]. A later call replaces an earlier one.
    pub fn request_log(mut self, domain: impl Into<String>, during: Duration) -> Self {
        self.log = Some(LogRead {
            domain: domain.into(),
            during,
        });
        self
    }

    /// Also read the browser's `User-Agent` (a browser-level call no page
    /// can observe), handed over as [`Captured::user_agent`], for a provider
    /// that binds the session to the browser that earned it (a Cloudflare
    /// clearance cookie), so the CLI can send the same one.
    pub fn with_user_agent(mut self) -> Self {
        self.user_agent = true;
        self
    }

    /// A check the CLI runs on each poll once everything the capture reads
    /// is there, before any tab is closed: typically one provider read made
    /// with the candidate session, for a site that sets its cookies before
    /// sign-in finishes. It is shown the candidate [`Captured`] and answers
    /// with a [`Confirm`]: [`Confirm::NotYet`] leaves the tab open and asks
    /// again on the next poll (its reason names what the wait was stuck on
    /// if it times out), [`Confirm::Ready`] closes the tab and hands the
    /// capture over, and [`Confirm::Fail`] ends the wait with
    /// [`BrowserError::Handler`].
    ///
    /// The request is the CLI's own: the crate never sends one, it calls the
    /// hook and acts on the answer. The poll waits for the hook, Ctrl-C
    /// included, so the hook bounds its own request with a timeout. A later
    /// call replaces an earlier one.
    pub fn confirm<F>(mut self, check: F) -> Self
    where
        F: Fn(&Captured) -> Confirm + Send + Sync + 'static,
    {
        self.confirm = Some(ConfirmHook(Arc::new(check)));
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
    /// non-empty expression, each cookie an `https` URL on one of the
    /// capture's hosts with a valid, distinct name, a jar or log domain that
    /// is registrable and that one of its hosts is on, and a watched request
    /// that is an `https` URL on its hosts without a query. The waits call
    /// this before reading anything; a CLI can call it in a unit test.
    pub fn validate(&self) -> Result<(), BrowserError> {
        let bad = |why: String| Err(BrowserError::Invalid(format!("invalid capture: {why}")));
        if self.label.trim().is_empty() {
            return bad("the label is empty".into());
        }
        let label = &self.label;
        if self.expression.is_none()
            && self.cookies.is_empty()
            && self.jar.is_none()
            && self.request.is_none()
            && self.log.is_none()
        {
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
        if let Some(domain) = &self.jar {
            self.domain_rule(domain, "reads the cookie jar of")?;
        }
        if let Some(read) = &self.request {
            let on_host = RequestRule::parse(&read.url)
                .is_some_and(|r| self.hosts.iter().any(|h| h.matches_host(r.host())));
            if !on_host {
                return bad(format!(
                    "`{label}` watches for a request to {}, which is not an https URL on its \
                     hosts without a query",
                    read.url
                ));
            }
        }
        if let Some(log) = &self.log {
            self.domain_rule(&log.domain, "logs the requests to")?;
        }
        Ok(())
    }

    /// The rule for a jar or log domain: a registrable domain (or a host
    /// under one) that one of the capture's hosts is on or under, so a
    /// capture never reads another site's cookies or traffic.
    fn domain_rule(&self, domain: &str, what: &str) -> Result<HostRule, BrowserError> {
        let label = &self.label;
        let bad = |why: &str| {
            BrowserError::Invalid(format!(
                "invalid capture: `{label}` {what} `{domain}`, {why}"
            ))
        };
        let rule =
            HostRule::domain(domain).map_err(|_| bad("which is not a registrable domain"))?;
        if !self.hosts.iter().any(|h| h.overlaps(&rule)) {
            return Err(bad("which none of its hosts is on"));
        }
        Ok(rule)
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
fn validate_set(captures: &[Capture]) -> Result<(), BrowserError> {
    if captures.is_empty() {
        return Err(BrowserError::Invalid(
            "invalid capture: nothing to capture".into(),
        ));
    }
    for (i, a) in captures.iter().enumerate() {
        a.validate()?;
        for b in &captures[..i] {
            if a.label.eq_ignore_ascii_case(&b.label) {
                return Err(BrowserError::Invalid(format!(
                    "invalid capture: two captures are labelled `{}`",
                    a.label
                )));
            }
            if a.hosts
                .iter()
                .any(|x| b.hosts.iter().any(|y| x.overlaps(y)))
            {
                return Err(BrowserError::Invalid(format!(
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
    jar: Option<Vec<JarCookie>>,
    request: Option<CapturedRequest>,
    request_log: Option<Vec<LoggedRequest>>,
    user_agent: Option<String>,
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

    /// The jar a [`Capture::cookie_jar`] read, in the order the browser
    /// lists it; empty for a capture without one.
    pub fn jar(&self) -> &[JarCookie] {
        self.jar.as_deref().unwrap_or_default()
    }

    /// The jar as one `Cookie` request-header value (`name=value; …`),
    /// longer paths first; `None` for a capture without a jar. Two cookies
    /// of one name (on different domains or paths) are both in it, as the
    /// browser would send both to a URL both match; [`Captured::jar`] has
    /// their scopes for a CLI that keeps one.
    pub fn cookie_header(&self) -> Option<Secret> {
        self.jar.as_deref().map(cookie_header)
    }

    /// The request a [`Capture::request`] or [`Capture::optional_request`]
    /// saw; `None` for a capture without one, or when an optional one never
    /// came.
    pub fn request(&self) -> Option<&CapturedRequest> {
        self.request.as_ref()
    }

    /// The endpoints a [`Capture::request_log`] logged, in the order they
    /// were first seen; empty for a capture without one.
    pub fn request_log(&self) -> &[LoggedRequest] {
        self.request_log.as_deref().unwrap_or_default()
    }

    /// The browser's `User-Agent`, for a capture
    /// [`with_user_agent`](Capture::with_user_agent).
    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }
}

/// Launch, wait for one capture, and tear the window down: the whole
/// `auth login --browser` mechanism in one call. The capture is validated
/// before Chrome starts.
pub fn capture(
    launch: Launch,
    capture: &Capture,
    timeout: Duration,
) -> Result<Captured, BrowserError> {
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
    pub fn wait(&mut self, capture: &Capture, timeout: Duration) -> Result<Captured, BrowserError> {
        let mut out = None;
        self.wait_each(std::slice::from_ref(capture), timeout, |captured| {
            out = Some(captured);
            Ok(())
        })?;
        // Unreachable in practice: the wait returns `Ok` only once every
        // capture has been handed over.
        out.ok_or_else(|| {
            BrowserError::Invalid("the browser wait ended without its capture".into())
        })
    }

    /// Wait for several captures in one window (one tab per provider),
    /// handing each to `on_capture` as soon as it is read, so one provider's
    /// session is never lost to another's timeout. The captures must not
    /// share hosts. Each capture's own [`confirm`](Capture::confirm) hook
    /// runs before its tab is closed; `on_capture` runs after. An error from
    /// `on_capture`, or a [`Confirm::Fail`], ends the wait, once the
    /// captures already read in that poll are handed over.
    pub fn wait_each(
        &mut self,
        captures: &[Capture],
        timeout: Duration,
        mut on_capture: impl FnMut(Captured) -> Result<(), CliError>,
    ) -> Result<(), BrowserError> {
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

/// What held a capture back on the last poll that found its page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Blocked {
    /// The expression's answer on that host was not a session.
    Check(String, Check),
    /// These required cookies were not set.
    Cookies(Vec<String>),
    /// The jar of this domain held no cookie.
    EmptyJar(String),
    /// No request to this URL was seen yet.
    Request(String),
    /// Ready, and still inside its watch window.
    Holding,
    /// The confirm hook said not yet, for this reason.
    Unconfirmed(String),
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Progress {
    saw_page: bool,
    blocked: Option<Blocked>,
    /// The jar's domain and the names it held when last read, for the
    /// timeout.
    jar: Option<(String, Vec<String>)>,
}

struct Waiting<'a> {
    capture: &'a Capture,
    progress: Progress,
    done: bool,
    /// The capture's jar, request and log rules, parsed once (the capture
    /// was validated, so each is there when the capture names it).
    jar: Option<HostRule>,
    request: Option<RequestRule>,
    seen: Option<CapturedRequest>,
    log: Option<RequestLog>,
    /// When everything but the watch window was first ready.
    hold_since: Option<Instant>,
}

impl<'a> Waiting<'a> {
    fn new(capture: &'a Capture) -> Self {
        Waiting {
            capture,
            progress: Progress::default(),
            done: false,
            jar: capture
                .jar
                .as_deref()
                .and_then(|d| HostRule::domain(d).ok()),
            request: capture
                .request
                .as_ref()
                .and_then(|r| RequestRule::parse(&r.url)),
            seen: None,
            log: capture
                .log
                .as_ref()
                .and_then(|l| HostRule::domain(&l.domain).ok())
                .map(RequestLog::new),
            hold_since: None,
        }
    }

    /// Whether the capture watches its pages' requests.
    fn watches(&self) -> bool {
        self.request.is_some() || self.log.is_some()
    }

    /// How long the tab is held open once everything else is ready: the
    /// longer of an optional request's wait (until it is seen) and the log's
    /// window. `None` when there is nothing to hold for.
    fn window(&self) -> Option<Duration> {
        let request = match &self.capture.request {
            Some(RequestRead {
                within: Some(within),
                ..
            }) if self.seen.is_none() => Some(*within),
            _ => None,
        };
        request.max(self.capture.log.as_ref().map(|l| l.during))
    }
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

/// One capture read in a poll, and how closing its tabs went.
struct Read {
    index: usize,
    captured: Captured,
    closed: Result<usize, String>,
}

struct Poll {
    pages: Vec<PageTarget>,
    /// Captures read (and their tabs closed) in this poll. Always handed
    /// over, even when the poll was cut short by a cancel or a failed
    /// confirm: a read whose tab is closed cannot be read again.
    reads: Vec<Read>,
    /// Status lines to print before the reads are handed over.
    notes: Vec<String>,
    canceled: bool,
    /// A confirm hook's [`Confirm::Fail`], which ends the wait.
    failed: Option<CliError>,
}

/// What one poll shares between its captures.
struct PollState<'w> {
    /// The browser's cookie list, read at most once per poll.
    cookies: Option<Result<Value, CdpError>>,
    /// The browser's user agent, read at most once per wait.
    user_agent: &'w mut Option<String>,
    now: Instant,
    /// The deadline has passed: a watch window ends now, so a session that
    /// is ready is captured rather than lost to the timeout.
    final_round: bool,
}

/// How one capture fared in a poll.
enum Step {
    Waiting,
    Read(Box<Captured>, Result<usize, String>),
    Canceled,
    Failed(CliError),
}

/// Why a gate in [`read_when_ready`] stops a capture for this poll.
enum Gate {
    /// Not ready yet, and what stopped it, for the timeout. `None` keeps
    /// the last reason: no page on the capture's hosts was open to check.
    Wait(Option<Blocked>),
    /// Ctrl-C came during a call.
    Canceled,
    /// The confirm hook ended the wait.
    Failed(CliError),
}

impl From<Blocked> for Gate {
    fn from(blocked: Blocked) -> Self {
        Gate::Wait(Some(blocked))
    }
}

/// A gate that could not get an answer it needs from the browser.
fn no_answer(host: &str, why: String) -> Gate {
    Blocked::Check(host.to_string(), Check::NoAnswer(why)).into()
}

fn poll_once<T: Transport>(
    cdp: &mut Cdp<T>,
    waiting: &mut [Waiting<'_>],
    state: &mut PollState<'_>,
) -> Result<Poll, CdpError> {
    let pages = page_targets(&cdp.call("Target.getTargets", Value::Null, None)?);
    let mut poll = Poll {
        pages: Vec::new(),
        reads: Vec::new(),
        notes: Vec::new(),
        canceled: false,
        failed: None,
    };
    for (index, w) in waiting.iter_mut().enumerate() {
        if w.done {
            continue;
        }
        match check_capture(cdp, w, &pages, state, &mut poll.notes) {
            Step::Waiting => {}
            Step::Read(captured, closed) => {
                w.done = true;
                poll.reads.push(Read {
                    index,
                    captured: *captured,
                    closed,
                });
            }
            Step::Canceled => {
                poll.canceled = true;
                break;
            }
            Step::Failed(e) => {
                poll.failed = Some(e);
                break;
            }
        }
    }
    poll.pages = pages;
    Ok(poll)
}

/// Check one capture against this poll's pages, and read and close it if
/// it is ready. Whatever stops it is recorded in its progress for the
/// timeout.
fn check_capture<T: Transport>(
    cdp: &mut Cdp<T>,
    w: &mut Waiting<'_>,
    pages: &[PageTarget],
    state: &mut PollState<'_>,
    notes: &mut Vec<String>,
) -> Step {
    match read_when_ready(cdp, w, pages, state, notes) {
        Ok((captured, closed)) => {
            w.progress.blocked = None;
            Step::Read(Box::new(captured), closed)
        }
        Err(Gate::Wait(blocked)) => {
            if blocked.is_some() {
                w.progress.blocked = blocked;
            }
            Step::Waiting
        }
        Err(Gate::Canceled) => Step::Canceled,
        Err(Gate::Failed(e)) => Step::Failed(e),
    }
}

/// The gates, in the order the module docs give; the first one that does
/// not pass stops the capture for this poll. Once all pass, the capture is
/// read and every page on its hosts closed.
fn read_when_ready<T: Transport>(
    cdp: &mut Cdp<T>,
    w: &mut Waiting<'_>,
    pages: &[PageTarget],
    state: &mut PollState<'_>,
    notes: &mut Vec<String>,
) -> Result<(Captured, Result<usize, String>), Gate> {
    let capture = w.capture;
    let matching: Vec<&PageTarget> = pages
        .iter()
        .filter(|p| capture.hosts.iter().any(|r| r.matches_url(&p.url)))
        .collect();
    let first = *matching.first().ok_or(Gate::Wait(None))?;
    w.progress.saw_page = true;

    let (host, value) = expression_gate(cdp, capture, &matching, first)?;
    let CookieReads {
        named: cookies,
        jar,
    } = cookie_gate(cdp, w, state, &host)?;
    request_gate(w)?;
    hold_gate(w, state, notes)?;
    let user_agent = user_agent_gate(cdp, capture, state, &host)?;
    let candidate = Captured {
        label: capture.label.clone(),
        host,
        value,
        cookies,
        jar,
        request: w.seen.take(),
        request_log: w.log.as_ref().map(|log| log.entries().to_vec()),
        user_agent,
    };
    let captured = confirm_gate(w, candidate)?;
    let closed = close_pages(cdp, &matching);
    Ok((captured, closed))
}

/// The expression's answer and the host of the page that gave it, from the
/// first matching page that has one; for a capture without an expression,
/// nothing, read from the first page.
fn expression_gate<T: Transport>(
    cdp: &mut Cdp<T>,
    capture: &Capture,
    matching: &[&PageTarget],
    first: &PageTarget,
) -> Result<(String, Option<Secret>), Gate> {
    let Some(expression) = &capture.expression else {
        return Ok((page_host(first), None));
    };
    let mut blocked = None;
    for page in matching {
        let check = match evaluate(cdp, page, expression, &capture.hosts) {
            Ok(Evaluation::Ready { host, value }) => return Ok((host, Some(value))),
            Ok(Evaluation::NotReady(check)) => check,
            Err(CdpError::Canceled) => return Err(Gate::Canceled),
            Err(e) => Check::NoAnswer(e.to_string()),
        };
        blocked = Some(Blocked::Check(page_host(page), check));
    }
    Err(Gate::Wait(blocked))
}

/// What the cookie gate read: the named cookies that were set, and the jar.
struct CookieReads {
    named: Vec<(String, Secret)>,
    jar: Option<Vec<JarCookie>>,
}

/// The named cookies and the jar, from the browser's cookie list (read at
/// most once per poll). The jar's cookie names are recorded for the
/// timeout whatever the outcome; a missing required cookie is reported
/// ahead of an empty jar.
fn cookie_gate<T: Transport>(
    cdp: &mut Cdp<T>,
    w: &mut Waiting<'_>,
    state: &mut PollState<'_>,
    host: &str,
) -> Result<CookieReads, Gate> {
    let capture = w.capture;
    if capture.cookies.is_empty() && w.jar.is_none() {
        return Ok(CookieReads {
            named: Vec::new(),
            jar: None,
        });
    }
    let listed = state.cookies.get_or_insert_with(|| {
        cdp.call("Storage.getCookies", Value::Null, None)
            .map(|all| all.get("cookies").cloned().unwrap_or(Value::Null))
    });
    let listed = match listed {
        Ok(listed) => &*listed,
        Err(CdpError::Canceled) => return Err(Gate::Canceled),
        // Not ready this poll; nothing was closed, so the next poll reads
        // it again.
        Err(e) => return Err(no_answer(host, format!("reading the cookies failed: {e}"))),
    };
    let mut named = Vec::new();
    let mut missing = Vec::new();
    for read in &capture.cookies {
        let found = parse_url(&read.url).and_then(|u| pick_cookie(listed, &u, &read.name));
        match found {
            Some(v) => named.push((read.name.clone(), Secret::new(v))),
            None if read.required => missing.push(read.name.clone()),
            None => {}
        }
    }
    let jar = w.jar.as_ref().map(|rule| {
        let found = jar_cookies(listed, rule);
        w.progress.jar = Some((rule.host().to_string(), jar_names(&found)));
        found
    });
    if !missing.is_empty() {
        return Err(Blocked::Cookies(missing).into());
    }
    if let (Some(rule), Some(found)) = (&w.jar, &jar) {
        if found.is_empty() {
            return Err(Blocked::EmptyJar(rule.host().to_string()).into());
        }
    }
    Ok(CookieReads { named, jar })
}

/// A capture that waits for a request has seen it.
fn request_gate(w: &Waiting<'_>) -> Result<(), Gate> {
    let required = matches!(w.capture.request, Some(RequestRead { within: None, .. }));
    match (&w.request, &w.seen) {
        (Some(rule), None) if required => Err(Blocked::Request(rule.to_string()).into()),
        _ => Ok(()),
    }
}

/// The tab stays open for the capture's watch window, which starts (with a
/// status line) the first time every earlier gate passes, and ends when it
/// runs out or at the deadline's final round.
fn hold_gate(
    w: &mut Waiting<'_>,
    state: &PollState<'_>,
    notes: &mut Vec<String>,
) -> Result<(), Gate> {
    let Some(window) = w.window() else {
        return Ok(());
    };
    let since = match w.hold_since {
        Some(since) => since,
        None => {
            notes.push(hold_note(w, window));
            w.hold_since = Some(state.now);
            state.now
        }
    };
    let open = since.checked_add(window).is_none_or(|end| state.now < end);
    if open && !state.final_round {
        return Err(Blocked::Holding.into());
    }
    Ok(())
}

/// The browser's user agent, for a capture that asks for it; read once per
/// wait.
fn user_agent_gate<T: Transport>(
    cdp: &mut Cdp<T>,
    capture: &Capture,
    state: &mut PollState<'_>,
    host: &str,
) -> Result<Option<String>, Gate> {
    if !capture.user_agent {
        return Ok(None);
    }
    if state.user_agent.is_none() {
        let version = match cdp.call("Browser.getVersion", Value::Null, None) {
            Ok(version) => version,
            Err(CdpError::Canceled) => return Err(Gate::Canceled),
            Err(e) => {
                let why = format!("reading the browser's user agent failed: {e}");
                return Err(no_answer(host, why));
            }
        };
        *state.user_agent = version
            .get("userAgent")
            .and_then(Value::as_str)
            .filter(|ua| !ua.is_empty())
            .map(str::to_string);
    }
    match state.user_agent.clone() {
        Some(ua) => Ok(Some(ua)),
        None => Err(no_answer(host, "the browser reported no user agent".into())),
    }
}

/// The CLI's check of the candidate. On `NotYet` the watched request is
/// kept for the next poll; the rest is read again then.
fn confirm_gate(w: &mut Waiting<'_>, mut candidate: Captured) -> Result<Captured, Gate> {
    let capture = w.capture;
    let Some(hook) = &capture.confirm else {
        return Ok(candidate);
    };
    match (hook.0)(&candidate) {
        Confirm::Ready => Ok(candidate),
        Confirm::NotYet(reason) => {
            w.seen = candidate.request.take();
            Err(Blocked::Unconfirmed(reason).into())
        }
        Confirm::Fail(e) => Err(Gate::Failed(e)),
    }
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
) -> Result<(), BrowserError> {
    validate_set(captures)?;
    let mut waiting: Vec<Waiting> = captures.iter().map(Waiting::new).collect();
    note(&announce(captures, settings.timeout));

    // `None`: a timeout too large to add to the clock, i.e. no deadline.
    let deadline = Instant::now().checked_add(settings.timeout);
    let mut saw_pages = false;
    let mut empty_polls = 0;
    let mut user_agent = None;
    let mut watch: Option<Watch<D::T>> = None;
    let mut final_round = false;
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
                let mut state = PollState {
                    cookies: None,
                    user_agent: &mut user_agent,
                    now: Instant::now(),
                    final_round,
                };
                match poll_once(&mut cdp, &mut waiting, &mut state) {
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
                if !poll.pages.is_empty() {
                    saw_pages = true;
                    empty_polls = 0;
                } else if saw_pages {
                    empty_polls += 1;
                }
                for line in &poll.notes {
                    note(line);
                }
                for read in poll.reads {
                    note(&captured_note(
                        &waiting[read.index].capture.label,
                        &read.closed,
                    ));
                    on_capture(read.captured).map_err(BrowserError::Handler)?;
                }
                if let Some(e) = poll.failed {
                    return Err(BrowserError::Handler(e));
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
                follow_pages(driver, &mut watch, &poll.pages, &waiting, settings)?;
                None
            }
        };
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // A session that is ready but held open for its watch window is
            // captured rather than lost: one more poll, with the windows
            // ended, after reading what the watch has.
            if !final_round && waiting.iter().any(|w| !w.done && w.hold_since.is_some()) {
                final_round = true;
                pause(driver, Duration::ZERO, None, &mut watch, &mut waiting)?;
                continue;
            }
            return Err(timed_out(
                &settings.login_hint,
                settings.timeout,
                &waiting,
                port_error.as_deref(),
            ));
        }
        pause(
            driver,
            settings.poll_interval,
            deadline,
            &mut watch,
            &mut waiting,
        )?;
    }
}

/// Point the watch at the pages the watching captures have open now: open
/// it when the first such page appears, follow pages as they come, go and
/// leave their hosts, and drop it once no capture still waiting watches. A
/// watch that fails is dropped and opened again on a later poll (a request
/// made meanwhile is missed, the next one counts); only a cancel ends the
/// wait.
fn follow_pages<D: Driver>(
    driver: &mut D,
    watch: &mut Option<Watch<D::T>>,
    pages: &[PageTarget],
    waiting: &[Waiting<'_>],
    settings: &WaitSettings,
) -> Result<(), BrowserError> {
    let watching = |w: &&Waiting<'_>| !w.done && w.watches();
    if !waiting.iter().any(|w| watching(&w)) {
        *watch = None;
        return Ok(());
    }
    let wanted: Vec<(usize, &PageTarget)> = waiting
        .iter()
        .enumerate()
        .filter(|(_, w)| watching(w))
        .flat_map(|(index, w)| {
            pages
                .iter()
                .filter(move |p| w.capture.hosts.iter().any(|r| r.matches_url(&p.url)))
                .map(move |p| (index, p))
        })
        .collect();
    if watch.is_none() {
        if wanted.is_empty() {
            return Ok(());
        }
        let Ok(transport) = driver.connect() else {
            return Ok(());
        };
        let cdp = Cdp::new(transport, settings.call_timeout, driver.cancel_check());
        *watch = Some(Watch::new(cdp.keeping_events()));
    }
    if let Some(w) = watch.as_mut() {
        match w.sync(&wanted) {
            Ok(()) => {}
            Err(CdpError::Canceled) => return Err(canceled()),
            Err(_) => *watch = None,
        }
    }
    Ok(())
}

/// Wait until the next poll (or the deadline), waking to notice a cancel,
/// and meanwhile apply what the watch reads. The watch is read at least
/// once, even with no time to wait.
fn pause<D: Driver>(
    driver: &D,
    interval: Duration,
    deadline: Option<Instant>,
    watch: &mut Option<Watch<D::T>>,
    waiting: &mut [Waiting<'_>],
) -> Result<(), BrowserError> {
    let next = Instant::now() + interval;
    let until = deadline.map_or(next, |d| next.min(d));
    loop {
        if driver.canceled() {
            return Err(canceled());
        }
        if let Some(w) = watch.as_mut() {
            match pump(w, waiting) {
                Ok(()) => {}
                Err(CdpError::Canceled) => return Err(canceled()),
                Err(_) => *watch = None,
            }
        }
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        // With a watch, its read slice is the wait; without one, sleep.
        if watch.is_none() {
            std::thread::sleep((until - now).min(Duration::from_millis(100)));
        }
    }
}

/// Apply what the watch has read, until a quiet read slice or
/// [`PUMP_BOUND`] messages.
fn pump<T: Transport>(watch: &mut Watch<T>, waiting: &mut [Waiting<'_>]) -> Result<(), CdpError> {
    for _ in 0..PUMP_BOUND {
        match watch.next()? {
            Next::Quiet => return Ok(()),
            Next::Ignored => {}
            Next::Request(seen) => {
                if let Some(w) = waiting.get_mut(seen.capture) {
                    observe_request(w, seen);
                }
            }
            Next::Response(seen) => {
                let log = waiting
                    .get_mut(seen.capture)
                    .filter(|w| !w.done)
                    .and_then(|w| w.log.as_mut());
                if let Some(log) = log {
                    log.respond(&seen.id, seen.status, seen.mime);
                }
            }
        }
    }
    Ok(())
}

/// Take a request one of a capture's pages made: the first that matches
/// its watched URL, and any its log keeps. Only a request a document on the
/// capture's own hosts made counts, so neither a page that navigated off
/// them since the last poll nor a frame from another site inside one is
/// read.
fn observe_request(w: &mut Waiting<'_>, seen: SeenRequest) {
    let on_host = w
        .capture
        .hosts
        .iter()
        .any(|r| r.matches_url(&seen.document_url));
    if w.done || !on_host {
        return;
    }
    let Some(url) = parse_url(&seen.url) else {
        return;
    };
    if w.seen.is_none() && w.request.as_ref().is_some_and(|rule| rule.matches(&url)) {
        w.seen = Some(CapturedRequest::new(&seen.method, &url, &seen.headers));
    }
    if let Some(log) = w.log.as_mut() {
        log.observe(&seen.id, &seen.method, &url, &seen.kind);
    }
}

/// The status line printed when a capture whose session is in place
/// starts holding its tab open for its watch window. The confirm hook has
/// not run yet, so it claims no sign-in.
fn hold_note(w: &Waiting<'_>, window: Duration) -> String {
    let label = &w.capture.label;
    let window = duration_words(window);
    if let Some(log) = &w.log {
        return format!(
            "{label}: keeping the tab open {window} to record the page's requests to {} before \
             capturing the session; use the app there meanwhile, and leave the tab open.",
            log.domain()
        );
    }
    let request = w
        .request
        .as_ref()
        .map_or_else(|| "its request".to_string(), ToString::to_string);
    format!(
        "{label}: keeping the tab open up to {window} for the page to request {request} before \
         capturing the session."
    )
}

fn canceled() -> BrowserError {
    BrowserError::Canceled
}

fn duration_words(d: Duration) -> String {
    let secs = d.as_secs();
    let (n, unit) = if secs >= 60 && secs % 60 == 0 {
        (secs / 60, "minute")
    } else if secs == 0 && !d.is_zero() {
        (u64::from(d.subsec_millis()), "millisecond")
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

fn window_closed(login_hint: &str, waiting: &[Waiting<'_>]) -> BrowserError {
    BrowserError::WindowClosed(format!(
        "the Chrome window was closed before the {} finished. Re-run `{login_hint}` and \
         finish signing in.",
        sign_ins(&pending_labels(waiting))
    ))
}

/// What a capture's progress says about why it never became ready. Names
/// hosts, URLs and cookie names only, never a value.
pub(crate) fn diagnose(capture: &Capture, progress: &Progress) -> String {
    if !progress.saw_page {
        let hosts: Vec<String> = capture.hosts.iter().map(ToString::to_string).collect();
        return format!("no page on {} was open", hosts.join(" or "));
    }
    let mut why = match &progress.blocked {
        Some(Blocked::Check(host, check)) => check_words(host, check),
        Some(Blocked::Cookies(names)) => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let s = if names.len() == 1 { " was" } else { "s were" };
            format!("the {} cookie{s} not set", and_list(&names))
        }
        Some(Blocked::EmptyJar(domain)) => format!("no cookie for {domain} was set"),
        Some(Blocked::Request(rule)) => format!("no request to {rule} was seen"),
        Some(Blocked::Holding) => {
            "the session was in place, but the wait ended while its tab was held open".into()
        }
        Some(Blocked::Unconfirmed(reason)) => {
            format!("the session was read but not confirmed: {reason}")
        }
        None => "a page was open, but the session was not ready".into(),
    };
    let empty_jar = matches!(progress.blocked, Some(Blocked::EmptyJar(_)));
    if let (Some((domain, names)), false) = (&progress.jar, empty_jar) {
        if !names.is_empty() {
            why.push_str(&format!("; the {domain} jar held {}", names_words(names)));
        }
    }
    why
}

fn check_words(host: &str, check: &Check) -> String {
    match check {
        Check::Empty => format!("{host} was open but held no session yet"),
        Check::NotString(kind) => {
            let article = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            format!("the session check on {host} returned {article} {kind}, not a string")
        }
        Check::Threw => format!("the session check on {host} threw an exception"),
        Check::LeftHost => {
            format!("the page on {host} navigated away during the session check")
        }
        Check::Unreadable => {
            format!("the session check on {host} returned an unreadable answer")
        }
        Check::NoAnswer(e) => format!("the session check on {host} got no answer ({e})"),
    }
}

/// Cookie names for a message: the first [`NAMES_SHOWN`], then a count.
fn names_words(names: &[String]) -> String {
    let shown: Vec<&str> = names
        .iter()
        .take(NAMES_SHOWN)
        .map(|n| {
            if n.is_empty() {
                "(unnamed)"
            } else {
                n.as_str()
            }
        })
        .collect();
    match names.len().checked_sub(NAMES_SHOWN) {
        Some(more) if more > 0 => format!("{} and {more} more", shown.join(", ")),
        _ => and_list(&shown),
    }
}

fn timed_out(
    login_hint: &str,
    timeout: Duration,
    waiting: &[Waiting<'_>],
    port_error: Option<&str>,
) -> BrowserError {
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
    BrowserError::TimedOut(format!(
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

#[cfg(feature = "test-util")]
mod test_util;
#[cfg(feature = "test-util")]
pub use test_util::CapturedBuilder;

#[cfg(test)]
mod tests;
