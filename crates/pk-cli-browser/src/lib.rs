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
//! Chrome DevTools Protocol, closes the tab at once and stops Chrome (on a
//! kept profile by asking it to quit, so a fresh cookie is written). The
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
//! # enum Me { Accepted, Refused, CouldNotTell(u16, Option<std::time::Duration>), NoAnswer }
//! # fn me_with(_cookie_header: &str) -> Me { Me::Accepted }
//! # fn main() -> Result<(), pk_cli_core::CliError> {
//!
//! let capture = Capture::new("Example", HostRule::exact("www.example.com")?)
//!     .cookie("https://www.example.com/", "sid") // set for visitors too
//!     .cookie_jar("example.com")
//!     .confirm(|candidate: &Captured| {
//!         let header = candidate.cookie_header().expect("the capture reads a jar");
//!         // The CLI's own request, bounded by its own timeout. It judges the
//!         // answer itself, with `pk_cli_http::inconclusive` for what says
//!         // nothing about the session: a 429, a 5xx, a 403 with an HTML page.
//!         match me_with(header.expose()) {
//!             Me::Accepted => Confirm::Ready,
//!             Me::Refused => Confirm::NotYet("GET /me answered 401".into()),
//!             // The crate asks again after a backoff.
//!             Me::CouldNotTell(status, retry_after) => Confirm::Undecided {
//!                 reason: format!("GET /me answered HTTP {status}"),
//!                 retry_after,
//!             },
//!             Me::NoAnswer => Confirm::undecided("GET /me got no answer"),
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
//!   dir, and removed when the window closes, leaving the kept one alone;
//! - [`profile_exists`] and [`named_profile_exists`] say whether one is kept,
//!   for a CLI's `auth status`. They take no lock and create nothing, and
//!   they refuse what [`forget_profile`] refuses. A `true` says a profile is
//!   kept, not that anything is signed in there.
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
//! ## A provider that keeps tokens in site storage
//!
//! Some providers keep a token bundle, with a rotating refresh token in it,
//! in `localStorage`. A kept profile retains it. On the next launch the site's
//! own script can read the stale bundle and present the retired refresh token,
//! so reuse detection revokes the whole token family; a capture can read the
//! stale bundle; and the refresh token sits unencrypted on disk between runs.
//! Removing the bundle from the page as it is read does not solve this: the
//! removal may not reach disk before Chrome stops, and an expression can be
//! evaluated more than once, so a lost answer loses the bundle.
//!
//! A CLI whose provider keeps tokens in site storage turns on
//! [`Launch::clear_site_storage`]. The profile keeps its cookies, so the
//! provider's remembered device and single-sign-on session carry over, and
//! never its site storage. A provider whose session lives in cookies has no
//! need of it, and one that remembers the device in site storage sees an
//! unknown device each time. These are removed from the kept profile:
//!
//! | In `Default/` | What a page's script keeps there |
//! |---|---|
//! | `Local Storage` | `localStorage` |
//! | `Session Storage` | `sessionStorage` |
//! | `IndexedDB` | IndexedDB |
//! | `WebStorage` | storage buckets: each one's IndexedDB, Cache API and file system |
//! | `Service Worker` | the Cache API, and the service workers themselves |
//! | `File System` | the origin-private file system |
//! | `databases` | Web SQL, in a Chromium old enough to have it |
//!
//! They are removed twice:
//!
//! - at launch, once the run lock is held and no Chrome is running there,
//!   before Chrome starts. If that fails, the launch fails as
//!   [`BrowserError::Profile`] naming the path, and no Chrome starts;
//! - on close, once the [`Browser`] guard has stopped Chrome on drop, so a
//!   refresh token does not sit on disk between runs. This is best effort,
//!   with a warning on stderr for what would not go. A crash or a second Ctrl-C
//!   skips it, and the next launch's removal covers that. So does a Chrome
//!   still running in the profile once the process the guard spawned has ended
//!   (a wrapper that does not `exec` it, a relaunch to update): its storage
//!   stays, with a warning, and the next launch clears it once that window is
//!   closed.
//!
//! A symlink at one of those paths, or inside them, is removed as a link and
//! its target is never touched; a `Default` that is a symlink, is not a
//! directory or is another user's refuses the launch. The cookie store and the
//! rest of the profile stay, including the HTTP cache, which a page reads only
//! by asking for a URL again. A Chrome profile the owner adds in the window
//! keeps its own storage: only `Default` is cleared. A scratch profile is
//! unaffected.
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
//! [`Confirm::NotYet`] or [`Confirm::Undecided`] reason is shown too, so a
//! CLI writes it the same way.
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
//! `auth status` takes [`profile_exists`] through a seam of the same kind,
//! which tests fill with `profile_exists_in(root, bin)` or
//! `named_profile_exists_in(root, bin, name)`. `example-cli`'s logout and
//! status are the worked examples.
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
pub use profile::{forget_named_profile, forget_profile, named_profile_exists, profile_exists};
#[cfg(any(test, feature = "test-util"))]
pub use profile::{
    forget_named_profile_in, forget_profile_in, named_profile_exists_in, profile_exists_in,
};
pub use watch::{CapturedRequest, LoggedRequest, RequestKind};

/// The family's `auth login --browser` flags. Flatten them into the CLI's
/// login arguments beside `pk_cli_auth::LoginArgs`, so every portal CLI
/// spells and documents them the same way, and pass `fresh_profile` on to
/// [`Launch::fresh_profile`].
///
/// `--fresh-profile` without `--browser` is a usage error (exit 2), including
/// when the CLI marks `browser` as conflicting with its other credential
/// sources (`mut_arg("browser", |a| a.conflicts_with_all(["stdin"]))`). Clap
/// counts a `requires` as satisfied when the required argument conflicts with
/// one given, so `--fresh-profile --stdin` passes clap's own check; this type
/// makes the check again as it is read from the matches, and the CLI adds
/// nothing for `fresh_profile`.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BrowserArgs {
    /// `--browser`: sign in through a Chrome window and capture the session
    /// from it.
    pub browser: bool,

    /// `--fresh-profile`: sign in through a fresh browser profile, deleted
    /// afterwards, instead of the one this CLI keeps.
    pub fresh_profile: bool,
}

// The flags as clap declares and reads them, separate from `BrowserArgs` so
// the check on reading can be its own. Plain comments here: a doc comment on a
// flattened struct becomes the `about` of the command it is flattened into.
#[derive(clap::Args, Debug, Clone)]
#[group(id = "BrowserArgs")]
struct BrowserFlags {
    /// Sign in through a Chrome window and capture the session from it: the
    /// password and any code are entered on the provider's own page, never
    /// in this CLI.
    #[arg(long)]
    browser: bool,

    /// With --browser: sign in through a fresh browser profile, deleted
    /// afterwards, instead of the one this CLI keeps. The provider sees a
    /// new device (expect its full sign-in), and nothing of this sign-in
    /// stays on disk.
    #[arg(long, requires = "browser")]
    fresh_profile: bool,
}

impl TryFrom<BrowserFlags> for BrowserArgs {
    type Error = clap::Error;

    fn try_from(flags: BrowserFlags) -> Result<Self, clap::Error> {
        if flags.fresh_profile && !flags.browser {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--fresh-profile needs --browser",
            ));
        }
        Ok(BrowserArgs {
            browser: flags.browser,
            fresh_profile: flags.fresh_profile,
        })
    }
}

impl clap::FromArgMatches for BrowserArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        BrowserFlags::from_arg_matches(matches)?.try_into()
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        let mut flags = BrowserFlags {
            browser: self.browser,
            fresh_profile: self.fresh_profile,
        };
        flags.update_from_arg_matches(matches)?;
        *self = flags.try_into()?;
        Ok(())
    }
}

impl clap::Args for BrowserArgs {
    fn group_id() -> Option<clap::Id> {
        BrowserFlags::group_id()
    }

    fn augment_args(cmd: clap::Command) -> clap::Command {
        BrowserFlags::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        BrowserFlags::augment_args_for_update(cmd)
    }
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

    /// A CLI that marks `browser` as conflicting with another source, the way
    /// the family does. Clap counts a `requires` as satisfied when the required
    /// argument conflicts with one given, so `requires` alone lets
    /// `--fresh-profile --stdin` through with the flag ignored.
    #[derive(Parser, Debug)]
    #[command(mut_arg("browser", |a| a.conflicts_with_all(["stdin"])))]
    struct ConflictOnBrowser {
        #[command(flatten)]
        browser: BrowserArgs,
        #[arg(long)]
        stdin: bool,
    }

    /// The same conflict declared on the other argument.
    #[derive(Parser, Debug)]
    struct ConflictOnSource {
        #[command(flatten)]
        browser: BrowserArgs,
        #[arg(long, conflicts_with = "browser")]
        stdin: bool,
    }

    /// The form a CLI's `auth login` has: flattened in a subcommand.
    #[derive(Parser, Debug)]
    struct ConflictInSubcommand {
        #[command(subcommand)]
        command: Login,
    }

    #[derive(clap::Subcommand, Debug)]
    enum Login {
        Login(ConflictOnBrowserArgs),
    }

    #[derive(clap::Args, Debug)]
    #[command(mut_arg("browser", |a| a.conflicts_with_all(["stdin"])))]
    struct ConflictOnBrowserArgs {
        #[command(flatten)]
        browser: BrowserArgs,
        #[arg(long)]
        stdin: bool,
    }

    fn assert_fresh_profile_refused(result: Result<(), clap::Error>) {
        let err = result.expect_err("--fresh-profile without --browser is a usage error");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert_eq!(err.exit_code(), 2);
        let msg = err.to_string();
        assert!(msg.contains("--fresh-profile needs --browser"), "{msg}");
    }

    #[test]
    fn a_fresh_profile_needs_the_browser_flag_whatever_that_flag_conflicts_with() {
        assert_fresh_profile_refused(
            ConflictOnBrowser::try_parse_from(["x", "--fresh-profile", "--stdin"]).map(drop),
        );
        assert_fresh_profile_refused(
            ConflictOnSource::try_parse_from(["x", "--fresh-profile", "--stdin"]).map(drop),
        );
        assert_fresh_profile_refused(
            ConflictInSubcommand::try_parse_from(["x", "login", "--fresh-profile", "--stdin"])
                .map(drop),
        );

        // Updating parsed arguments goes through the same check.
        let mut cli = ConflictOnBrowser::try_parse_from(["x", "--stdin"]).unwrap();
        assert_fresh_profile_refused(cli.try_update_from(["x", "--fresh-profile", "--stdin"]));
    }

    #[test]
    fn the_browser_flags_parse_as_before_beside_a_conflict() {
        let stdin = ConflictOnBrowser::try_parse_from(["x", "--stdin"]).unwrap();
        assert!(stdin.stdin && !stdin.browser.browser && !stdin.browser.fresh_profile);
        let none = ConflictOnBrowser::try_parse_from(["x"]).unwrap();
        assert!(!none.stdin && !none.browser.browser && !none.browser.fresh_profile);
        let both =
            ConflictOnBrowser::try_parse_from(["x", "--browser", "--fresh-profile"]).unwrap();
        assert!(both.browser.browser && both.browser.fresh_profile);
        let plain = ConflictOnBrowser::try_parse_from(["x", "--browser"]).unwrap();
        assert!(plain.browser.browser && !plain.browser.fresh_profile);

        // The CLI's own conflict still stands, and alone the flag is clap's
        // own refusal.
        let err = ConflictOnBrowser::try_parse_from(["x", "--browser", "--stdin"])
            .err()
            .unwrap();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let err = ConflictOnBrowser::try_parse_from(["x", "--fresh-profile"])
            .err()
            .unwrap();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// `BrowserArgs` reads as the flags it declares: the ids a CLI's `mut_arg`
    /// names, the group a derive names, and no `about` of its own.
    #[test]
    fn the_flags_keep_their_ids_and_group_and_add_no_about() {
        use clap::Args;
        let cmd = BrowserArgs::augment_args(clap::Command::new("x"));
        let ids: Vec<&str> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
        assert_eq!(ids, ["browser", "fresh_profile"]);
        assert_eq!(
            BrowserArgs::group_id().as_ref().map(clap::Id::as_str),
            Some("BrowserArgs")
        );
        assert_eq!(cmd.get_about().map(ToString::to_string), None);
    }
}
