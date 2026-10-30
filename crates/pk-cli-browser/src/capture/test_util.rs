//! Building what a capture hands over by hand, for a CLI's unit tests: the
//! `test-util` feature (see the crate docs). Nothing here is compiled
//! without it.

use pk_cli_secrets::Secret;

use super::Captured;
use crate::host::normalize_host;
use crate::jar::JarCookie;
use crate::watch::{CapturedRequest, LoggedRequest};

impl Captured {
    /// Start building a `Captured` by hand, for a CLI's unit tests (the
    /// `test-util` feature, a dev-dependency only; see the crate docs): the
    /// capture labelled `label`, read from a page on `host`, holding nothing
    /// else, as a capture that reads nothing else hands it over. Each step
    /// of the [`CapturedBuilder`] sets what one getter answers, and
    /// [`CapturedBuilder::build`] ends it. `host` is lowercased, as a read
    /// one is.
    ///
    /// With it a CLI runs its confirm hook, and its mapping of a capture
    /// onto its own session, on known values without a browser:
    ///
    /// ```
    /// use pk_cli_browser::{Captured, Confirm, JarCookie};
    ///
    /// // The CLI's confirm hook, the function it passes to `Capture::confirm`.
    /// fn confirm(candidate: &Captured) -> Confirm {
    ///     if candidate.jar().iter().any(|c| c.name() == "sid") {
    ///         Confirm::Ready
    ///     } else {
    ///         Confirm::NotYet("the jar holds no sid cookie yet".into())
    ///     }
    /// }
    ///
    /// let signed_in = Captured::for_test("Example", "www.example.com")
    ///     .jar([
    ///         JarCookie::for_test("sid", "dummy-sid", ".example.com", false, "/"),
    ///         JarCookie::for_test("pref", "dummy-pref", "www.example.com", true, "/app"),
    ///     ])
    ///     .build();
    /// assert!(matches!(confirm(&signed_in), Confirm::Ready));
    /// // The header the crate builds from the jar: longer paths first.
    /// let header = signed_in.cookie_header().expect("the capture has a jar");
    /// assert_eq!(header.expose(), "pref=dummy-pref; sid=dummy-sid");
    ///
    /// let visitor = Captured::for_test("Example", "www.example.com")
    ///     .jar([JarCookie::for_test("pref", "dummy-pref", "www.example.com", true, "/")])
    ///     .build();
    /// assert!(matches!(confirm(&visitor), Confirm::NotYet(_)));
    /// ```
    ///
    /// # Panics
    ///
    /// If `host` is not a host name.
    pub fn for_test(label: impl Into<String>, host: &str) -> CapturedBuilder {
        let Some(normalized) = normalize_host(host) else {
            panic!("Captured::for_test: `{host}` is not a host name");
        };
        CapturedBuilder {
            captured: Captured {
                label: label.into(),
                host: normalized,
                value: None,
                cookies: Vec::new(),
                jar: None,
                request: None,
                request_log: None,
                user_agent: None,
            },
        }
    }
}

/// A [`Captured`] being built by hand, from [`Captured::for_test`]. Each step
/// sets what one of its getters answers, and a later call to a step replaces
/// an earlier one, except [`cookie`](CapturedBuilder::cookie), which adds a
/// cookie (replacing only one of the same name). Values go in as plain
/// strings and are wrapped in [`Secret`]s, as read ones are.
#[derive(Debug)]
#[must_use = "a builder does nothing until `build` is called"]
pub struct CapturedBuilder {
    captured: Captured,
}

impl CapturedBuilder {
    /// The expression's answer: [`Captured::value`] and
    /// [`Captured::into_value`].
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.captured.value = Some(Secret::new(value));
        self
    }

    /// A cookie the capture named: [`Captured::cookie`].
    pub fn cookie(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        self.captured.cookies.retain(|(n, _)| *n != name);
        self.captured.cookies.push((name, Secret::new(value)));
        self
    }

    /// The jar a cookie-jar capture read, in the order given (the browser's
    /// listing order): [`Captured::jar`], and the header
    /// [`Captured::cookie_header`] builds from it as it does from a read
    /// one. Once set, the header is `Some`, even for an empty jar, which a
    /// real capture never hands over.
    pub fn jar(mut self, cookies: impl IntoIterator<Item = JarCookie>) -> Self {
        self.captured.jar = Some(cookies.into_iter().collect());
        self
    }

    /// The watched request: [`Captured::request`].
    pub fn request(mut self, request: CapturedRequest) -> Self {
        self.captured.request = Some(request);
        self
    }

    /// The request log, in the order given (the order the endpoints were
    /// first seen): [`Captured::request_log`].
    pub fn request_log(mut self, entries: impl IntoIterator<Item = LoggedRequest>) -> Self {
        self.captured.request_log = Some(entries.into_iter().collect());
        self
    }

    /// The browser's `User-Agent`: [`Captured::user_agent`].
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.captured.user_agent = Some(user_agent.into());
        self
    }

    /// The `Captured`, as a capture hands it to a confirm hook or to the
    /// CLI.
    pub fn build(self) -> Captured {
        self.captured
    }
}
