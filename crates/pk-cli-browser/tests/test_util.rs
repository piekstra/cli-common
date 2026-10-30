//! The `test-util` constructors, used the way a CLI's own unit tests use
//! them: build what a capture hands over, run the CLI's confirm hook on it,
//! and read the jar, the request and the request log back through the
//! public getters, as the CLI's mapping onto its own session does. Only the
//! public API is used, as from a CLI's `[dev-dependencies]`. Every value is
//! an obvious dummy.

#![cfg(feature = "test-util")]

use std::time::Duration;

use pk_cli_browser::{
    Capture, Captured, CapturedRequest, Confirm, HostRule, JarCookie, LoggedRequest, RequestKind,
};
use pk_cli_core::CliError;

/// What a CLI's browser module holds for its portal: the capture, the
/// confirm hook it registers, and the mappings of a capture onto the CLI's
/// own session and endpoint list. The provider read the hook makes is a
/// stand-in that answers from the cookie header, as a loopback server would.
mod cli {
    use super::*;

    pub const SESSION_COOKIE: &str = "sid";
    pub const GRAPHQL: &str = "https://www.example.com/bff/graphql";

    pub fn browser_capture() -> Result<Capture, CliError> {
        Ok(Capture::new("Example", HostRule::exact("www.example.com")?)
            .cookie("https://www.example.com/", SESSION_COOKIE)
            .cookie_jar("example.com")
            .optional_request(GRAPHQL, Duration::from_secs(25))
            .request_log("example.com", Duration::from_secs(60))
            .with_user_agent()
            .confirm(confirm))
    }

    /// The provider read: signed in only with the session a sign-in sets.
    fn me(cookie_header: &str) -> Result<(), CliError> {
        if cookie_header.contains("sid=dummy-signed-in") {
            Ok(())
        } else {
            Err(CliError::Auth("GET /me answered 401".into()))
        }
    }

    /// The hook `browser_capture` registers.
    pub fn confirm(candidate: &Captured) -> Confirm {
        let Some(header) = candidate.cookie_header() else {
            return Confirm::Fail(CliError::Other("the capture read no cookie jar".into()));
        };
        match me(header.expose()) {
            Ok(()) => Confirm::Ready,
            Err(CliError::Auth(why)) => Confirm::NotYet(why),
            Err(e) => Confirm::Fail(e),
        }
    }

    /// The session the CLI stores.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Session {
        pub cookie: String,
        pub user_agent: Option<String>,
        /// The CSRF header's name, taken from the app's own request.
        pub csrf_header: Option<String>,
    }

    pub fn session(captured: &Captured) -> Option<Session> {
        let cookie = captured
            .jar()
            .iter()
            .map(|c| format!("{}={}", c.name(), c.value().expose()))
            .collect::<Vec<_>>()
            .join("; ");
        captured.cookie(SESSION_COOKIE)?;
        Some(Session {
            cookie,
            user_agent: captured.user_agent().map(str::to_string),
            csrf_header: captured.request().and_then(|r| {
                r.headers()
                    .map(|(name, _)| name)
                    .find(|name| name.contains("csrf"))
                    .map(str::to_string)
            }),
        })
    }

    /// The endpoints the app called, as a `--show-endpoints` line each.
    pub fn endpoints(captured: &Captured) -> Vec<String> {
        captured
            .request_log()
            .iter()
            .map(|e| {
                let status = e.status().map_or("?".to_string(), |s| s.to_string());
                format!(
                    "{} {}{} {} {status} {}",
                    e.method(),
                    e.host(),
                    e.path(),
                    e.kind(),
                    e.mime().unwrap_or("?")
                )
            })
            .collect()
    }
}

fn jar(sid: &str) -> Vec<JarCookie> {
    vec![
        JarCookie::for_test(cli::SESSION_COOKIE, sid, ".example.com", false, "/"),
        JarCookie::for_test("csrf", "dummy-csrf", "www.example.com", true, "/"),
        JarCookie::for_test("pref", "dummy-pref", "www.example.com", true, "/app"),
    ]
}

fn signed_in() -> Captured {
    Captured::for_test("Example", "www.example.com")
        .cookie(cli::SESSION_COOKIE, "dummy-signed-in")
        .jar(jar("dummy-signed-in"))
        .request(CapturedRequest::for_test(
            "POST",
            "https://www.example.com/bff/graphql?op=Accounts",
            &[
                ("X-CSRF-Token", "dummy-csrf"),
                ("content-type", "application/json"),
            ],
        ))
        .request_log([
            LoggedRequest::for_test(
                "GET",
                "www.example.com",
                "/app/home",
                RequestKind::Document,
                Some(200),
                Some("text/html"),
            ),
            LoggedRequest::for_test(
                "POST",
                "api.example.com",
                "/v1/accounts",
                RequestKind::Fetch,
                None,
                None,
            ),
        ])
        .user_agent("Mozilla/5.0 (dummy)")
        .build()
}

#[test]
fn the_cli_s_capture_is_well_formed() {
    cli::browser_capture().unwrap().validate().unwrap();
}

#[test]
fn a_signed_in_capture_is_confirmed_and_read_back_through_the_getters() {
    let captured = signed_in();
    assert!(matches!(cli::confirm(&captured), Confirm::Ready));

    assert_eq!(captured.label(), "Example");
    assert_eq!(captured.host(), "www.example.com");
    assert!(captured.value().is_none());
    assert_eq!(
        captured.cookie(cli::SESSION_COOKIE).unwrap().expose(),
        "dummy-signed-in"
    );

    let jar = captured.jar();
    let scopes: Vec<(&str, &str, bool, &str)> = jar
        .iter()
        .map(|c| (c.name(), c.domain(), c.host_only(), c.path()))
        .collect();
    assert_eq!(
        scopes,
        [
            ("sid", "example.com", false, "/"),
            ("csrf", "www.example.com", true, "/"),
            ("pref", "www.example.com", true, "/app"),
        ]
    );
    assert_eq!(jar[1].value().expose(), "dummy-csrf");
    assert_eq!(
        captured.cookie_header().unwrap().expose(),
        "pref=dummy-pref; sid=dummy-signed-in; csrf=dummy-csrf",
        "the header lists longer paths first, as the browser sends them"
    );

    let request = captured.request().unwrap();
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url(), "https://www.example.com/bff/graphql");
    let names: Vec<&str> = request.headers().map(|(name, _)| name).collect();
    assert_eq!(names, ["content-type", "x-csrf-token"]);
    assert_eq!(
        request.header("X-Csrf-Token").unwrap().expose(),
        "dummy-csrf"
    );

    assert_eq!(
        cli::endpoints(&captured),
        [
            "GET www.example.com/app/home Document 200 text/html",
            "POST api.example.com/v1/accounts Fetch ? ?",
        ]
    );
    assert_eq!(captured.user_agent(), Some("Mozilla/5.0 (dummy)"));

    assert_eq!(
        cli::session(&captured),
        Some(cli::Session {
            cookie: "sid=dummy-signed-in; csrf=dummy-csrf; pref=dummy-pref".into(),
            user_agent: Some("Mozilla/5.0 (dummy)".into()),
            csrf_header: Some("x-csrf-token".into()),
        })
    );
}

#[test]
fn a_visitor_s_jar_keeps_the_wait_going_and_no_jar_ends_it() {
    let visitor = Captured::for_test("Example", "www.example.com")
        .cookie(cli::SESSION_COOKIE, "dummy-visitor")
        .jar(jar("dummy-visitor"))
        .build();
    match cli::confirm(&visitor) {
        Confirm::NotYet(reason) => assert_eq!(reason, "GET /me answered 401"),
        other => panic!("a visitor's jar must keep the wait going: {other:?}"),
    }

    let no_jar = Captured::for_test("Example", "www.example.com").build();
    match cli::confirm(&no_jar) {
        Confirm::Fail(e) => assert_eq!(e.exit_code(), 1),
        other => panic!("a capture without a jar must end the wait: {other:?}"),
    }
    assert_eq!(cli::session(&no_jar), None);
}

#[test]
fn a_bare_capture_holds_only_its_label_and_host() {
    let bare = Captured::for_test("Example", "WWW.Example.com.").build();
    assert_eq!(bare.label(), "Example");
    assert_eq!(bare.host(), "www.example.com");
    assert!(bare.value().is_none());
    assert!(bare.cookie(cli::SESSION_COOKIE).is_none());
    assert!(bare.jar().is_empty());
    assert!(bare.cookie_header().is_none());
    assert!(bare.request().is_none());
    assert!(bare.request_log().is_empty());
    assert!(bare.user_agent().is_none());
    assert!(bare.into_value().is_none());
}

#[test]
fn a_later_step_replaces_an_earlier_one_and_cookies_are_kept_by_name() {
    let captured = Captured::for_test("Example", "app.example.com")
        .value("dummy-first")
        .value(r#"{"access_token":"dummy-access"}"#)
        .cookie("a", "dummy-a1")
        .cookie("b", "dummy-b")
        .cookie("a", "dummy-a2")
        .jar(jar("dummy-signed-in"))
        .jar([])
        .user_agent("first")
        .user_agent("second")
        .build();
    assert_eq!(captured.cookie("a").unwrap().expose(), "dummy-a2");
    assert_eq!(captured.cookie("b").unwrap().expose(), "dummy-b");
    assert!(captured.jar().is_empty());
    assert_eq!(captured.cookie_header().unwrap().expose(), "");
    assert_eq!(captured.user_agent(), Some("second"));
    assert_eq!(
        captured.into_value().unwrap().expose(),
        r#"{"access_token":"dummy-access"}"#
    );
}

#[test]
fn the_constructors_normalize_as_a_read_does() {
    let request = CapturedRequest::for_test(
        "post",
        "https://WWW.Example.com:443/bff/graphql?token=dummy-query#frag",
        &[("B", "2"), ("a", "1"), ("a", "dummy-later")],
    );
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url(), "https://www.example.com/bff/graphql");
    let headers: Vec<(&str, &str)> = request
        .headers()
        .map(|(name, value)| (name, value.expose()))
        .collect();
    assert_eq!(headers, [("a", "dummy-later"), ("b", "2")]);

    let logged = LoggedRequest::for_test(
        "get",
        "API.Example.com",
        "/v1/x?page=2#top",
        RequestKind::Xhr,
        Some(404),
        Some("application/json"),
    );
    assert_eq!(logged.method(), "GET");
    assert_eq!(logged.host(), "api.example.com");
    assert_eq!(logged.path(), "/v1/x");
    assert_eq!(logged.kind(), RequestKind::Xhr);
    assert_eq!(logged.status(), Some(404));
    assert_eq!(logged.mime(), Some("application/json"));

    let cookie = JarCookie::for_test("sid", "dummy-sid", ".Example.COM.", false, "/");
    assert_eq!(cookie.domain(), "example.com");
}

#[test]
fn values_never_print() {
    let builder = Captured::for_test("Example", "www.example.com")
        .value("dummy-value")
        .cookie("named", "dummy-named");
    let debug = format!("{builder:?}");
    let captured = builder
        .jar(jar("dummy-signed-in"))
        .request(CapturedRequest::for_test(
            "GET",
            cli::GRAPHQL,
            &[("x-token", "dummy-header")],
        ))
        .build();
    let debug = format!("{debug} {captured:?}");
    for value in [
        "dummy-value",
        "dummy-named",
        "dummy-signed-in",
        "dummy-header",
    ] {
        assert!(!debug.contains(value), "{value} printed: {debug}");
    }
}

#[test]
#[should_panic(expected = "CapturedRequest::for_test: the URL is not https on the default port")]
fn a_request_off_https_is_refused() {
    let _ = CapturedRequest::for_test("GET", "http://www.example.com/api", &[]);
}

#[test]
#[should_panic(expected = "CapturedRequest::for_test: the URL is not https on the default port")]
fn a_request_off_the_default_port_is_refused() {
    let _ = CapturedRequest::for_test("GET", "https://www.example.com:8443/api", &[]);
}

#[test]
#[should_panic(expected = "JarCookie::for_test: the domain `exa mple.com` is not a host name")]
fn a_cookie_off_a_host_name_is_refused() {
    let _ = JarCookie::for_test("sid", "dummy", "exa mple.com", true, "/");
}

#[test]
#[should_panic(expected = "LoggedRequest::for_test: `https://example.com` is not a host name")]
fn a_log_entry_with_a_url_for_a_host_is_refused() {
    let _ = LoggedRequest::for_test(
        "GET",
        "https://example.com",
        "/",
        RequestKind::Xhr,
        None,
        None,
    );
}

#[test]
#[should_panic(expected = "LoggedRequest::for_test: the path `api/x` does not start with `/`")]
fn a_log_entry_with_a_relative_path_is_refused() {
    let _ = LoggedRequest::for_test("GET", "example.com", "api/x", RequestKind::Xhr, None, None);
}

#[test]
#[should_panic(expected = "Captured::for_test: `example.com/app` is not a host name")]
fn a_capture_off_a_host_name_is_refused() {
    let _ = Captured::for_test("Example", "example.com/app");
}
