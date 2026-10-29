//! The wait against an in-memory fake browser that answers the CDP calls
//! the crate makes, so the whole poll (targets, attach, evaluate, cookies,
//! close) runs offline and deterministically. Every value here is an
//! obvious dummy.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

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
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<State>>);

struct FakeTransport {
    state: Fake,
    inbox: VecDeque<String>,
}

impl Transport for FakeTransport {
    fn send(&mut self, text: &str) -> Result<(), String> {
        let req: Value = serde_json::from_str(text).unwrap();
        let id = req["id"].as_u64().unwrap();
        let method = req["method"].as_str().unwrap().to_string();
        let session = req["sessionId"].as_str().map(str::to_string);
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let mut s = self.state.0.borrow_mut();
        let target = params["targetId"].as_str().map(str::to_string).or_else(|| {
            session
                .as_ref()
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
                s.sessions.remove(params["sessionId"].as_str().unwrap());
                Ok(json!({}))
            }
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
        Ok(self.inbox.pop_front())
    }
}

fn never() -> bool {
    false
}

impl Driver for Fake {
    type T = FakeTransport;
    fn connect(&mut self) -> Result<FakeTransport, String> {
        if let Some(e) = &self.0.borrow().connect_error {
            return Err(e.clone());
        }
        Ok(FakeTransport {
            state: self.clone(),
            inbox: VecDeque::new(),
        })
    }
    fn exited(&mut self) -> bool {
        self.0.borrow().exited
    }
    fn canceled(&self) -> bool {
        self.0.borrow().canceled
    }
    fn cancel_check(&self) -> fn() -> bool {
        never
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
    result: Result<Vec<Captured>, CliError>,
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
            "returned a object, not a string",
        ),
        (json!(["x"]), "returned a object, not a string"),
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
    let progress = |saw_page, check, missing: &[&str]| Progress {
        saw_page,
        check,
        missing_cookies: missing.iter().map(|s| s.to_string()).collect(),
    };
    let host = || "app.example.com".to_string();
    let cases: Vec<(Progress, &str)> = vec![
        (
            progress(false, None, &[]),
            "no page on app.example.com was open",
        ),
        (progress(true, None, &["a"]), "the a cookie was not set"),
        (
            progress(true, None, &["a", "b"]),
            "the a and b cookies were not set",
        ),
        (
            progress(true, Some((host(), Check::Unreadable)), &[]),
            "the session check on app.example.com returned an unreadable answer",
        ),
        (
            progress(true, None, &[]),
            "a page was open, but the session was not ready",
        ),
    ];
    for (p, want) in cases {
        assert_eq!(diagnose(&capture, &p), want);
    }
}
