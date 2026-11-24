//! Which Experience Cloud site a client talks to, and what its HTTP answers
//! mean for a signed-in session.

use pk_cli_core::transport::Inconclusive;
use pk_cli_core::CliError;

use crate::protocol::{bootstrap_context, is_sign_in_page, Context};

/// An Experience Cloud site: its host, the path its pages live under, and
/// the words its errors use. Built with [`Site::new`] and its `with_*` steps.
#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    host: String,
    base: String,
    name: String,
    login_hint: Option<String>,
    lost_session_note: Option<String>,
    user_agent: String,
}

impl Site {
    /// The site at `host` (`example.my.site.com`, no scheme or path), with
    /// its pages under `/s`, named by its host in errors.
    pub fn new(host: impl Into<String>) -> Site {
        let host = host.into();
        Site {
            name: host.clone(),
            host,
            base: "/s".into(),
            login_hint: None,
            lost_session_note: None,
            user_agent: format!("pk-cli-aura/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// The path the site's pages live under: `/s` unless the site has a URL
    /// prefix (`/members/s`). A missing leading `/` is added and a trailing
    /// one dropped.
    pub fn with_base_path(mut self, path: &str) -> Site {
        let trimmed = path.trim_matches('/');
        self.base = format!("/{trimmed}");
        self
    }

    /// What errors call the site, as a name: "Example" reads "the Example
    /// session has ended" and "Example answered HTTP 503".
    pub fn with_name(mut self, name: impl Into<String>) -> Site {
        self.name = name.into();
        self
    }

    /// The command that signs in again, named by every exit-3 error.
    pub fn with_login_hint(mut self, command: impl Into<String>) -> Site {
        self.login_hint = Some(command.into());
        self
    }

    /// Why a session ends, in parentheses after "the session has ended"
    /// ("the site signs out after an hour idle").
    pub fn with_lost_session_note(mut self, note: impl Into<String>) -> Site {
        self.lost_session_note = Some(note.into());
        self
    }

    /// The `User-Agent` every request carries: the CLI's own name, never a
    /// browser's.
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Site {
        self.user_agent = user_agent.into();
        self
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// The signed-in home page, whose scripts carry the app's context.
    pub fn home_url(&self) -> String {
        format!("https://{}{}/", self.host, self.base)
    }

    /// The `aura.pageURI` a call names: the home page's path.
    pub fn page_uri(&self) -> String {
        format!("{}/", self.base)
    }

    /// The URL a call is posted to, with its query string.
    pub fn aura_url(&self, query: &str) -> String {
        format!("https://{}{}/sfsites/aura?{query}", self.host, self.base)
    }

    /// Exit 3: the session is gone, and how to sign in again.
    pub fn lost_session(&self) -> CliError {
        let note = self
            .lost_session_note
            .as_ref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        CliError::Auth(format!(
            "the {} session has ended{note}; {}",
            self.name,
            self.sign_in_again()
        ))
    }

    /// Exit 3: what is stored is not a session a request can carry.
    pub fn unusable_session(&self) -> CliError {
        CliError::Auth(format!(
            "the stored {} session is not a valid cookie header; {}",
            self.name,
            self.sign_in_again()
        ))
    }

    fn sign_in_again(&self) -> String {
        match &self.login_hint {
            Some(hint) => format!("sign in again with `{hint}`"),
            None => "sign in again".into(),
        }
    }

    /// Exit 5 for an answer that says nothing about the session: a 429, a
    /// 5xx, or a 403 with an HTML page (cli-common DESIGN.md §2.2), with the
    /// wait the site asked for.
    pub fn edge_error(&self, answer: Inconclusive) -> CliError {
        let wait = answer.retry_after.map_or_else(
            || " later".to_string(),
            |d| format!(" in {} s", d.as_secs()),
        );
        CliError::Upstream(format!(
            "{} answered HTTP {} (an edge block, a rate limit or an outage), which says nothing \
             about the session; try again{wait}",
            self.name, answer.status
        ))
    }

    /// What a home-page answer that is not an edge answer means: the
    /// signed-in app's context, a lost session (a redirect, a 401, or the
    /// sign-in page under any status), or the site having changed.
    pub fn read_home(&self, status: u16, body: &str) -> Result<Context, CliError> {
        if is_sign_in_page(body) || matches!(status, 300..=399 | 401) {
            return Err(self.lost_session());
        }
        if status != 200 {
            return Err(CliError::Upstream(format!(
                "{}'s home page answered HTTP {status}",
                self.name
            )));
        }
        bootstrap_context(body).ok_or_else(|| {
            CliError::Upstream(format!(
                "{}'s home page carries no app context: the site may have changed",
                self.name
            ))
        })
    }

    /// What a call's status means before its body is parsed: a redirect, a
    /// 401 or a 403 sign-in page is a lost session; any other answer but a
    /// 200 is the site's fault.
    pub fn read_call_status(&self, status: u16, body: &str) -> Result<(), CliError> {
        match status {
            200 => Ok(()),
            300..=399 | 401 => Err(self.lost_session()),
            403 if is_sign_in_page(body) => Err(self.lost_session()),
            s => Err(CliError::Upstream(format!(
                "{} answered HTTP {s}",
                self.name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pk_cli_core::transport::inconclusive_answer;

    fn site() -> Site {
        Site::new("example.my.site.com")
            .with_name("Example")
            .with_login_hint("ex auth login --browser")
            .with_lost_session_note("the site signs out after an hour idle")
    }

    #[test]
    fn urls_follow_the_base_path() {
        let s = site();
        assert_eq!(s.home_url(), "https://example.my.site.com/s/");
        assert_eq!(s.page_uri(), "/s/");
        assert_eq!(
            s.aura_url("r=1"),
            "https://example.my.site.com/s/sfsites/aura?r=1"
        );
        let prefixed = site().with_base_path("members/s/");
        assert_eq!(
            prefixed.home_url(),
            "https://example.my.site.com/members/s/"
        );
        assert_eq!(prefixed.page_uri(), "/members/s/");
        assert_eq!(
            prefixed.aura_url("r=2"),
            "https://example.my.site.com/members/s/sfsites/aura?r=2"
        );
    }

    #[test]
    fn a_lost_session_names_the_site_the_reason_and_the_sign_in() {
        assert_eq!(
            site().lost_session().to_string(),
            "the Example session has ended (the site signs out after an hour idle); \
             sign in again with `ex auth login --browser`"
        );
        assert_eq!(site().lost_session().exit_code(), 3);
        assert_eq!(
            Site::new("h.example.com").lost_session().to_string(),
            "the h.example.com session has ended; sign in again"
        );
        assert_eq!(site().unusable_session().exit_code(), 3);
    }

    #[test]
    fn an_edge_answer_is_exit_5_with_its_wait() {
        let limited = site().edge_error(inconclusive_answer(429, None, Some("120"), 0).unwrap());
        assert_eq!(limited.exit_code(), 5);
        assert!(
            limited.to_string().ends_with("try again in 120 s"),
            "{limited}"
        );
        let blocked =
            site().edge_error(inconclusive_answer(403, Some("text/html"), None, 0).unwrap());
        assert!(
            blocked.to_string().ends_with("try again later"),
            "{blocked}"
        );
    }

    #[test]
    fn home_page_answers_map_to_exit_codes() {
        let signed_out = include_str!("../tests/fixtures/signed-out.html");
        let home = include_str!("../tests/fixtures/home.html");
        let s = site();
        assert_eq!(s.read_home(200, home).unwrap().fwuid, "RFVNTVktRldVSUQ");
        assert_eq!(s.read_home(200, signed_out).unwrap_err().exit_code(), 3);
        assert_eq!(s.read_home(403, signed_out).unwrap_err().exit_code(), 3);
        assert_eq!(s.read_home(302, "").unwrap_err().exit_code(), 3);
        assert_eq!(s.read_home(401, "").unwrap_err().exit_code(), 3);
        assert_eq!(s.read_home(404, "").unwrap_err().exit_code(), 5);
        // A 200 that is neither the app nor the sign-in page is the site
        // having changed, not a lost session.
        assert_eq!(
            s.read_home(200, "<html>maintenance</html>")
                .unwrap_err()
                .exit_code(),
            5
        );
    }

    #[test]
    fn call_statuses_map_to_exit_codes() {
        let signed_out = include_str!("../tests/fixtures/signed-out.html");
        let s = site();
        assert!(s.read_call_status(200, "").is_ok());
        assert_eq!(s.read_call_status(302, "").unwrap_err().exit_code(), 3);
        assert_eq!(
            s.read_call_status(403, signed_out).unwrap_err().exit_code(),
            3
        );
        assert_eq!(s.read_call_status(403, "{}").unwrap_err().exit_code(), 5);
        assert_eq!(s.read_call_status(500, "").unwrap_err().exit_code(), 5);
    }
}
