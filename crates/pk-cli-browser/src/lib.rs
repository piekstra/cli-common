//! Browser sign-in for the piekstra CLI family (DESIGN.md §2.2): the
//! mechanism behind `<bin> auth login --browser`.
//!
//! Some providers cannot be signed in to by a CLI: a bot check (Cloudflare
//! Turnstile, reCAPTCHA, Akamai) or an OAuth single-page app stands in front
//! of the password. The owner can still sign in on the provider's own page,
//! in a real browser, and the CLI can read the resulting web session out of
//! that browser. This crate is that: it launches Chrome with the CLI's own
//! profile and a DevTools debug port, waits while the owner signs in
//! (password and MFA happen on the provider's page, never through the CLI),
//! reads the session the web app keeps (a `localStorage` entry, a cookie,
//! `HttpOnly` cookies included, or a domain's whole cookie jar) over the
//! Chrome DevTools Protocol, closes the tab at once and kills Chrome. The
//! profile is kept for the next sign-in ([below](#the-kept-profile)).
//!
//! ```no_run
//! use pk_cli_browser::{Capture, HostRule, Launch, DEFAULT_TIMEOUT};
//! # fn main() -> Result<(), pk_cli_core::CliError> {
//! // The web app stores its OAuth session in localStorage once signed in.
//! let capture = Capture::new("Example", HostRule::exact("app.example.com")?)
//!     .expression("localStorage.getItem('session') || null")
//!     .hint("password and the texted code");
//! let captured = pk_cli_browser::capture(
//!     Launch::new("example-cli").url("https://app.example.com/login"),
//!     &capture,
//!     DEFAULT_TIMEOUT,
//! )?;
//! let session = captured.into_value().expect("the capture has an expression");
//! // Verify it with one provider read, then store it in the keychain.
//! # drop(session);
//! # Ok(())
//! # }
//! ```
//!
//! A site whose cookies exist before sign-in finishes, so that no cookie or
//! stored value proves the owner is signed in, reads its whole jar and lets
//! the CLI confirm it with one provider read before the tab closes:
//!
//! ```no_run
//! use pk_cli_browser::{Capture, Captured, Confirm, HostRule, Launch, DEFAULT_TIMEOUT};
//! # fn me_with(_cookie_header: &str) -> Result<(), pk_cli_core::CliError> { Ok(()) }
//! # fn main() -> Result<(), pk_cli_core::CliError> {
//! use pk_cli_core::CliError;
//!
//! let capture = Capture::new("Example", HostRule::exact("www.example.com")?)
//!     .cookie("https://www.example.com/", "sid") // set for visitors too
//!     .cookie_jar("example.com")
//!     .confirm(|candidate: &Captured| {
//!         let header = candidate.cookie_header().expect("the capture reads a jar");
//!         // The CLI's own request, bounded by its own timeout.
//!         match me_with(header.expose()) {
//!             Ok(()) => Confirm::Ready,
//!             Err(CliError::Auth(_)) => Confirm::NotYet("GET /me answered 401".into()),
//!             Err(e) => Confirm::Fail(e),
//!         }
//!     });
//! let captured = pk_cli_browser::capture(
//!     Launch::new("example-cli").url("https://www.example.com/login"),
//!     &capture,
//!     DEFAULT_TIMEOUT,
//! )?;
//! let session = captured.cookie_header().expect("the capture reads a jar");
//! # drop(session);
//! # Ok(())
//! # }
//! ```
//!
//! # Why a plain Chrome
//!
//! Chrome launched normally, with only a debug port and its own profile
//! and none of the flags a WebDriver or Playwright launch adds, reports
//! `navigator.webdriver == false` and looks like an ordinary browser to the
//! bot checks; the owner is the one who clicks. The crate never drives a
//! page and never sends a request of its own. It lists targets and reads
//! cookies and the user agent at the browser level, which no page can
//! observe, and evaluates the capture's expression only in pages on the
//! capture's own hosts, never on the login host where the bot checks run.
//! It enables one CDP domain in a page, `Network`, and only for a capture
//! that watches requests ([`Capture::request`],
//! [`Capture::optional_request`], [`Capture::request_log`]): only in pages
//! on that capture's hosts, from a connection that detaches as soon as a
//! page leaves them, and never with request bodies. A capture that watches
//! nothing leaves every page untouched but for the expression.
//!
//! # The kept profile
//!
//! Chrome runs in a profile the CLI keeps between sign-ins:
//! `<data dir>/piekstra.<bin>/browser-profile`, which on macOS is
//! `~/Library/Application Support/piekstra.<bin>/browser-profile` and
//! elsewhere is under `$XDG_DATA_HOME` (or `~/.local/share`). A brand-new
//! profile is an unknown device to the provider, so every sign-in in one
//! costs a password, a texted code and a bot check. The kept profile keeps
//! the provider's own cookies instead: its "remember this device" cookie,
//! its single-sign-on session, a bot check's clearance. The next sign-in
//! often needs no input at all: the window opens, the app loads already
//! signed in, the capture is read and the window closes.
//!
//! That is a trade: a profile kept on disk, for far fewer sign-ins. The
//! profile holds the provider's session cookies between runs (and whatever
//! the site keeps in `localStorage`), so:
//!
//! - it is created `0700`, and a launch is refused ([`BrowserError::Profile`])
//!   when it is a symlink, is not the current user's or is not `0700`;
//! - Chrome encrypts its cookies at rest, on macOS with the "Chrome Safe
//!   Storage" key in the login keychain (on Linux with the desktop keyring,
//!   where there is one). A site's `localStorage` is not encrypted; the
//!   directory's mode is what protects it;
//! - one sign-in uses it at a time: a second is refused
//!   ([`BrowserError::ProfileInUse`]) while the first holds it, and so is a
//!   launch while a Chrome is still running in it (its `SingletonLock` names
//!   a live pid whose program is a Chrome: one recorded in the profile when
//!   a sign-in launched it, or one named like the Chromium family). A lock
//!   left by a Chrome that is gone, or whose pid now runs another program,
//!   is cleared, with the rest of what that Chrome left;
//! - [`forget_profile`] removes it, so a CLI's `auth logout` signs the
//!   browser out too, and [`Launch::fresh_profile`] (the `--fresh-profile`
//!   flag of [`BrowserArgs`]) runs one sign-in in a scratch profile instead:
//!   created `0700`, exclusively, under an unpredictable name in the temp
//!   dir, and removed when the window closes, leaving the kept one alone.
//!
//! Chrome holds a cookie it just received in memory until it writes it, so
//! dropping the [`Browser`] asks Chrome to quit over its debug port and waits
//! for it to exit, which writes the cookie for the next sign-in. If the port is
//! dead or Chrome has not exited within 5 seconds, it gets SIGTERM, then SIGKILL
//! after 3 more, and SIGTERM can lose that cookie.
//!
//! A CLI that signs in more than one account at a provider keeps one
//! profile per account ([`Launch::profile_name`],
//! [`forget_named_profile`]), so one account's sign-in never opens already
//! signed in as another; names that differ only in case get their own
//! profiles even where the filesystem folds case. Nothing outside the
//! profile directory is ever removed or rewritten.
//!
//! The kept profile needs macOS or Linux (the run lock is `flock(2)`, and
//! Chrome's lock a symlink). Elsewhere every sign-in uses a scratch profile,
//! with a one-line note on stderr.
//!
//! # Trust boundary
//!
//! The debug port has no authentication: anything that can reach
//! `127.0.0.1:<port>`, or read `DevToolsActivePort`, can read every cookie
//! in the window while it is open. The profile, which holds the port file
//! and the session, is therefore owner-only (above). Chrome is stopped on
//! every exit path, Ctrl-C included ([`Launch::ctrl_c`]), and a scratch
//! profile is removed with it; the port file a stopped Chrome leaves in the
//! kept profile names a port nothing listens on, and is removed before the
//! next launch.
//!
//! # Errors
//!
//! Every fallible call returns a [`BrowserError`], so a CLI can branch on
//! the cause (fall back to its `--stdin` path on
//! [`BrowserError::ChromeNotFound`], recognise its own
//! [`BrowserError::Canceled`]); `?` converts it into the family's
//! `CliError` with the matching exit code.
//!
//! # Secrets
//!
//! A captured value is a [`pk_cli_secrets::Secret`]: an expression's answer,
//! a cookie, each cookie of a jar and the header built from it, and each
//! header value of a watched request. It never prints through `Debug` or
//! `Display` and is zeroized when dropped. A request's URL comes back
//! without its query, and a request log keeps no header, body or query, so
//! neither carries a token. The crate never logs a value, never passes one
//! on a command line, and never writes one to disk: Chrome's own profile is
//! the only place a session touches the filesystem (kept between sign-ins
//! unless the CLI asks for a fresh one, above). Status lines and errors
//! name hosts, labels, URLs without queries and cookie names only; a
//! [`Confirm::NotYet`] reason is shown too, so a CLI writes it the same
//! way.
//!
//! The [`Capture::confirm`] hook is the CLI's code, run in the CLI's
//! process with the candidate session: the request it makes is the CLI's
//! own, as any request the CLI makes after the capture is.
//!
//! # Testing a CLI's side
//!
//! Only the crate reads a [`Captured`] out of a browser, so a CLI's unit
//! tests build one by hand, through the `test-util` feature:
//! `Captured::for_test(label, host)` starts a builder whose steps (`value`,
//! `cookie`, `jar`, `request`, `request_log`, `user_agent`) set what each
//! getter answers, and `JarCookie::for_test`, `CapturedRequest::for_test`
//! and `LoggedRequest::for_test` build the parts. Values go in as plain
//! strings and come back as `Secret`s, normalized as read ones are (method
//! and host case, a cookie domain's leading dot, the query cut). The CLI
//! then runs its confirm hook, and its mapping of a capture onto its own
//! session, on known values.
//!
//! The feature also moves the kept profile for a test. A CLI's
//! `auth logout` calls [`forget_profile`], which removes the owner's real
//! profile, so **a logout test must never reach it**: the CLI's logout
//! takes the call through a seam its tests fill, such as a closure the
//! binary binds to `|| forget_profile(BIN)` and a test to
//! `|| forget_profile_in(&temp_dir, BIN)`. `forget_profile_in(root, bin)`
//! and `forget_named_profile_in(root, bin, name)` look under `root` instead
//! of the per-user data dir, and `Launch::profile_root(root)` does the same
//! for a launch (the profile is `<root>/piekstra.<bin>/browser-profile`).
//! `example-cli`'s logout is the worked example.
//!
//! The feature is for a CLI's `[dev-dependencies]` only, at the same tag as
//! the dependency so both name one crate, which keeps the constructors out
//! of the binary it ships:
//!
//! ```toml
//! [dependencies]
//! pk-cli-browser = { git = "https://github.com/piekstra/cli-common", tag = "vX.Y.Z" }
//!
//! [dev-dependencies]
//! pk-cli-browser = { git = "https://github.com/piekstra/cli-common", tag = "vX.Y.Z", features = ["test-util"] }
//! ```
//!
//! Without the feature none of it is compiled: the crate's API and
//! behaviour are exactly those of a build that never had it.

mod capture;
mod cdp;
mod chrome;
mod error;
mod host;
mod interrupt;
mod jar;
mod profile;
mod watch;

#[cfg(feature = "test-util")]
pub use capture::CapturedBuilder;
pub use capture::{capture, Capture, Captured, Confirm, DEFAULT_TIMEOUT};
pub use chrome::{Browser, Launch};
pub use error::BrowserError;
pub use host::HostRule;
pub use interrupt::cancel;
pub use jar::JarCookie;
pub use profile::{forget_named_profile, forget_profile};
#[cfg(any(test, feature = "test-util"))]
pub use profile::{forget_named_profile_in, forget_profile_in};
pub use watch::{CapturedRequest, LoggedRequest, RequestKind};

/// The family's `auth login --browser` flags. Flatten them into the CLI's
/// login arguments beside `pk_cli_auth::LoginArgs`, so every portal CLI
/// spells and documents them the same way, and pass `fresh_profile` on to
/// [`Launch::fresh_profile`].
#[derive(clap::Args, Debug, Default, Clone)]
#[non_exhaustive]
pub struct BrowserArgs {
    /// Sign in through a Chrome window and capture the session from it: the
    /// password and any code are entered on the provider's own page, never
    /// in this CLI.
    #[arg(long)]
    pub browser: bool,

    /// With --browser: sign in through a fresh browser profile, deleted
    /// afterwards, instead of the one this CLI keeps. The provider sees a
    /// new device (expect its full sign-in), and nothing of this sign-in
    /// stays on disk.
    #[arg(long, requires = "browser")]
    pub fresh_profile: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn the_flag_is_spelled_browser() {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            browser: BrowserArgs,
        }
        assert!(
            Cli::try_parse_from(["x", "--browser"])
                .unwrap()
                .browser
                .browser
        );
        assert!(!Cli::try_parse_from(["x"]).unwrap().browser.browser);
    }

    #[test]
    fn a_fresh_profile_is_asked_for_with_the_browser_flag() {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            browser: BrowserArgs,
        }
        let both = Cli::try_parse_from(["x", "--browser", "--fresh-profile"]).unwrap();
        assert!(both.browser.browser && both.browser.fresh_profile);
        assert!(
            !Cli::try_parse_from(["x", "--browser"])
                .unwrap()
                .browser
                .fresh_profile
        );
        // Alone it would do nothing, so it is a usage error (exit 2).
        let err = Cli::try_parse_from(["x", "--fresh-profile"]).err().unwrap();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert_eq!(err.exit_code(), 2);
    }
}
