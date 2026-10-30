//! The wait against an in-memory fake browser that answers the CDP calls
//! the crate makes, so the whole poll (targets, attach, evaluate, cookies,
//! close) runs offline and deterministically. Every value here is an
//! obvious dummy.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;

const TOKEN: &str = r#"{"access_token":"dummy-access-token","refresh_token":"dummy-refresh"}"#;

#[derive(Clone)]
struct FakePage {
    id: &'static str,
    url: String,
    /// What the capture's expression answers once `ready_after` polls have
    /// happened (`null` before).
    value: Value,
    ready_after: usize,
    /// `location.hostname` as the page itself reports it, when it differs
    /// from the target URL's host.
    location: Option<&'static str>,
    throws: bool,
    hangs: bool,
    /// The requests the page makes, sent to a session that enabled
    /// `Network` in it once their poll has come.
    requests: Vec<FakeRequest>,
    /// After this many polls the page navigates to this URL.
    navigates: Option<(usize, &'static str)>,
}

fn page(id: &'static str, url: &str) -> FakePage {
    FakePage {
        id,
        url: url.into(),
        value: Value::Null,
        ready_after: 0,
        location: None,
        throws: false,
        hangs: false,
        requests: Vec::new(),
        navigates: None,
    }
}

#[derive(Clone)]
struct FakeRequest {
    /// Made once this many polls have happened.
    after: usize,
    id: &'static str,
    method: &'static str,
    url: &'static str,
    kind: &'static str,
    headers: Value,
    /// The requesting document's URL; the page's own when `None`.
    document: Option<&'static str>,
    /// The response, if one comes.
    response: Option<(u16, &'static str)>,
}

fn request(id: &'static str, method: &'static str, url: &'static str) -> FakeRequest {
    FakeRequest {
        after: 0,
        id,
        method,
        url,
        kind: "XHR",
        headers: json!({}),
        document: None,
        response: None,
    }
}

#[derive(Default)]
struct State {
    pages: Vec<FakePage>,
    cookies: Value,
    /// Cookies appear once this many polls have happened.
    cookies_after: usize,
    /// Every page disappears after this many polls (the owner closed it).
    pages_vanish_after: Option<usize>,
    polls: usize,
    sessions: HashMap<String, &'static str>,
    next_session: usize,
    /// (method, session, target) of every call, in order.
    calls: Vec<(String, Option<String>, Option<String>)>,
    expressions: Vec<String>,
    closed: Vec<String>,
    exited: bool,
    canceled: bool,
    connect_error: Option<String>,
    /// `Storage.getCookies` answers with a protocol error.
    cookies_fail: bool,
    /// Attaching to this target raises the cancel flag first, as a Ctrl-C
    /// landing mid-poll would.
    cancel_on_attach: Option<&'static str>,
    /// Chrome quits once this many polls have happened, ending a wait at a
    /// known poll rather than a time.
    exits_after: Option<usize>,
    /// What `Browser.getVersion` reports; `None` answers with an error.
    user_agent: Option<&'static str>,
    connections: usize,
    /// (connection, session, target) of each `Network.enable`.
    enabled: Vec<(usize, String, &'static str)>,
    /// (session, request id) of each request event sent.
    emitted: Vec<(String, &'static str)>,
}

thread_local! {
    /// The CDP-level cancel flag for the current test's thread (tests run
    /// on their own threads, so this never leaks between them).
    static CANCELED_MID_POLL: Cell<bool> = const { Cell::new(false) };
}

fn canceled_mid_poll() -> bool {
    CANCELED_MID_POLL.with(Cell::get)
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<State>>);

struct FakeTransport {
    state: Fake,
    inbox: VecDeque<String>,
    conn: usize,
}

impl FakeTransport {
    /// The network events due on this connection's watched pages.
    fn network_events(&mut self) {
        let mut s = self.state.0.borrow_mut();
        let polls = s.polls;
        let mut due = Vec::new();
        for (conn, session, target) in &s.enabled {
            if *conn != self.conn {
                continue;
            }
            let Some(p) = s.pages.iter().find(|p| p.id == *target) else {
                continue;
            };
            for r in &p.requests {
                let sent = s
                    .emitted
                    .iter()
                    .any(|(es, id)| es == session && *id == r.id);
                if r.after <= polls && !sent {
                    due.push((session.clone(), p.url.clone(), r.clone()));
                }
            }
        }
        for (session, page_url, r) in due {
            s.emitted.push((session.clone(), r.id));
            let document = r.document.map(str::to_string).unwrap_or(page_url);
            self.inbox.push_back(
                json!({"method": "Network.requestWillBeSent", "sessionId": session, "params": {
                    "requestId": r.id, "documentURL": document, "type": r.kind,
                    "request": {"url": r.url, "method": r.method, "headers": r.headers}}})
                .to_string(),
            );
            // Traffic the watch does not act on.
            self.inbox.push_back(
                json!({"method": "Network.loadingFinished", "sessionId": session,
                       "params": {"requestId": r.id}})
                .to_string(),
            );
            if let Some((status, mime)) = r.response {
                self.inbox.push_back(
                    json!({"method": "Network.responseReceived", "sessionId": session, "params": {
                        "requestId": r.id, "response": {"status": status, "mimeType": mime}}})
                    .to_string(),
                );
            }
        }
    }
}

impl Transport for FakeTransport {
    fn send(&mut self, text: &str) -> Result<(), String> {
        let req: Value = serde_json::from_str(text).unwrap();
        let id = req["id"].as_u64().unwrap();
        let method = req["method"].as_str().unwrap().to_string();
        let session = req["sessionId"].as_str().map(str::to_string);
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let mut s = self.state.0.borrow_mut();
        // The target a call is about: named, or the one its session (the
        // call's own, or the one it detaches) is attached to.
        let target = params["targetId"].as_str().map(str::to_string).or_else(|| {
            session
                .as_deref()
                .or(params["sessionId"].as_str())
                .and_then(|x| s.sessions.get(x))
                .map(|t| t.to_string())
        });
        s.calls
            .push((method.clone(), session.clone(), target.clone()));
        // An unsolicited event ahead of every reply, which the client skips.
        self.inbox
            .push_back(r#"{"method":"Target.targetInfoChanged","params":{}}"#.into());
        let reply = match method.as_str() {
            "Target.getTargets" => {
                s.polls += 1;
                if s.pages_vanish_after.is_some_and(|n| s.polls > n) {
                    s.pages.clear();
                }
                let polls = s.polls;
                for p in &mut s.pages {
                    if let Some((_, url)) = p.navigates.filter(|(after, _)| polls > *after) {
                        p.url = url.to_string();
                    }
                }
                let mut infos: Vec<Value> = s
                    .pages
                    .iter()
                    .map(|p| json!({"targetId": p.id, "type": "page", "url": p.url, "attached": false}))
                    .collect();
                // Non-page targets on the same hosts are never read.
                for p in &s.pages {
                    infos.push(json!({"targetId": format!("sw-{}", p.id), "type": "service_worker", "url": p.url}));
                    infos.push(
                        json!({"targetId": format!("if-{}", p.id), "type": "iframe", "url": p.url}),
                    );
                }
                Ok(json!({ "targetInfos": infos }))
            }
            "Target.attachToTarget" => {
                let target = target.unwrap();
                if s.cancel_on_attach == Some(target.as_str()) {
                    CANCELED_MID_POLL.with(|c| c.set(true));
                }
                match s.pages.iter().find(|p| p.id == target) {
                    Some(p) => {
                        let id = p.id;
                        s.next_session += 1;
                        let sid = format!("S{}", s.next_session);
                        s.sessions.insert(sid.clone(), id);
                        assert_eq!(params["flatten"], true);
                        Ok(json!({ "sessionId": sid }))
                    }
                    None => Err((-32602, "No target with given id found")),
                }
            }
            "Runtime.evaluate" => {
                let target = target.expect("evaluate runs in an attached session");
                let p = s.pages.iter().find(|p| p.id == target).unwrap().clone();
                let expression = params["expression"].as_str().unwrap().to_string();
                assert!(expression.starts_with("(async () => [location.hostname, await ("));
                assert_eq!(params["returnByValue"], true);
                assert_eq!(params["awaitPromise"], true);
                assert_eq!(params["silent"], true);
                s.expressions.push(expression);
                if p.hangs {
                    return Ok(());
                }
                if p.throws {
                    Ok(json!({"result": {"type": "object", "subtype": "error"},
                              "exceptionDetails": {"exceptionId": 1, "text": "Uncaught"}}))
                } else {
                    let host = p
                        .location
                        .map(str::to_string)
                        .unwrap_or_else(|| parse_url(&p.url).unwrap().host);
                    let value = if s.polls > p.ready_after {
                        p.value.clone()
                    } else {
                        Value::Null
                    };
                    Ok(
                        json!({"result": {"type": "object", "subtype": "array", "value": [host, value]}}),
                    )
                }
            }
            "Target.detachFromTarget" => {
                let session = params["sessionId"].as_str().unwrap().to_string();
                s.sessions.remove(&session);
                s.enabled.retain(|(_, es, _)| *es != session);
                Ok(json!({}))
            }
            "Network.enable" => {
                let session = session.expect("Network.enable runs in an attached session");
                let target = s.sessions[&session];
                assert_eq!(params, json!({"maxPostDataSize": 0}));
                let conn = self.conn;
                s.enabled.push((conn, session, target));
                Ok(json!({}))
            }
            "Browser.getVersion" => match s.user_agent {
                Some(ua) => Ok(json!({"product": "Chrome/0", "userAgent": ua})),
                None => Err((-32000, "Browser unavailable")),
            },
            "Storage.getCookies" if s.cookies_fail => Err((-32000, "Storage unavailable")),
            "Storage.getCookies" => {
                let cookies = if s.polls > s.cookies_after {
                    s.cookies.clone()
                } else {
                    json!([])
                };
                Ok(json!({ "cookies": cookies }))
            }
            "Target.closeTarget" => {
                let target = target.unwrap();
                s.pages.retain(|p| p.id != target);
                s.enabled.retain(|(_, _, t)| *t != target);
                s.closed.push(target);
                Ok(json!({ "success": true }))
            }
            _ => Err((-32601, "method not found")),
        };
        let msg = match reply {
            Ok(result) => json!({ "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "id": id, "error": {"code": code, "message": message} })
            }
        };
        self.inbox.push_back(msg.to_string());
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<String>, String> {
        if self.inbox.is_empty() {
            self.network_events();
        }
        Ok(self.inbox.pop_front())
    }
}

impl Driver for Fake {
    type T = FakeTransport;
    fn connect(&mut self) -> Result<FakeTransport, String> {
        let mut s = self.0.borrow_mut();
        if let Some(e) = &s.connect_error {
            return Err(e.clone());
        }
        s.connections += 1;
        Ok(FakeTransport {
            state: self.clone(),
            inbox: VecDeque::new(),
            conn: s.connections,
        })
    }
    fn exited(&mut self) -> bool {
        let s = self.0.borrow();
        s.exited || s.exits_after.is_some_and(|n| s.polls >= n)
    }
    fn canceled(&self) -> bool {
        self.0.borrow().canceled
    }
    fn cancel_check(&self) -> fn() -> bool {
        canceled_mid_poll
    }
}

fn fake_with(pages: Vec<FakePage>) -> Fake {
    let fake = Fake::default();
    fake.0.borrow_mut().pages = pages;
    fake
}

fn settings(timeout: Duration) -> WaitSettings {
    WaitSettings {
        timeout,
        poll_interval: Duration::ZERO,
        call_timeout: Duration::from_millis(50),
        login_hint: "ex auth login --browser".into(),
    }
}

struct Outcome {
    result: Result<Vec<Captured>, BrowserError>,
    notes: Vec<String>,
}

fn run(fake: &mut Fake, captures: &[Capture], timeout: Duration) -> Outcome {
    let mut notes = Vec::new();
    let mut got = Vec::new();
    let result = run_wait(
        fake,
        captures,
        &settings(timeout),
        &mut |line| notes.push(line.to_string()),
        &mut |captured| {
            got.push(captured);
            Ok(())
        },
    );
    Outcome {
        result: result.map(|()| got),
        notes,
    }
}

const LONG: Duration = Duration::from_secs(30);

fn app_capture() -> Capture {
    Capture::new("Example", HostRule::exact("app.example.com").unwrap())
        .expression("localStorage.getItem('session')")
}

fn calls_named(fake: &Fake, method: &str) -> Vec<(Option<String>, Option<String>)> {
    fake.0
        .borrow()
        .calls
        .iter()
        .filter(|(m, _, _)| m == method)
        .map(|(_, s, t)| (s.clone(), t.clone()))
        .collect()
}

#[test]
fn a_session_is_read_only_from_the_matching_page_and_its_tab_closed() {
    let mut target = page("T-app", "https://app.example.com/home");
    target.value = json!(TOKEN);
    target.ready_after = 2;
    let mut fake = fake_with(vec![
        page("T-login", "https://login.example.com/u/login"),
        page("T-confusable", "https://app.example.com.evil.example/"),
        page("T-downgrade", "http://app.example.com/"),
        target,
    ]);
    let out = run(&mut fake, &[app_capture()], LONG);
    let captured = out.result.unwrap();
    assert_eq!(captured.len(), 1);
    let captured = &captured[0];
    assert_eq!(captured.label(), "Example");
    assert_eq!(captured.host(), "app.example.com");
    assert_eq!(captured.value().unwrap().expose(), TOKEN);
    assert!(captured.cookie("anything").is_none());

    // Only the app page was ever attached to; it was checked once per poll
    // until ready, then closed; nothing enabled a domain the page can see.
    let attached: Vec<_> = calls_named(&fake, "Target.attachToTarget")
        .into_iter()
        .map(|(_, t)| t.unwrap())
        .collect();
    assert_eq!(attached, ["T-app", "T-app", "T-app"]);
    assert_eq!(calls_named(&fake, "Runtime.evaluate").len(), 3);
    assert_eq!(calls_named(&fake, "Target.detachFromTarget").len(), 3);
    assert_eq!(fake.0.borrow().closed, ["T-app"]);
    let methods: Vec<String> = fake.0.borrow().calls.iter().map(|c| c.0.clone()).collect();
    assert!(
        !methods.iter().any(|m| m.ends_with(".enable")),
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|m| m == "Storage.getCookies"),
        "no cookies named"
    );
    // The expression ran inside the host-reporting wrapper.
    assert!(fake.0.borrow().expressions[0].contains("localStorage.getItem('session')"));

    // The status lines, and no value in them or in Debug output.
    assert_eq!(
        out.notes,
        [
            "A Chrome window opened at the Example sign-in. Sign in there; this waits for the \
             session and captures it (up to 30 seconds; Ctrl-C cancels).",
            "Example: session captured, tab closed.",
        ]
    );
    let debug = format!("{captured:?}");
    assert!(!debug.contains("dummy-access-token"), "{debug}");
    assert!(debug.contains("redacted"), "{debug}");
}

#[test]
fn every_tab_on_the_hosts_is_closed_so_none_can_renew_the_token() {
    let mut a = page("T-a", "https://app.example.com/a");
    a.value = json!(TOKEN);
    let b = page("T-b", "https://app.example.com/b");
    let mut fake = fake_with(vec![a, b, page("T-other", "https://www.example.org/")]);
    let out = run(&mut fake, &[app_capture()], LONG);
    out.result.unwrap();
    assert_eq!(fake.0.borrow().closed, ["T-a", "T-b"]);
    assert_eq!(out.notes[1], "Example: session captured, 2 tabs closed.");
}

#[test]
fn answers_that_are_not_a_session_keep_the_wait_going() {
    let cases: &[(Value, &str)] = &[
        (
            Value::Null,
            "app.example.com was open but held no session yet",
        ),
        (
            json!(""),
            "app.example.com was open but held no session yet",
        ),
        (
            json!("  \n"),
            "app.example.com was open but held no session yet",
        ),
        (json!(42), "returned a number, not a string"),
        (json!(true), "returned a boolean, not a string"),
        (
            json!({"access_token": "x"}),
            "returned an object, not a string",
        ),
        (json!(["x"]), "returned an object, not a string"),
    ];
    for (value, why) in cases {
        let mut p = page("T-app", "https://app.example.com/");
        p.value = value.clone();
        let mut fake = fake_with(vec![p]);
        let err = run(&mut fake, &[app_capture()], Duration::ZERO)
            .result
            .unwrap_err();
        assert_eq!(err.exit_code(), 3, "{value}");
        let msg = err.to_string();
        assert!(msg.contains(why), "{value}: {msg}");
        assert!(fake.0.borrow().closed.is_empty(), "nothing closed: {value}");
    }
}

#[test]
fn a_throwing_check_a_page_that_left_its_host_and_a_hung_check_are_named() {
    let mut throws = page("T-app", "https://app.example.com/");
    throws.throws = true;
    let mut left = page("T-app", "https://app.example.com/");
    left.value = json!(TOKEN);
    left.location = Some("evil.example");
    let mut hangs = page("T-app", "https://app.example.com/");
    hangs.hangs = true;
    for (p, why) in [
        (throws, "the session check on app.example.com threw an exception"),
        (left, "the page on app.example.com navigated away during the session check"),
        (hangs, "the session check on app.example.com got no answer (Runtime.evaluate got no answer in time)"),
    ] {
        let mut fake = fake_with(vec![p]);
        let msg = run(&mut fake, &[app_capture()], Duration::ZERO)
            .result
            .unwrap_err()
            .to_string();
        assert!(msg.contains(why), "{msg}");
        assert!(!msg.contains("dummy"), "{msg}");
        assert!(fake.0.borrow().closed.is_empty());
    }
}

#[test]
fn a_timeout_names_the_hosts_no_page_was_open_on_and_the_command_to_rerun() {
    let mut fake = fake_with(vec![page("T-login", "https://login.example.com/")]);
    let capture = Capture::new("Example", HostRule::exact("app.example.com").unwrap())
        .host(HostRule::domain("example.net").unwrap())
        .expression("1");
    let err = run(&mut fake, &[capture], Duration::from_secs(0))
        .result
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert_eq!(
        err.to_string(),
        "timed out after 0 seconds: the Example sign-in was not completed in the browser \
         window (no page on app.example.com or example.net (or a subdomain) was open). Re-run \
         `ex auth login --browser` and finish signing in."
    );
    assert!(calls_named(&fake, "Target.attachToTarget").is_empty());
}

#[test]
fn cookies_are_read_from_the_browser_jar_httponly_included() {
    let mut p = page("T-app", "https://example.com/account");
    p.value = json!(TOKEN);
    let mut fake = fake_with(vec![p]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "device_id", "value": "dummy-device", "domain": ".example.com", "path": "/",
         "httpOnly": true, "secure": true},
        {"name": "device_id", "value": "dummy-foreign", "domain": ".example.org", "path": "/"},
        {"name": "unrelated", "value": "x", "domain": "example.com", "path": "/"}
    ]);
    let capture = Capture::new("Example", HostRule::domain("example.com").unwrap())
        .expression("1")
        .optional_cookie("https://example.com/", "device_id")
        .optional_cookie("https://example.com/", "absent");
    let captured = run(&mut fake, &[capture], LONG).result.unwrap();
    let captured = &captured[0];
    assert_eq!(captured.value().unwrap().expose(), TOKEN);
    assert_eq!(
        captured.cookie("device_id").unwrap().expose(),
        "dummy-device"
    );
    assert!(captured.cookie("absent").is_none(), "optional and unset");
    assert!(captured.cookie("unrelated").is_none(), "only named cookies");
    assert!(!format!("{captured:?}").contains("dummy-device"));
}

#[test]
fn a_required_cookie_holds_the_capture_until_it_is_set() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "SESSION", "value": "dummy-session", "domain": "www.example.com", "path": "/"}
    ]);
    fake.0.borrow_mut().cookies_after = 2;
    let capture = Capture::new("Example", HostRule::domain("example.com").unwrap())
        .cookie("https://www.example.com/", "SESSION");

    // Not set within the time: the timeout names the cookie, never a value.
    let msg = run(&mut fake, std::slice::from_ref(&capture), Duration::ZERO)
        .result
        .unwrap_err()
        .to_string();
    assert!(msg.contains("the SESSION cookie was not set"), "{msg}");
    assert!(fake.0.borrow().closed.is_empty());

    // Set on a later poll: captured, with no expression, and the tab closed.
    let captured = run(&mut fake, &[capture], LONG).result.unwrap();
    assert!(captured[0].value().is_none());
    assert_eq!(
        captured[0].cookie("SESSION").unwrap().expose(),
        "dummy-session"
    );
    assert_eq!(captured[0].host(), "www.example.com");
    assert_eq!(fake.0.borrow().closed, ["T-app"]);
    assert!(
        calls_named(&fake, "Target.attachToTarget").is_empty(),
        "cookie-only: no page attach"
    );
}

#[test]
fn several_captures_are_handed_over_as_each_is_ready() {
    let mut a = page("T-a", "https://app.example.com/");
    a.value = json!("dummy-a");
    a.ready_after = 3;
    let mut b = page("T-b", "https://portal.example.org/");
    b.value = json!("dummy-b");
    b.ready_after = 1;
    let mut fake = fake_with(vec![a, b]);
    let captures = [
        app_capture().hint("password and code"),
        Capture::new("Other", HostRule::exact("portal.example.org").unwrap()).expression("2"),
    ];
    let out = run(&mut fake, &captures, LONG);
    let got = out.result.unwrap();
    let order: Vec<&str> = got.iter().map(Captured::label).collect();
    assert_eq!(order, ["Other", "Example"]);
    assert_eq!(got[0].value().unwrap().expose(), "dummy-b");
    assert_eq!(got[1].value().unwrap().expose(), "dummy-a");
    assert_eq!(fake.0.borrow().closed, ["T-b", "T-a"]);
    assert_eq!(
        out.notes,
        [
            "A Chrome window opened with Example and Other. Sign in to each there (Example: \
             password and code); this waits for the sessions and captures each as it is ready \
             (up to 30 seconds; Ctrl-C cancels).",
            "Other: session captured, tab closed.",
            "Example: session captured, tab closed.",
        ]
    );
}

#[test]
fn a_timeout_with_several_captures_names_only_the_pending_ones() {
    let mut b = page("T-b", "https://portal.example.org/");
    b.value = json!("dummy-b");
    let mut fake = fake_with(vec![b]);
    let captures = [
        app_capture(),
        Capture::new("Other", HostRule::exact("portal.example.org").unwrap()).expression("2"),
    ];
    let mut handed = Vec::new();
    let err = run_wait(
        &mut fake,
        &captures,
        &settings(Duration::ZERO),
        &mut |_| {},
        &mut |c| {
            handed.push(c.label().to_string());
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(handed, ["Other"], "the ready one was handed over first");
    let msg = err.to_string();
    assert!(
        msg.contains("the Example sign-in was not completed"),
        "{msg}"
    );
    assert!(!msg.contains("Other"), "{msg}");
}

#[test]
fn an_error_from_the_handler_ends_the_wait() {
    let mut p = page("T-app", "https://app.example.com/");
    p.value = json!(TOKEN);
    let mut fake = fake_with(vec![p]);
    let err = run_wait(
        &mut fake,
        &[app_capture()],
        &settings(LONG),
        &mut |_| {},
        &mut |_| Err(CliError::Upstream("import failed".into())),
    )
    .unwrap_err();
    assert!(
        matches!(&err, BrowserError::Handler(CliError::Upstream(_))),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), 5);
}

#[test]
fn a_closed_window_ends_the_wait() {
    // Chrome quit.
    let mut fake = fake_with(vec![page("T-app", "https://app.example.com/")]);
    fake.0.borrow_mut().exited = true;
    let err = run(&mut fake, &[app_capture()], LONG).result.unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert_eq!(
        err.to_string(),
        "the Chrome window was closed before the Example sign-in finished. Re-run `ex auth \
         login --browser` and finish signing in."
    );

    // Every page closed while Chrome keeps running (macOS): two empty polls.
    let mut fake = fake_with(vec![page("T-app", "https://app.example.com/")]);
    fake.0.borrow_mut().pages_vanish_after = Some(1);
    let err = run(&mut fake, &[app_capture()], LONG).result.unwrap_err();
    assert!(err.to_string().contains("window was closed"), "{err}");
    assert_eq!(fake.0.borrow().polls, 3);
}

#[test]
fn a_window_with_no_pages_yet_is_not_taken_for_closed() {
    let mut fake = fake_with(vec![]);
    let err = run(&mut fake, &[app_capture()], Duration::from_millis(30))
        .result
        .unwrap_err();
    assert!(err.to_string().starts_with("timed out"), "{err}");
}

#[test]
fn a_cancel_ends_the_wait_as_a_canceled_login() {
    let mut fake = fake_with(vec![page("T-app", "https://app.example.com/")]);
    fake.0.borrow_mut().canceled = true;
    let err = run(&mut fake, &[app_capture()], LONG).result.unwrap_err();
    assert!(matches!(err, BrowserError::Canceled), "{err:?}");
    assert_eq!(err.to_string(), "browser login canceled");
    assert_eq!(err.exit_code(), 1);
    assert!(fake.0.borrow().calls.is_empty());
}

#[test]
fn an_unreachable_debug_port_is_retried_and_named_at_the_timeout() {
    let mut fake = fake_with(vec![]);
    fake.0.borrow_mut().connect_error = Some("connecting to 127.0.0.1:9: refused".into());
    let msg = run(&mut fake, &[app_capture()], Duration::from_millis(30))
        .result
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("Chrome's debug port: connecting to 127.0.0.1:9: refused"),
        "{msg}"
    );
}

#[test]
fn an_invalid_capture_is_refused_before_any_call() {
    let mut fake = fake_with(vec![]);
    let err = run(
        &mut fake,
        &[Capture::new("X", HostRule::exact("a.example").unwrap())],
        LONG,
    )
    .result
    .unwrap_err();
    assert!(err.to_string().contains("names nothing to read"), "{err}");
    assert!(fake.0.borrow().calls.is_empty());
}

#[test]
fn captures_are_validated() {
    let host = || HostRule::exact("app.example.com").unwrap();
    let domain = || HostRule::domain("example.com").unwrap();
    let cases: Vec<(Capture, Option<&str>)> = vec![
        (Capture::new("X", host()).expression("1"), None),
        (
            Capture::new("X", host()).cookie("https://app.example.com/", "sid"),
            None,
        ),
        (
            Capture::new("X", domain()).cookie("https://www.example.com/a", "sid"),
            None,
        ),
        (
            Capture::new(" ", host()).expression("1"),
            Some("the label is empty"),
        ),
        (Capture::new("X", host()), Some("names nothing to read")),
        (
            Capture::new("X", host()).expression("  "),
            Some("empty expression"),
        ),
        (
            Capture::new("X", host()).cookie("https://other.example.com/", "sid"),
            Some("not an https URL on its hosts"),
        ),
        (
            Capture::new("X", host()).cookie("http://app.example.com/", "sid"),
            Some("not an https URL on its hosts"),
        ),
        (
            Capture::new("X", host()).cookie("https://app.example.com:8443/", "sid"),
            Some("not an https URL on its hosts"),
        ),
        (
            Capture::new("X", host()).cookie("https://app.example.com.evil.example/", "sid"),
            Some("not an https URL on its hosts"),
        ),
        (
            Capture::new("X", host()).cookie("https://app.example.com/", "a=b"),
            Some("invalid cookie name"),
        ),
        (
            Capture::new("X", host()).cookie("https://app.example.com/", "a b"),
            Some("invalid cookie name"),
        ),
        (
            Capture::new("X", host()).cookie("https://app.example.com/", ""),
            Some("invalid cookie name"),
        ),
        (
            Capture::new("X", host())
                .cookie("https://app.example.com/", "sid")
                .optional_cookie("https://app.example.com/x", "sid"),
            Some("reads the cookie `sid` twice"),
        ),
    ];
    for (capture, want) in cases {
        let got = capture.validate();
        match want {
            None => assert!(got.is_ok(), "{capture:?}: {got:?}"),
            Some(why) => {
                let msg = got.unwrap_err().to_string();
                assert!(msg.contains(why), "{capture:?}: {msg}");
            }
        }
    }
}

#[test]
fn captures_in_one_window_must_be_distinct() {
    let a = || Capture::new("A", HostRule::domain("example.com").unwrap()).expression("1");
    let b = |rule| Capture::new("B", rule).expression("1");
    assert!(validate_set(&[a(), b(HostRule::exact("example.org").unwrap())]).is_ok());
    let err = validate_set(&[a(), b(HostRule::exact("app.example.com").unwrap())]).unwrap_err();
    assert!(err.to_string().contains("overlapping hosts"), "{err}");
    let err = validate_set(&[
        a(),
        Capture::new("a", HostRule::exact("example.org").unwrap()).expression("1"),
    ])
    .unwrap_err();
    assert!(
        err.to_string().contains("two captures are labelled"),
        "{err}"
    );
    assert!(validate_set(&[]).is_err());
}

#[test]
fn evaluation_results_are_read_strictly() {
    let hosts = [HostRule::domain("example.com").unwrap()];
    let ok = |host: &str, v: Value| json!({"result": {"type": "object", "value": [host, v]}});
    let ready = |r: &Value| match read_evaluation(r, &hosts) {
        Evaluation::Ready { host, value } => Ok((host, value.expose().to_string())),
        Evaluation::NotReady(check) => Err(check),
    };
    assert_eq!(
        ready(&ok("App.Example.com", json!("v"))),
        Ok(("app.example.com".into(), "v".into()))
    );
    // A value with surrounding space is returned exactly as the page gave it.
    assert_eq!(ready(&ok("example.com", json!(" v "))).unwrap().1, " v ");
    let cases: &[(Value, Check)] = &[
        (ok("example.com", Value::Null), Check::Empty),
        (ok("example.com", json!("")), Check::Empty),
        (ok("example.com", json!("\t")), Check::Empty),
        (ok("example.com", json!(0)), Check::NotString("number")),
        (ok("example.com", json!(false)), Check::NotString("boolean")),
        (ok("example.com", json!({})), Check::NotString("object")),
        (ok("example.com", json!([])), Check::NotString("object")),
        (ok("example.com.evil.example", json!("v")), Check::LeftHost),
        (ok("", json!("v")), Check::LeftHost),
        (
            json!({"result": {"type": "object", "value": [7, "v"]}}),
            Check::LeftHost,
        ),
        (
            json!({"result": {"type": "string", "value": "v"}, "exceptionDetails": {}}),
            Check::Threw,
        ),
        (
            json!({"result": {"type": "string", "value": "v"}}),
            Check::Unreadable,
        ),
        (
            json!({"result": {"type": "object", "value": ["example.com"]}}),
            Check::Unreadable,
        ),
        (
            json!({"result": {"type": "object", "value": ["example.com", "v", 3]}}),
            Check::Unreadable,
        ),
        (json!({"result": {"type": "undefined"}}), Check::Unreadable),
        (json!({}), Check::Unreadable),
    ];
    for (result, want) in cases {
        assert_eq!(ready(result).unwrap_err(), *want, "{result}");
    }
}

#[test]
fn the_expression_is_wrapped_to_report_its_own_host() {
    assert_eq!(
        wrap_expression("localStorage['k'] // trailing comment"),
        "(async () => [location.hostname, await (localStorage['k'] // trailing comment\n)])()"
    );
}

#[test]
fn page_targets_are_pages_only() {
    let result = json!({"targetInfos": [
        {"targetId": "P1", "type": "page", "url": "https://a.example/"},
        {"targetId": "W1", "type": "service_worker", "url": "https://a.example/sw.js"},
        {"targetId": "F1", "type": "iframe", "url": "https://a.example/frame"},
        {"targetId": "B1", "type": "background_page", "url": "chrome-extension://x/"},
        {"targetId": "X1", "type": "browser", "url": ""},
        {"type": "page", "url": "https://no-id.example/"},
        {"targetId": "P2", "type": "page"},
        {"targetId": "P3", "type": "page", "url": "about:blank"}
    ]});
    assert_eq!(
        page_targets(&result),
        [
            PageTarget {
                id: "P1".into(),
                url: "https://a.example/".into()
            },
            PageTarget {
                id: "P3".into(),
                url: "about:blank".into()
            },
        ]
    );
    assert!(page_targets(&json!({})).is_empty());
    assert!(page_targets(&json!({"targetInfos": "x"})).is_empty());
}

#[test]
fn cookies_are_chosen_as_the_browser_would_send_them() {
    let url = |u: &str| parse_url(u).unwrap();
    let jar = json!([
        {"name": "host_only", "value": "h", "domain": "app.example.com", "path": "/"},
        {"name": "domain", "value": "d", "domain": ".example.com", "path": "/"},
        {"name": "scoped", "value": "root", "domain": ".example.com", "path": "/"},
        {"name": "scoped", "value": "api", "domain": ".example.com", "path": "/api"},
        {"name": "tie", "value": "domain-wide", "domain": ".example.com", "path": "/"},
        {"name": "tie", "value": "host-only", "domain": "app.example.com", "path": "/"},
        {"name": "empty", "value": "", "domain": ".example.com", "path": "/"},
        {"name": "confusable", "value": "c", "domain": ".example.com.evil.example", "path": "/"},
        {"name": "suffix", "value": "s", "domain": ".ample.com", "path": "/"},
        {"name": "part_own", "value": "p1", "domain": ".example.com", "path": "/",
         "partitionKey": {"topLevelSite": "https://example.com", "hasCrossSiteAncestor": false}},
        {"name": "part_cross", "value": "p2", "domain": ".example.com", "path": "/",
         "partitionKey": {"topLevelSite": "https://example.com", "hasCrossSiteAncestor": true}},
        {"name": "part_other", "value": "p3", "domain": ".example.com", "path": "/",
         "partitionKey": "https://other.example"},
        {"name": "part_null", "value": "p4", "domain": ".example.com", "path": "/",
         "partitionKey": null},
        {"name": "no_path", "value": "np", "domain": ".example.com"},
        {"name": "bad_domain", "value": "b", "domain": "exa mple.com", "path": "/"}
    ]);
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("https://app.example.com/", "host_only", Some("h")),
        ("https://www.example.com/", "host_only", None),
        ("https://sub.app.example.com/", "host_only", None),
        ("https://example.com/", "domain", Some("d")),
        ("https://deep.app.example.com/x", "domain", Some("d")),
        ("https://example.org/", "domain", None),
        ("https://notexample.com/", "domain", None),
        // The longest matching path wins; a path only matches on a boundary.
        ("https://app.example.com/api/v1", "scoped", Some("api")),
        ("https://app.example.com/api", "scoped", Some("api")),
        ("https://app.example.com/apiary", "scoped", Some("root")),
        ("https://app.example.com/", "scoped", Some("root")),
        // Same path: the host-only cookie is the more specific.
        ("https://app.example.com/", "tie", Some("host-only")),
        ("https://www.example.com/", "tie", Some("domain-wide")),
        ("https://app.example.com/", "empty", None),
        ("https://app.example.com/", "confusable", None),
        ("https://app.example.com/", "suffix", None),
        ("https://app.example.com/", "part_own", Some("p1")),
        ("https://app.example.com/", "part_cross", None),
        ("https://app.example.com/", "part_other", None),
        ("https://app.example.com/", "part_null", Some("p4")),
        ("https://app.example.com/x", "no_path", Some("np")),
        ("https://app.example.com/", "bad_domain", None),
        ("https://app.example.com/", "missing", None),
    ];
    for (u, name, want) in cases {
        assert_eq!(
            pick_cookie(&jar, &url(u), name).as_deref(),
            *want,
            "{u} {name}"
        );
    }
    // A Secure cookie is never chosen for a plain-http URL.
    let secure =
        json!([{"name": "s", "value": "v", "domain": "a.example", "path": "/", "secure": true}]);
    assert_eq!(pick_cookie(&secure, &url("http://a.example/"), "s"), None);
    assert_eq!(
        pick_cookie(&secure, &url("https://a.example/"), "s").as_deref(),
        Some("v")
    );
    assert_eq!(
        pick_cookie(&Value::Null, &url("https://a.example/"), "s"),
        None
    );
}

#[test]
fn path_matching_follows_rfc_6265() {
    let cases: &[(&str, &str, bool)] = &[
        ("/", "/", true),
        ("/a", "/", true),
        ("/a/b", "/a", true),
        ("/a/b", "/a/", true),
        ("/a", "/a", true),
        ("/ab", "/a", false),
        ("/", "/a", false),
        ("/b", "/a", false),
    ];
    for (request, cookie, want) in cases {
        assert_eq!(path_matches(request, cookie), *want, "{request} {cookie}");
    }
}

#[test]
fn status_lines_and_durations_read_naturally() {
    assert_eq!(duration_words(Duration::from_secs(600)), "10 minutes");
    assert_eq!(duration_words(Duration::from_secs(60)), "1 minute");
    assert_eq!(duration_words(Duration::from_secs(90)), "90 seconds");
    assert_eq!(duration_words(Duration::from_secs(1)), "1 second");
    assert_eq!(duration_words(Duration::ZERO), "0 seconds");
    assert_eq!(
        duration_words(Duration::from_millis(250)),
        "250 milliseconds"
    );
    assert_eq!(duration_words(Duration::from_millis(1)), "1 millisecond");
    assert_eq!(duration_words(Duration::from_millis(1500)), "1 second");
    assert_eq!(and_list(&["A"]), "A");
    assert_eq!(and_list(&["A", "B"]), "A and B");
    assert_eq!(and_list(&["A", "B", "C"]), "A, B and C");
    assert_eq!(
        announce(
            &[app_capture().hint("password and the texted code")],
            DEFAULT_TIMEOUT
        ),
        "A Chrome window opened at the Example sign-in. Sign in there (password and the texted \
         code); this waits for the session and captures it (up to 10 minutes; Ctrl-C cancels)."
    );
    let other = Capture::new("Other", HostRule::exact("b.example").unwrap()).expression("1");
    assert_eq!(
        announce(&[app_capture(), other], DEFAULT_TIMEOUT),
        "A Chrome window opened with Example and Other. Sign in to each there; this waits for \
         the sessions and captures each as it is ready (up to 10 minutes; Ctrl-C cancels)."
    );
    assert_eq!(
        captured_note("Example", &Ok(1)),
        "Example: session captured, tab closed."
    );
    assert_eq!(
        captured_note("Example", &Ok(3)),
        "Example: session captured, 3 tabs closed."
    );
    assert_eq!(
        captured_note(
            "Example",
            &Err("No target with given id found (-32602)".into())
        ),
        "Example: session captured; its tab could not be closed (No target with given id \
         found (-32602)), so it closes with the window."
    );
}

#[test]
fn diagnostics_cover_every_state_without_values() {
    let capture = app_capture();
    let progress = |saw_page, blocked| Progress {
        saw_page,
        blocked,
        jar: None,
    };
    let host = || "app.example.com".to_string();
    let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let cases: Vec<(Progress, &str)> = vec![
        (progress(false, None), "no page on app.example.com was open"),
        (
            progress(true, Some(Blocked::Cookies(names(&["a"])))),
            "the a cookie was not set",
        ),
        (
            progress(true, Some(Blocked::Cookies(names(&["a", "b"])))),
            "the a and b cookies were not set",
        ),
        (
            progress(true, Some(Blocked::Check(host(), Check::Unreadable))),
            "the session check on app.example.com returned an unreadable answer",
        ),
        (
            progress(true, Some(Blocked::EmptyJar("example.com".into()))),
            "no cookie for example.com was set",
        ),
        (
            progress(
                true,
                Some(Blocked::Request("app.example.com/bff/graphql".into())),
            ),
            "no request to app.example.com/bff/graphql was seen",
        ),
        (
            progress(true, Some(Blocked::Holding)),
            "the session was in place, but the wait ended while its tab was held open",
        ),
        (
            progress(
                true,
                Some(Blocked::Unconfirmed("GET /me answered 401".into())),
            ),
            "the session was read but not confirmed: GET /me answered 401",
        ),
        (
            progress(true, None),
            "a page was open, but the session was not ready",
        ),
    ];
    for (p, want) in cases {
        assert_eq!(diagnose(&capture, &p), want);
    }

    // The jar's names follow what held the capture back; an empty jar
    // says so on its own.
    let mut p = progress(true, Some(Blocked::Cookies(names(&["SESSION"]))));
    p.jar = Some(("example.com".into(), names(&["", "a", "b"])));
    assert_eq!(
        diagnose(&capture, &p),
        "the SESSION cookie was not set; the example.com jar held (unnamed), a and b"
    );
    p.blocked = Some(Blocked::EmptyJar("example.com".into()));
    p.jar = Some(("example.com".into(), Vec::new()));
    assert_eq!(diagnose(&capture, &p), "no cookie for example.com was set");
    let many: Vec<String> = (0..25).map(|i| format!("c{i:02}")).collect();
    let words = names_words(&many);
    assert!(words.starts_with("c00, c01, "), "{words}");
    assert!(words.ends_with("c19 and 5 more"), "{words}");
}

#[test]
fn a_failed_cookie_read_never_costs_another_capture_its_session() {
    // Example is read (and its tab closed) first in the poll; Other's
    // cookie read then fails. Example must still be handed over: its tab is
    // gone, so it could not be read again.
    let mut a = page("T-a", "https://app.example.com/");
    a.value = json!("dummy-a");
    let mut b = page("T-b", "https://portal.example.org/");
    b.value = json!("dummy-b");
    let mut fake = fake_with(vec![a, b]);
    fake.0.borrow_mut().cookies_fail = true;
    let captures = [
        app_capture(),
        Capture::new("Other", HostRule::exact("portal.example.org").unwrap())
            .expression("2")
            .cookie("https://portal.example.org/", "sid"),
    ];
    let mut handed = Vec::new();
    let err = run_wait(
        &mut fake,
        &captures,
        &settings(Duration::ZERO),
        &mut |_| {},
        &mut |c| {
            handed.push(c.label().to_string());
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(handed, ["Example"]);
    let msg = err.to_string();
    assert!(
        msg.contains(
            "the session check on portal.example.org got no answer (reading the cookies \
             failed: Storage unavailable (-32000))"
        ),
        "{msg}"
    );
    // Other's tab stays open for the next poll.
    assert_eq!(fake.0.borrow().closed, ["T-a"]);
}

#[test]
fn a_cancel_mid_poll_still_hands_over_what_was_read() {
    let mut a = page("T-a", "https://app.example.com/");
    a.value = json!("dummy-a");
    let mut b = page("T-b", "https://portal.example.org/");
    b.value = json!("dummy-b");
    let mut fake = fake_with(vec![a, b]);
    fake.0.borrow_mut().cancel_on_attach = Some("T-b");
    let captures = [
        app_capture(),
        Capture::new("Other", HostRule::exact("portal.example.org").unwrap()).expression("2"),
    ];
    let mut handed = Vec::new();
    let err = run_wait(
        &mut fake,
        &captures,
        &settings(LONG),
        &mut |_| {},
        &mut |c| {
            handed.push(c.label().to_string());
            Ok(())
        },
    )
    .unwrap_err();
    CANCELED_MID_POLL.with(|c| c.set(false));
    assert_eq!(handed, ["Example"]);
    assert_eq!(err.to_string(), "browser login canceled");
    assert_eq!(fake.0.borrow().closed, ["T-a"]);
}

#[test]
fn an_unbounded_timeout_waits_without_overflowing_the_clock() {
    let mut p = page("T-app", "https://app.example.com/");
    p.value = json!(TOKEN);
    p.ready_after = 1;
    let mut fake = fake_with(vec![p]);
    let out = run(&mut fake, &[app_capture()], Duration::MAX);
    assert_eq!(out.result.unwrap().len(), 1);
    assert!(
        out.notes[0].contains("(no time limit; Ctrl-C cancels)"),
        "{}",
        out.notes[0]
    );
}

// ---- The whole jar ----------------------------------------------------

/// A sign-in that spans a login host and an app host, with cookies on
/// both, a domain cookie, and look-alike hosts that must stay out.
fn split_jar() -> Value {
    json!([
        {"name": "auth0", "value": "dummy-auth0", "domain": "login.example.com", "path": "/"},
        {"name": "SESSION", "value": "dummy-session", "domain": "www.example.com", "path": "/",
         "httpOnly": true, "secure": true},
        {"name": "CSRF", "value": "dummy-csrf", "domain": ".example.com", "path": "/"},
        {"name": "scoped", "value": "dummy-scoped", "domain": "www.example.com", "path": "/api"},
        {"name": "foreign", "value": "x", "domain": ".example.org", "path": "/"},
        {"name": "glued", "value": "x", "domain": "notexample.com", "path": "/"},
        {"name": "suffixed", "value": "x", "domain": ".example.com.evil.example", "path": "/"}
    ])
}

fn jar_capture() -> Capture {
    Capture::new("Example", HostRule::exact("www.example.com").unwrap())
        .cookie("https://www.example.com/", "SESSION")
        .cookie_jar("example.com")
}

#[test]
fn the_whole_jar_is_read_across_hosts_and_handed_over_as_a_header() {
    let mut fake = fake_with(vec![
        page("T-login", "https://login.example.com/u/login"),
        page("T-app", "https://www.example.com/home"),
    ]);
    fake.0.borrow_mut().cookies = split_jar();
    let got = run(&mut fake, &[jar_capture()], LONG).result.unwrap();
    let c = &got[0];
    let names: Vec<(&str, &str, &str)> = c
        .jar()
        .iter()
        .map(|k| (k.name(), k.domain(), k.path()))
        .collect();
    assert_eq!(
        names,
        [
            ("auth0", "login.example.com", "/"),
            ("SESSION", "www.example.com", "/"),
            ("CSRF", "example.com", "/"),
            ("scoped", "www.example.com", "/api"),
        ]
    );
    assert_eq!(
        c.cookie_header().unwrap().expose(),
        "scoped=dummy-scoped; auth0=dummy-auth0; SESSION=dummy-session; CSRF=dummy-csrf"
    );
    assert_eq!(c.cookie("SESSION").unwrap().expose(), "dummy-session");
    // Only the app page is closed; the login host is never touched.
    assert_eq!(fake.0.borrow().closed, ["T-app"]);
    assert!(calls_named(&fake, "Target.attachToTarget").is_empty());
    assert_eq!(
        calls_named(&fake, "Storage.getCookies").len(),
        1,
        "one read"
    );
    let debug = format!("{c:?}");
    assert!(!debug.contains("dummy-"), "{debug}");
}

#[test]
fn an_empty_jar_holds_the_capture_and_the_timeout_says_so() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "foreign", "value": "x", "domain": ".example.org", "path": "/"}
    ]);
    let capture = Capture::new("Example", HostRule::exact("www.example.com").unwrap())
        .cookie_jar("example.com");
    let msg = run(&mut fake, std::slice::from_ref(&capture), Duration::ZERO)
        .result
        .unwrap_err()
        .to_string();
    assert!(msg.contains("(no cookie for example.com was set)"), "{msg}");
    assert!(fake.0.borrow().closed.is_empty());
    // A capture without a jar has none to show.
    let mut p = page("T-app", "https://app.example.com/");
    p.value = json!(TOKEN);
    let got = run(&mut fake_with(vec![p]), &[app_capture()], LONG)
        .result
        .unwrap();
    assert!(got[0].jar().is_empty());
    assert!(got[0].cookie_header().is_none());
    assert!(got[0].request().is_none());
    assert!(got[0].request_log().is_empty());
    assert!(got[0].user_agent().is_none());
}

#[test]
fn a_timeout_lists_the_jar_s_cookie_names_never_their_values() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "CSRF", "value": "dummy-csrf", "domain": ".example.com", "path": "/"},
        {"name": "auth0", "value": "dummy-auth0", "domain": "login.example.com", "path": "/"}
    ]);
    let msg = run(&mut fake, &[jar_capture()], Duration::ZERO)
        .result
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("(the SESSION cookie was not set; the example.com jar held CSRF and auth0)"),
        "{msg}"
    );
    assert!(!msg.contains("dummy"), "{msg}");
}

// ---- The confirm hook ---------------------------------------------------

/// A hook answering `NotYet` for its first `not_yet` calls, then `Ready`,
/// counting its calls.
fn counting_hook(not_yet: usize) -> (Arc<AtomicUsize>, impl Fn(&Captured) -> Confirm) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let hook = move |_: &Captured| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= not_yet {
            Confirm::NotYet(format!("GET /me answered 401 (call {n})"))
        } else {
            Confirm::Ready
        }
    };
    (calls, hook)
}

fn method_positions(fake: &Fake, method: &str) -> Vec<usize> {
    fake.0
        .borrow()
        .calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.0 == method)
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn not_yet_keeps_the_tab_open_and_asks_again_until_ready() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    let (calls, hook) = counting_hook(2);
    let out = run(&mut fake, &[jar_capture().confirm(hook)], LONG);
    let got = out.result.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "asked on each of three polls"
    );
    // The tab stayed open through both NotYets: it was closed once, after
    // the third poll's read.
    assert_eq!(fake.0.borrow().closed, ["T-app"]);
    let polls = method_positions(&fake, "Target.getTargets");
    let closes = method_positions(&fake, "Target.closeTarget");
    assert_eq!(polls.len(), 3);
    assert!(closes[0] > polls[2], "{polls:?} {closes:?}");
    // The jar was read afresh on every poll.
    assert_eq!(calls_named(&fake, "Storage.getCookies").len(), 3);
    assert_eq!(
        out.notes.len(),
        2,
        "no status line per NotYet: {:?}",
        out.notes
    );
}

#[test]
fn the_hook_is_shown_what_was_read_and_only_once_the_browser_side_is_ready() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    fake.0.borrow_mut().cookies_after = 2;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let capture = jar_capture().confirm(move |c| {
        log.lock().unwrap().push((
            c.label().to_string(),
            c.host().to_string(),
            c.cookie("SESSION").map(|s| s.expose().to_string()),
            c.cookie_header().map(|h| h.expose().len()),
        ));
        Confirm::Ready
    });
    run(&mut fake, &[capture], LONG).result.unwrap();
    // Two polls without the SESSION cookie never reached the hook.
    assert_eq!(fake.0.borrow().polls, 3);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "Example");
    assert_eq!(seen[0].1, "www.example.com");
    assert_eq!(seen[0].2.as_deref(), Some("dummy-session"));
    assert!(seen[0].3.unwrap() > 0);
}

#[test]
fn fail_ends_the_wait_with_the_hook_s_error_and_closes_nothing() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    let capture = jar_capture()
        .confirm(|_| Confirm::Fail(CliError::Upstream("validation answered 429".into())));
    let mut handed = 0;
    let err = run_wait(
        &mut fake,
        &[capture],
        &settings(LONG),
        &mut |_| {},
        &mut |_| {
            handed += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, BrowserError::Handler(CliError::Upstream(m)) if m == "validation answered 429"),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), 5);
    assert_eq!(handed, 0);
    assert!(fake.0.borrow().closed.is_empty());
    assert_eq!(fake.0.borrow().polls, 1);
}

#[test]
fn a_timeout_names_the_last_not_yet_reason() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    let (calls, hook) = counting_hook(usize::MAX);
    let err = run(
        &mut fake,
        &[jar_capture().confirm(hook)],
        Duration::from_millis(150),
    )
    .result
    .unwrap_err();
    let last = calls.load(Ordering::SeqCst);
    assert!(last >= 2, "asked more than once: {last}");
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "(the session was read but not confirmed: GET /me answered 401 (call {last}); the \
             example.com jar held CSRF, SESSION, auth0 and scoped)"
        )),
        "{msg}"
    );
    assert!(fake.0.borrow().closed.is_empty());
}

#[test]
fn a_failed_confirm_still_hands_over_the_captures_already_read() {
    let mut a = page("T-a", "https://app.example.com/");
    a.value = json!("dummy-a");
    let mut b = page("T-b", "https://portal.example.org/");
    b.value = json!("dummy-b");
    let mut fake = fake_with(vec![a, b]);
    let captures = [
        app_capture(),
        Capture::new("Other", HostRule::exact("portal.example.org").unwrap())
            .expression("2")
            .confirm(|_| Confirm::Fail(CliError::Auth("the account is locked".into()))),
    ];
    let mut handed = Vec::new();
    let err = run_wait(
        &mut fake,
        &captures,
        &settings(LONG),
        &mut |_| {},
        &mut |c| {
            handed.push(c.label().to_string());
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(handed, ["Example"]);
    assert_eq!(err.to_string(), "the account is locked");
    assert_eq!(err.exit_code(), 3);
    assert_eq!(fake.0.borrow().closed, ["T-a"]);
}

#[test]
fn each_capture_in_one_window_is_confirmed_on_its_own() {
    let mut a = page("T-a", "https://app.example.com/");
    a.value = json!("dummy-a");
    let mut b = page("T-b", "https://portal.example.org/");
    b.value = json!("dummy-b");
    let mut fake = fake_with(vec![a, b]);
    let (calls_a, hook_a) = counting_hook(2);
    let (calls_b, hook_b) = counting_hook(0);
    let captures = [
        app_capture().confirm(hook_a),
        Capture::new("Other", HostRule::exact("portal.example.org").unwrap())
            .expression("2")
            .confirm(hook_b),
    ];
    let got = run(&mut fake, &captures, LONG).result.unwrap();
    let order: Vec<&str> = got.iter().map(Captured::label).collect();
    assert_eq!(order, ["Other", "Example"]);
    assert_eq!(calls_a.load(Ordering::SeqCst), 3);
    assert_eq!(calls_b.load(Ordering::SeqCst), 1);
    assert_eq!(fake.0.borrow().closed, ["T-b", "T-a"]);
}

#[test]
fn a_capture_s_clones_share_its_hook() {
    let (calls, hook) = counting_hook(0);
    let capture = jar_capture().confirm(hook);
    let clone = capture.clone();
    assert!(format!("{clone:?}").contains("ConfirmHook(..)"));
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    run(&mut fake, &[clone], LONG).result.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(capture);
}

// ---- Watching requests --------------------------------------------------

const GRAPHQL: &str = "https://www.example.com/bff/graphql";

fn graphql_request(after: usize) -> FakeRequest {
    FakeRequest {
        after,
        kind: "Fetch",
        headers: json!({"X-CSRF-Token": "dummy-csrf", "content-type": "application/json"}),
        ..request(
            "R-graphql",
            "POST",
            "https://www.example.com/bff/graphql?op=Accounts",
        )
    }
}

#[test]
fn a_watched_request_is_read_only_from_the_capture_s_own_pages() {
    let mut login = page("T-login", "https://login.example.com/u/login");
    login.requests = vec![request(
        "R-login",
        "POST",
        "https://www.example.com/bff/graphql",
    )];
    let mut app = page("T-app", "https://www.example.com/home");
    app.requests = vec![
        request("R-other", "GET", "https://www.example.com/api/me"),
        graphql_request(2),
    ];
    let mut fake = fake_with(vec![login, app]);
    fake.0.borrow_mut().cookies = split_jar();
    let capture = jar_capture().request(GRAPHQL);
    let got = run(&mut fake, &[capture], LONG).result.unwrap();
    let r = got[0].request().expect("the request was seen");
    assert_eq!(r.method(), "POST");
    assert_eq!(r.url(), "https://www.example.com/bff/graphql");
    assert_eq!(r.header("x-csrf-token").unwrap().expose(), "dummy-csrf");
    let names: Vec<&str> = r.headers().map(|(n, _)| n).collect();
    assert_eq!(names, ["content-type", "x-csrf-token"]);
    assert!(!format!("{:?}", got[0]).contains("dummy-csrf"));

    // Network was enabled in the app page only, never on the login host,
    // and on its own connection: the polls' connections enable nothing.
    let enabled = fake.0.borrow().enabled.clone();
    assert!(enabled.is_empty(), "the closed page's session is gone");
    let enables = calls_named(&fake, "Network.enable");
    assert_eq!(enables.len(), 1, "{enables:?}");
    assert_eq!(enables[0].1.as_deref(), Some("T-app"));
    let attached: Vec<String> = calls_named(&fake, "Target.attachToTarget")
        .into_iter()
        .map(|(_, t)| t.unwrap())
        .collect();
    assert_eq!(attached, ["T-app"]);
    assert_eq!(fake.0.borrow().closed, ["T-app"]);
}

#[test]
fn a_required_request_holds_the_capture_until_it_is_seen() {
    let mut app = page("T-app", "https://www.example.com/home");
    app.requests = vec![graphql_request(4)];
    let mut fake = fake_with(vec![app]);
    fake.0.borrow_mut().cookies = split_jar();
    let capture = jar_capture().request(GRAPHQL);
    let msg = run(&mut fake, std::slice::from_ref(&capture), Duration::ZERO)
        .result
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("(no request to www.example.com/bff/graphql was seen; the example.com"),
        "{msg}"
    );
    assert!(fake.0.borrow().closed.is_empty());
    let got = run(&mut fake, &[capture], LONG).result.unwrap();
    assert!(got[0].request().is_some());
    assert!(fake.0.borrow().polls >= 5);
}

#[test]
fn an_optional_request_is_waited_for_up_to_its_window_then_skipped() {
    let mut app = page("T-app", "https://www.example.com/home");
    app.requests = vec![request("R-other", "GET", "https://www.example.com/api/me")];
    let mut fake = fake_with(vec![app]);
    fake.0.borrow_mut().cookies = split_jar();
    let capture = jar_capture().optional_request(GRAPHQL, Duration::from_millis(30));
    let out = run(&mut fake, &[capture], LONG);
    let got = out.result.unwrap();
    assert!(got[0].request().is_none());
    assert!(fake.0.borrow().polls > 1, "the tab was held open");
    assert_eq!(
        out.notes[1],
        "Example: keeping the tab open up to 30 milliseconds for the page to request \
         www.example.com/bff/graphql before capturing the session."
    );
    assert_eq!(out.notes[2], "Example: session captured, tab closed.");
}

#[test]
fn an_optional_request_seen_in_the_window_ends_it_at_once() {
    let mut app = page("T-app", "https://www.example.com/home");
    app.requests = vec![graphql_request(3)];
    let mut fake = fake_with(vec![app]);
    fake.0.borrow_mut().cookies = split_jar();
    let capture = jar_capture().optional_request(GRAPHQL, Duration::from_secs(60));
    let got = run(&mut fake, &[capture], LONG).result.unwrap();
    assert_eq!(
        got[0]
            .request()
            .unwrap()
            .header("x-csrf-token")
            .unwrap()
            .expose(),
        "dummy-csrf"
    );
    assert_eq!(fake.0.borrow().polls, 4, "read on the poll after it came");
}

#[test]
fn a_request_seen_before_the_rest_is_ready_counts() {
    let mut app = page("T-app", "https://www.example.com/home");
    app.requests = vec![graphql_request(0)];
    let mut fake = fake_with(vec![app]);
    fake.0.borrow_mut().cookies = split_jar();
    fake.0.borrow_mut().cookies_after = 3;
    let capture = jar_capture().optional_request(GRAPHQL, Duration::ZERO);
    let out = run(&mut fake, &[capture], LONG);
    assert!(out.result.unwrap()[0].request().is_some());
    assert_eq!(out.notes.len(), 2, "no window to hold: {:?}", out.notes);
}

#[test]
fn a_request_from_a_document_off_the_capture_s_hosts_is_not_read() {
    let mut app = page("T-app", "https://www.example.com/home");
    app.requests = vec![
        FakeRequest {
            document: Some("https://login.example.com/u/login"),
            ..graphql_request(0)
        },
        FakeRequest {
            id: "R-frame",
            document: Some("https://widgets.example.org/frame"),
            ..graphql_request(0)
        },
    ];
    let mut fake = fake_with(vec![app]);
    fake.0.borrow_mut().cookies = split_jar();
    let capture = jar_capture().optional_request(GRAPHQL, Duration::from_millis(20));
    let got = run(&mut fake, &[capture], LONG).result.unwrap();
    assert!(got[0].request().is_none());
}

#[test]
fn the_watch_lets_go_of_a_page_that_leaves_the_capture_s_hosts() {
    let mut app = page("T-app", "https://www.example.com/home");
    app.navigates = Some((1, "https://login.example.com/u/login"));
    let mut fake = fake_with(vec![app]);
    fake.0.borrow_mut().cookies = split_jar();
    fake.0.borrow_mut().exits_after = Some(3);
    let capture = jar_capture().request(GRAPHQL);
    let err = run(&mut fake, &[capture], LONG).result.unwrap_err();
    assert!(matches!(err, BrowserError::WindowClosed(_)), "{err:?}");
    let methods: Vec<String> = fake
        .0
        .borrow()
        .calls
        .iter()
        .filter(|c| !matches!(c.0.as_str(), "Target.getTargets" | "Storage.getCookies"))
        .map(|c| format!("{} {}", c.0, c.2.clone().unwrap_or_default()))
        .collect();
    assert_eq!(
        methods,
        [
            "Target.attachToTarget T-app",
            "Network.enable T-app",
            "Target.detachFromTarget T-app",
        ]
    );
    assert!(fake.0.borrow().enabled.is_empty());
}

#[test]
fn the_request_log_records_endpoints_while_the_tab_is_held_open() {
    let mut app = page("T-app", "https://app.example.com/portfolio");
    app.requests = vec![
        FakeRequest {
            response: Some((200, "application/json")),
            ..request("R1", "GET", "https://app.example.com/api/holdings/?page=1")
        },
        FakeRequest {
            after: 2,
            response: Some((200, "application/json")),
            ..request("R2", "GET", "https://app.example.com/api/holdings/?page=2")
        },
        FakeRequest {
            kind: "Document",
            response: Some((302, "text/html")),
            ..request("R3", "GET", "https://app.example.com/portfolio/vesting")
        },
        FakeRequest {
            kind: "Script",
            ..request("R4", "GET", "https://app.example.com/static/app.js")
        },
        FakeRequest {
            headers: json!({"authorization": "Bearer dummy-token"}),
            ..request("R5", "POST", "https://cdn.example.org/collect")
        },
    ];
    let mut fake = fake_with(vec![page("T-login", "https://login.app.example.com/"), app]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "sessionid", "value": "dummy-session", "domain": "app.example.com", "path": "/"}
    ]);
    let capture = Capture::new("Example", HostRule::exact("app.example.com").unwrap())
        .cookie_jar("example.com")
        .request_log("example.com", Duration::from_millis(40));
    let out = run(&mut fake, &[capture], LONG);
    let got = out.result.unwrap();
    let log: Vec<String> = got[0]
        .request_log()
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
        log,
        [
            "GET app.example.com/api/holdings/ XHR Some(200) Some(\"application/json\")",
            "GET app.example.com/portfolio/vesting Document Some(302) Some(\"text/html\")",
        ]
    );
    assert_eq!(
        out.notes[1],
        "Example: keeping the tab open 40 milliseconds to record the page's requests to \
         example.com before capturing the session; use the app there meanwhile, and leave the \
         tab open."
    );
    assert!(!format!("{:?}", got[0]).contains("dummy"));
    let enables = calls_named(&fake, "Network.enable");
    assert_eq!(enables.len(), 1);
    assert_eq!(
        enables[0].1.as_deref(),
        Some("T-app"),
        "never the login host"
    );
}

#[test]
fn a_session_held_open_at_the_deadline_is_captured_not_lost() {
    let mut fake = fake_with(vec![page("T-app", "https://app.example.com/")]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "sessionid", "value": "dummy-session", "domain": "app.example.com", "path": "/"}
    ]);
    let (calls, hook) = counting_hook(0);
    let capture = Capture::new("Example", HostRule::exact("app.example.com").unwrap())
        .cookie_jar("example.com")
        .request_log("example.com", Duration::from_secs(3600))
        .confirm(hook);
    let got = run(&mut fake, &[capture], Duration::from_millis(30))
        .result
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "confirmed once, at the end"
    );
    assert_eq!(fake.0.borrow().closed, ["T-app"]);

    // Confirmed NotYet in that last round: the timeout names it.
    let mut fake = fake_with(vec![page("T-app", "https://app.example.com/")]);
    fake.0.borrow_mut().cookies = json!([
        {"name": "sessionid", "value": "dummy-session", "domain": "app.example.com", "path": "/"}
    ]);
    let (_, hook) = counting_hook(usize::MAX);
    let capture = Capture::new("Example", HostRule::exact("app.example.com").unwrap())
        .cookie_jar("example.com")
        .request_log("example.com", Duration::from_secs(3600))
        .confirm(hook);
    let msg = run(&mut fake, &[capture], Duration::from_millis(30))
        .result
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("not confirmed: GET /me answered 401 (call 1)"),
        "{msg}"
    );
}

// ---- The user agent -----------------------------------------------------

#[test]
fn the_user_agent_is_read_once_and_handed_to_the_hook_and_the_cli() {
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    fake.0.borrow_mut().user_agent = Some("Mozilla/5.0 (dummy)");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let calls = AtomicUsize::new(0);
    let capture = jar_capture().with_user_agent().confirm(move |c| {
        log.lock().unwrap().push(c.user_agent().map(str::to_string));
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Confirm::NotYet("not yet".into())
        } else {
            Confirm::Ready
        }
    });
    let got = run(&mut fake, &[capture], LONG).result.unwrap();
    assert_eq!(got[0].user_agent(), Some("Mozilla/5.0 (dummy)"));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Some("Mozilla/5.0 (dummy)".to_string()); 2]
    );
    assert_eq!(calls_named(&fake, "Browser.getVersion").len(), 1);

    // Unreadable: the capture waits, and the timeout names why.
    let mut fake = fake_with(vec![page("T-app", "https://www.example.com/")]);
    fake.0.borrow_mut().cookies = split_jar();
    let msg = run(
        &mut fake,
        &[jar_capture().with_user_agent()],
        Duration::ZERO,
    )
    .result
    .unwrap_err()
    .to_string();
    assert!(
        msg.contains("reading the browser's user agent failed: Browser unavailable (-32000)"),
        "{msg}"
    );
    assert!(fake.0.borrow().closed.is_empty());
}

// ---- Validation of the new reads ---------------------------------------

#[test]
fn jar_request_and_log_reads_are_validated() {
    let www = || HostRule::exact("www.example.com").unwrap();
    let cases: Vec<(Capture, Option<&str>)> = vec![
        (Capture::new("X", www()).cookie_jar("example.com"), None),
        (Capture::new("X", www()).cookie_jar("www.example.com"), None),
        (Capture::new("X", www()).cookie_jar("Example.COM."), None),
        (Capture::new("X", www()).request(GRAPHQL), None),
        (
            Capture::new("X", www()).optional_request("https://www.example.com", Duration::ZERO),
            None,
        ),
        (
            Capture::new("X", www()).request_log("example.com", Duration::ZERO),
            None,
        ),
        (
            Capture::new("X", HostRule::domain("example.com").unwrap())
                .cookie_jar("app.example.com"),
            None,
        ),
        (
            Capture::new("X", www()).cookie_jar("example.org"),
            Some("reads the cookie jar of `example.org`, which none of its hosts is on"),
        ),
        (
            Capture::new("X", www()).cookie_jar("app.example.com"),
            Some("which none of its hosts is on"),
        ),
        (
            Capture::new("X", www()).cookie_jar("example.com.evil.example"),
            Some("which none of its hosts is on"),
        ),
        (
            Capture::new("X", www()).cookie_jar("com"),
            Some("reads the cookie jar of `com`, which is not a registrable domain"),
        ),
        (
            Capture::new("X", www()).cookie_jar("https://example.com/"),
            Some("which is not a registrable domain"),
        ),
        (
            Capture::new("X", www()).request("https://app.example.com/bff/graphql"),
            Some("watches for a request to https://app.example.com/bff/graphql, which is not"),
        ),
        (
            Capture::new("X", www()).request("http://www.example.com/bff/graphql"),
            Some("not an https URL on its hosts without a query"),
        ),
        (
            Capture::new("X", www()).request("https://www.example.com/bff/graphql?op=1"),
            Some("not an https URL on its hosts without a query"),
        ),
        (
            Capture::new("X", www()).request("https://www.example.com.evil.example/bff"),
            Some("not an https URL on its hosts without a query"),
        ),
        (
            Capture::new("X", www()).request_log("example.org", Duration::ZERO),
            Some("logs the requests to `example.org`, which none of its hosts is on"),
        ),
        (
            Capture::new("X", www()).with_user_agent(),
            Some("names nothing to read"),
        ),
        (
            Capture::new("X", www()).confirm(|_| Confirm::Ready),
            Some("names nothing to read"),
        ),
    ];
    for (capture, want) in cases {
        let got = capture.validate();
        match want {
            None => assert!(got.is_ok(), "{capture:?}: {got:?}"),
            Some(why) => {
                let msg = got.unwrap_err().to_string();
                assert!(msg.contains(why), "{capture:?}: {msg}");
            }
        }
    }
}

#[test]
fn captures_and_what_they_hand_over_can_cross_threads() {
    fn shareable<T: Send + Sync>() {}
    fn cloneable<T: Clone>() {}
    shareable::<Capture>();
    cloneable::<Capture>();
    shareable::<Captured>();
    shareable::<Confirm>();
}
