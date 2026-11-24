//! Aura calls with a signed-in session, over [`Transport`].
//!
//! Requests carry the session as one explicit `Cookie` header and the Aura
//! token in the form, never a cookie store, and the transport follows no
//! redirect: a site answers a lost session with a redirect or its sign-in
//! page, which is exit 3, not something to follow. The user agent is the
//! site's ([`Site::with_user_agent`]); the client never presents itself as a
//! browser.

use std::time::{SystemTime, UNIX_EPOCH};

use pk_cli_core::transport::{percent_encode, HttpRequest, HttpResponse, Transport};
use pk_cli_core::CliError;
use pk_cli_secrets::Secret;

use crate::cookies::{is_sendable, merge_set_cookies};
use crate::protocol::{self, is_sign_in_page, Action, Answer, AuraError, Context};
use crate::Site;

/// A client for one run of a command: the site, the session it calls with,
/// and the app context it reads once from the home page.
pub struct Client<T> {
    transport: T,
    site: Site,
    cookie_header: Secret,
    token: Secret,
    ctx: Option<Context>,
    counter: u32,
    log_prefix: Option<String>,
    refreshed: bool,
}

impl<T: Transport> Client<T> {
    /// A client calling `site` with a signed-in session: its cookies as one
    /// `Cookie` header, and the CSRF token the site's app keeps in
    /// `localStorage` (`$AuraClientService.token$<app>`). Nothing is sent
    /// until the first call.
    pub fn new(transport: T, site: Site, cookie_header: Secret, token: Secret) -> Client<T> {
        Client {
            transport,
            site,
            cookie_header,
            token,
            ctx: None,
            counter: 0,
            log_prefix: None,
            refreshed: false,
        }
    }

    /// Log each request's actions, and the app the home page names, on
    /// stderr under `prefix` (`ex: POST /s/sfsites/aura (C.m)`). Never a
    /// cookie, a token or a parameter.
    pub fn with_log_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.log_prefix = Some(prefix.into());
        self
    }

    pub fn site(&self) -> &Site {
        &self.site
    }

    /// The session's cookie header as it stands now, when an answer rotated
    /// one of its cookies ([`merge_set_cookies`]). The caller stores it, or
    /// the stored copy goes stale while the site still holds the session.
    pub fn refreshed(&self) -> Option<&Secret> {
        self.refreshed.then_some(&self.cookie_header)
    }

    /// The app's context, read once from the signed-in home page. A
    /// redirect, a 401 or the sign-in page is exit 3; an edge answer, any
    /// other status, or a page without the context is exit 5.
    pub async fn context(&mut self) -> Result<Context, CliError> {
        if let Some(ctx) = &self.ctx {
            return Ok(ctx.clone());
        }
        let request = HttpRequest::get(self.site.home_url())
            .header("Cookie", self.cookie()?)
            .header("User-Agent", self.site.user_agent());
        let resp = self.transport.send(request).await?;
        if let Some(e) = self.edge(&resp) {
            return Err(e);
        }
        let ctx = self.site.read_home(resp.status, &resp.body)?;
        self.absorb(&resp);
        if let Some(prefix) = &self.log_prefix {
            eprintln!("{prefix}: {} app {} ready", self.site.name(), ctx.app);
        }
        self.ctx = Some(ctx.clone());
        Ok(ctx)
    }

    /// Posts `actions` as one request and parses the answer; read each
    /// action's result with [`Answer::take`] by its position.
    ///
    /// An answer that says the app's framework moved on (a `COOSE` warning
    /// beside the actions) still carries the actions' results, so it is
    /// kept, and the new framework id is used from then on. Only when an
    /// action has no result in it is the request sent once more, with the
    /// new id.
    pub async fn call(&mut self, actions: &[Action]) -> Result<Answer, CliError> {
        let answer = self.post(actions).await?;
        if !answer.out_of_sync {
            return Ok(answer);
        }
        let moved = match (&answer.fwuid, self.ctx.as_mut()) {
            (Some(fwuid), Some(ctx)) if *fwuid != ctx.fwuid => {
                ctx.fwuid = fwuid.clone();
                true
            }
            _ => false,
        };
        if !moved || (0..actions.len()).all(|n| answer.has(n)) {
            return Ok(answer);
        }
        self.post(actions).await
    }

    async fn post(&mut self, actions: &[Action]) -> Result<Answer, CliError> {
        let ctx = self.context().await?;
        let cookie = self.cookie()?;
        self.counter += 1;
        let url = self.site.aura_url(&protocol::query(self.counter, actions));
        let form = protocol::form(actions, &ctx, &self.site.page_uri(), self.token.expose());
        let body = form
            .iter()
            .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        if let Some(prefix) = &self.log_prefix {
            let labels: Vec<&str> = actions.iter().map(|a| a.label.as_str()).collect();
            eprintln!(
                "{prefix}: POST {}sfsites/aura ({})",
                self.site.page_uri(),
                labels.join(", ")
            );
        }
        let request = HttpRequest::new("POST", url)
            .header("Cookie", cookie)
            .header(
                "Content-Type",
                "application/x-www-form-urlencoded;charset=UTF-8",
            )
            .header("User-Agent", self.site.user_agent())
            .body(body);
        let resp = self.transport.send(request).await?;
        if let Some(e) = self.edge(&resp) {
            return Err(e);
        }
        self.site.read_call_status(resp.status, &resp.body)?;
        let parsed = protocol::parse(&resp.body);
        if !matches!(parsed, Err(AuraError::InvalidSession)) {
            self.absorb(&resp);
        }
        parsed.map_err(|e| e.into_cli(&self.site))
    }

    /// The cookie header, refused (exit 3) before any request when it could
    /// not go on the wire as it is.
    fn cookie(&self) -> Result<String, CliError> {
        let header = self.cookie_header.expose();
        if !is_sendable(header) {
            return Err(self.site.unusable_session());
        }
        Ok(header.to_string())
    }

    /// An edge answer, which says nothing about the session; a 403 whose
    /// page is the sign-in page is a lost session instead, left to the
    /// status reading.
    fn edge(&self, resp: &HttpResponse) -> Option<CliError> {
        if resp.status == 403 && is_sign_in_page(&resp.body) {
            return None;
        }
        resp.inconclusive(unix_now())
            .map(|answer| self.site.edge_error(answer))
    }

    fn absorb(&mut self, resp: &HttpResponse) {
        let set: Vec<&str> = resp
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.as_str())
            .collect();
        if let Some(merged) = merge_set_cookies(self.cookie_header.expose(), &set) {
            self.cookie_header = Secret::new(merged);
            self.refreshed = true;
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use pk_cli_core::block_on;
    use serde_json::json;

    use super::*;

    /// Answers each request with the next scripted response and keeps what
    /// was sent.
    #[derive(Default)]
    struct Script {
        answers: RefCell<VecDeque<HttpResponse>>,
        sent: RefCell<Vec<HttpRequest>>,
    }

    impl Script {
        fn new(answers: Vec<HttpResponse>) -> Script {
            Script {
                answers: RefCell::new(answers.into()),
                sent: RefCell::default(),
            }
        }
    }

    impl Transport for &Script {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse, CliError> {
            self.sent.borrow_mut().push(request);
            self.answers
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| CliError::Upstream("no answer scripted".into()))
        }
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn site() -> Site {
        Site::new("example.my.site.com")
            .with_name("Example")
            .with_login_hint("ex auth login --browser")
            .with_user_agent("ex/1.0 (unofficial)")
    }

    fn client(script: &Script) -> Client<&Script> {
        Client::new(
            script,
            site(),
            Secret::new("oid=1; sid=live"),
            Secret::new("TOKEN"),
        )
    }

    fn home() -> HttpResponse {
        HttpResponse::new(200, fixture("home.html"))
    }

    fn ok() -> HttpResponse {
        HttpResponse::new(200, fixture("ok.json"))
    }

    fn probe() -> [Action; 1] {
        [Action::apex(
            "ExampleController",
            "getThing",
            json!({"id": "a"}),
        )]
    }

    fn form_of(request: &HttpRequest) -> Vec<(String, String)> {
        request
            .body
            .as_deref()
            .unwrap()
            .split('&')
            .map(|pair| {
                let (n, v) = pair.split_once('=').unwrap();
                let decode = |s: &str| {
                    let mut out = Vec::new();
                    let b = s.as_bytes();
                    let mut i = 0;
                    while i < b.len() {
                        if b[i] == b'%' {
                            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                            i += 3;
                        } else {
                            out.push(b[i]);
                            i += 1;
                        }
                    }
                    String::from_utf8(out).unwrap()
                };
                (decode(n), decode(v))
            })
            .collect()
    }

    #[test]
    fn a_call_reads_the_context_once_and_posts_the_form_the_site_does() {
        let script = Script::new(vec![home(), ok(), ok()]);
        let mut c = client(&script);
        let answer = block_on(c.call(&probe())).unwrap();
        assert_eq!(answer.take(0, "probe").unwrap()["returnValue"]["rows"], 2);
        block_on(c.call(&probe())).unwrap();

        let sent = script.sent.borrow();
        assert_eq!(sent.len(), 3, "the home page is read once");
        assert_eq!(sent[0].method, "GET");
        assert_eq!(sent[0].url, "https://example.my.site.com/s/");
        assert_eq!(sent[0].header_value("cookie"), Some("oid=1; sid=live"));
        assert_eq!(
            sent[0].header_value("user-agent"),
            Some("ex/1.0 (unofficial)")
        );
        let post = &sent[1];
        assert_eq!(post.method, "POST");
        assert_eq!(
            post.url,
            "https://example.my.site.com/s/sfsites/aura?r=1&other.ExampleController.getThing=1"
        );
        assert!(sent[2].url.contains("?r=2&"));
        assert_eq!(post.header_value("cookie"), Some("oid=1; sid=live"));
        assert_eq!(
            post.header_value("content-type"),
            Some("application/x-www-form-urlencoded;charset=UTF-8")
        );
        let form = form_of(post);
        let field = |name: &str| form.iter().find(|(n, _)| n == name).unwrap().1.clone();
        let message: serde_json::Value = serde_json::from_str(&field("message")).unwrap();
        assert_eq!(
            message["actions"][0]["descriptor"],
            "apex://ExampleController/ACTION$getThing"
        );
        assert_eq!(message["actions"][0]["params"]["id"], "a");
        let context: serde_json::Value = serde_json::from_str(&field("aura.context")).unwrap();
        assert_eq!(context["fwuid"], "RFVNTVktRldVSUQ");
        assert_eq!(context["app"], "siteforce:communityApp");
        assert_eq!(field("aura.pageURI"), "/s/");
        assert_eq!(field("aura.token"), "TOKEN");
        assert!(c.refreshed().is_none());
    }

    #[test]
    fn a_stale_framework_id_keeps_the_answer_and_moves_to_the_new_id() {
        let script = Script::new(vec![
            home(),
            HttpResponse::new(200, fixture("stale-fwuid.json")),
            ok(),
        ]);
        let mut c = client(&script);
        let answer = block_on(c.call(&probe())).unwrap();
        assert_eq!(answer.take(0, "probe").unwrap(), true);
        block_on(c.call(&probe())).unwrap();
        let sent = script.sent.borrow();
        assert_eq!(
            sent.len(),
            3,
            "no second post for an answer that has every result"
        );
        let context: serde_json::Value = serde_json::from_str(
            &form_of(&sent[2])
                .into_iter()
                .find(|(n, _)| n == "aura.context")
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(context["fwuid"], "RFVNTVktRldVSUQtTkVX");
    }

    #[test]
    fn a_stale_framework_id_without_a_result_is_posted_once_more() {
        let stale_empty =
            r#"{"actions":[{"id":"COOSE","state":"warning"}],"context":{"fwuid":"NEW"}}"#;
        let script = Script::new(vec![home(), HttpResponse::new(200, stale_empty), ok()]);
        let mut c = client(&script);
        let answer = block_on(c.call(&probe())).unwrap();
        assert!(answer.has(0));
        assert_eq!(script.sent.borrow().len(), 3);

        // The same answer twice: one retry only, then the missing result is
        // the method being unknown (exit 5).
        let script = Script::new(vec![
            home(),
            HttpResponse::new(200, stale_empty),
            HttpResponse::new(200, stale_empty.replace("NEW", "NEWER")),
        ]);
        let mut c = client(&script);
        let answer = block_on(c.call(&probe())).unwrap();
        assert_eq!(
            answer
                .take(0, "probe")
                .unwrap_err()
                .into_cli(c.site())
                .exit_code(),
            5
        );
        assert_eq!(script.sent.borrow().len(), 3);
    }

    #[test]
    fn every_lost_session_is_exit_3_naming_the_sign_in() {
        let signed_out = fixture("signed-out.html");
        let cases = vec![
            vec![HttpResponse::new(302, "").header("location", "/s/login")],
            vec![HttpResponse::new(200, signed_out.clone())],
            vec![HttpResponse::new(403, signed_out.clone()).header("content-type", "text/html")],
            vec![home(), HttpResponse::new(200, fixture("invalid-csrf.txt"))],
            vec![home(), HttpResponse::new(302, "")],
            vec![
                home(),
                HttpResponse::new(403, signed_out.clone()).header("content-type", "text/html"),
            ],
            vec![home(), HttpResponse::new(200, signed_out)],
        ];
        for (n, answers) in cases.into_iter().enumerate() {
            let script = Script::new(answers);
            let mut c = client(&script);
            let e = block_on(c.call(&probe())).unwrap_err();
            assert_eq!(e.exit_code(), 3, "case {n}: {e}");
            assert!(
                e.to_string().contains("`ex auth login --browser`"),
                "case {n}: {e}"
            );
        }
        // A call that ran as the guest user answers 200; its action is the
        // lost session.
        let script = Script::new(vec![
            home(),
            HttpResponse::new(200, fixture("guest-no-access.json")),
        ]);
        let mut c = client(&script);
        let answer = block_on(c.call(&probe())).unwrap();
        assert_eq!(
            answer
                .take(0, "probe")
                .unwrap_err()
                .into_cli(c.site())
                .exit_code(),
            3
        );
    }

    #[test]
    fn an_edge_answer_is_exit_5_and_never_a_lost_session() {
        let cases = vec![
            vec![HttpResponse::new(429, "").header("retry-after", "120")],
            vec![HttpResponse::new(403, "<html>blocked</html>").header("content-type", "text/html")],
            vec![
                home(),
                HttpResponse::new(503, "<html>down</html>").header("content-type", "text/html"),
            ],
            vec![
                home(),
                HttpResponse::new(200, "<html>Service Unavailable</html>"),
            ],
            vec![home(), HttpResponse::new(500, "{}")],
        ];
        for (n, answers) in cases.into_iter().enumerate() {
            let script = Script::new(answers);
            let mut c = client(&script);
            let e = block_on(c.call(&probe())).unwrap_err();
            assert_eq!(e.exit_code(), 5, "case {n}: {e}");
        }
        let script = Script::new(vec![HttpResponse::new(429, "").header("retry-after", "120")]);
        let e = block_on(client(&script).call(&probe())).unwrap_err();
        assert!(e.to_string().ends_with("try again in 120 s"), "{e}");
    }

    #[test]
    fn a_rotated_cookie_is_kept_and_sent_from_then_on() {
        let script = Script::new(vec![
            home().header("set-cookie", "renderCtx=x; Path=/"),
            ok().header("set-cookie", "sid=rotated; Path=/; Secure; HttpOnly"),
            ok(),
        ]);
        let mut c = client(&script);
        assert!(block_on(c.context()).is_ok());
        assert!(
            c.refreshed().is_none(),
            "a cookie the session does not carry is not added"
        );
        block_on(c.call(&probe())).unwrap();
        assert_eq!(c.refreshed().unwrap().expose(), "oid=1; sid=rotated");
        block_on(c.call(&probe())).unwrap();
        assert_eq!(
            script.sent.borrow()[2].header_value("cookie"),
            Some("oid=1; sid=rotated")
        );
    }

    #[test]
    fn a_lost_session_answer_rotates_nothing() {
        let script = Script::new(vec![
            home(),
            HttpResponse::new(200, fixture("invalid-csrf.txt"))
                .header("set-cookie", "sid=guest; Path=/"),
        ]);
        let mut c = client(&script);
        assert_eq!(block_on(c.call(&probe())).unwrap_err().exit_code(), 3);
        assert!(c.refreshed().is_none());
        let script = Script::new(vec![
            HttpResponse::new(302, "").header("set-cookie", "sid=guest; Path=/")
        ]);
        let mut c = client(&script);
        assert_eq!(block_on(c.context()).unwrap_err().exit_code(), 3);
        assert!(c.refreshed().is_none());
    }

    #[test]
    fn an_unsendable_cookie_header_is_exit_3_before_any_request() {
        let script = Script::new(vec![home()]);
        let mut c = Client::new(
            &script,
            site(),
            Secret::new("sid=a\r\nX-Injected: 1"),
            Secret::new("TOKEN"),
        );
        let e = block_on(c.call(&probe())).unwrap_err();
        assert_eq!(e.exit_code(), 3);
        assert!(e.to_string().contains("not a valid cookie header"), "{e}");
        assert!(script.sent.borrow().is_empty());
    }

    #[test]
    fn a_site_with_a_url_prefix_is_called_under_it() {
        let script = Script::new(vec![home(), ok()]);
        let mut c = Client::new(
            &script,
            site().with_base_path("/members/s"),
            Secret::new("sid=live"),
            Secret::new("TOKEN"),
        );
        block_on(c.call(&probe())).unwrap();
        let sent = script.sent.borrow();
        assert_eq!(sent[0].url, "https://example.my.site.com/members/s/");
        assert!(sent[1]
            .url
            .starts_with("https://example.my.site.com/members/s/sfsites/aura?r=1"));
        assert!(form_of(&sent[1]).contains(&("aura.pageURI".into(), "/members/s/".into())));
    }

    #[test]
    fn a_request_without_an_answer_is_exit_5_naming_no_secret() {
        let script = Script::new(vec![]);
        let e = block_on(client(&script).call(&probe())).unwrap_err();
        assert_eq!(e.exit_code(), 5);
        assert!(!e.to_string().contains("sid=live"));
    }
}
