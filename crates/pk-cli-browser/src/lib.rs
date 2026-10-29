//! Browser sign-in for the piekstra CLI family (DESIGN.md §2.2): the
//! mechanism behind `<bin> auth login --browser`.
//!
//! Some providers cannot be signed in to by a CLI: a bot check (Cloudflare
//! Turnstile, reCAPTCHA, Akamai) or an OAuth single-page app stands in front
//! of the password. The owner can still sign in on the provider's own page,
//! in a real browser, and the CLI can read the resulting web session out of
//! that browser. This crate is that: it launches Chrome with a throwaway
//! profile and a DevTools debug port, waits while the owner signs in
//! (password and MFA happen on the provider's page, never through the CLI),
//! reads the session the web app keeps (a `localStorage` entry, a cookie,
//! `HttpOnly` cookies included) over the Chrome DevTools Protocol, closes the
//! tab at once, kills Chrome and deletes the profile.
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
//! # Why a plain Chrome
//!
//! Chrome launched normally, with only a debug port and a scratch profile
//! and none of the flags a WebDriver or Playwright launch adds, reports
//! `navigator.webdriver == false` and looks like an ordinary browser to the
//! bot checks; the owner is the one who clicks. The crate never drives a
//! page. It lists targets and reads cookies at the browser level, which no
//! page can observe, and evaluates the capture's expression only in pages on
//! the capture's own hosts, never on the login host where the bot checks run.
//! It never enables a CDP domain in a page.
//!
//! # Trust boundary
//!
//! The debug port has no authentication: anything that can reach
//! `127.0.0.1:<port>`, or read `DevToolsActivePort`, can read every cookie
//! in the window while it is open. The scratch profile, which holds the
//! port file and the live session, is therefore created `0700`, exclusively,
//! under an unpredictable name, and removed on every exit path, Ctrl-C
//! included ([`Launch::ctrl_c`]).
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
//! A captured value is a [`pk_cli_secrets::Secret`]: it never prints through
//! `Debug` or `Display` and is zeroized when dropped. The crate never logs
//! one, never passes one on a command line, and never writes one to disk
//! (Chrome's own profile, deleted at the end, is the only place a session
//! touches the filesystem). Status lines and errors name hosts, labels and
//! cookie names only.

mod capture;
mod cdp;
mod chrome;
mod error;
mod host;
mod interrupt;

pub use capture::{capture, Capture, Captured, DEFAULT_TIMEOUT};
pub use chrome::{Browser, Launch};
pub use error::BrowserError;
pub use host::HostRule;
pub use interrupt::cancel;

/// The family's `auth login --browser` flag. Flatten it into the CLI's login
/// arguments beside `pk_cli_auth::LoginArgs`, so every portal CLI spells and
/// documents it the same way.
#[derive(clap::Args, Debug, Default, Clone)]
#[non_exhaustive]
pub struct BrowserArgs {
    /// Sign in through a Chrome window and capture the session from it: the
    /// password and any code are entered on the provider's own page, never
    /// in this CLI.
    #[arg(long)]
    pub browser: bool,
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
}
