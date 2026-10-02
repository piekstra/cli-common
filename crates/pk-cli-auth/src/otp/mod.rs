//! Logging in with a one-time code, sent by email or text message.
//!
//! `insp` logs in with a code sent by email, and `rpmfl`, `pmac`, `sofi`,
//! `robinhood` and `m1f` with a code sent by text message. Each carries its
//! own copy of the same loop: ask the provider for a code, park the in-flight
//! session so a later `--code` can finish it, and either prompt for the code
//! or tell the caller how to resume. [`OtpLogin`] is that loop once, plus one
//! step the copies leave to an agent: reading the code out of the mailbox
//! ([`GroMailbox`]), so an email-code login finishes unattended. Without a
//! mailbox the flow serves a text-message code the same way.
//!
//! ```no_run
//! # use pk_cli_core::CliError;
//! # use pk_cli_auth::otp::*;
//! # struct Portal;
//! # impl OtpTransport for Portal {
//! #     type Session = String;
//! #     fn request_code(&self) -> Result<CodeSent, CliError> { Ok(CodeSent::new("jar")) }
//! #     fn redeem(&self, _: &str, _: &OtpCode) -> Result<String, CliError> { Ok("jar".into()) }
//! # }
//! # let (portal, code_args, interactive, quiet) = (Portal, CodeArgs::default(), false, false);
//! // In `auth login`, before any keychain read:
//! let code = code_args.resolve()?;
//! let store = pk_cli_secrets::CredentialStore::for_binary("demo");
//! let slot = KeychainSlot::new(&store, "pending-login");
//! let gro = GroMailbox::new();
//! let session = OtpLogin::new("demo", &portal, &slot)
//!     .mailbox(&gro, MailQuery::new("from:no-reply@example.com")?)
//!     .interactive(interactive)
//!     .quiet(quiet)
//!     .login(code.as_ref())?;
//! // Store `session`; exit 0.
//! # Ok::<(), CliError>(())
//! ```
//!
//! # What the CLI supplies
//!
//! - An [`OtpTransport`]: the provider calls. `request_code` performs whatever
//!   the provider needs up to "a code was sent" and returns the in-flight
//!   session; `redeem` presents a code against that session.
//! - A [`ParkingSlot`]: where the parked login lives. [`KeychainSlot`] is one
//!   keychain item; a CLI with a one-item credential bundle implements the
//!   trait over its bundle instead.
//! - Optionally a [`Mailbox`] and the [`MailQuery`] naming the provider's
//!   code email.
//!
//! # The flow
//!
//! `auth login` resolves [`CodeArgs`] first, so a malformed `--code` is exit 2
//! before any keychain read. Then [`OtpLogin::login`]:
//!
//! - **with a code**, resumes the parked login: no parked login, or one older
//!   than the window, is exit 3; otherwise the code is redeemed against the
//!   parked session.
//! - **without one**, requests a code and parks the session *before* waiting
//!   on anything, so an interrupted run can still be finished with `--code`.
//!   Then the code is read from the mailbox (when one is configured), else
//!   prompted for (on a TTY), else the call ends with exit 3 and a message
//!   naming `<bin> auth login --code <CODE>`.
//!
//! A rejected code (the transport returns [`CliError::Auth`]) keeps the
//! login parked, so a mistyped code can be retyped without burning a new one,
//! up to [`DEFAULT_MAX_REJECTED`] rejections, after which it is discarded. A
//! non-auth failure (the provider is down) neither counts nor discards. The
//! parked login is cleared on success.
//!
//! # Exit codes
//!
//! Every way the login ends unauthenticated is [`CliError::Auth`], exit 3,
//! including "a code was sent; finish with `--code`": a driver that branches
//! on exit codes must not read a half-finished login as success. A malformed
//! code is exit 2. Transport and keychain errors pass through as they are.

mod code;
pub mod mailbox;
mod park;

use std::time::Duration;

use pk_cli_core::CliError;

pub use code::{extract_code, CodeArgs, CodeShape, OtpCode};
pub use mailbox::{GroMailbox, MailQuery, Mailbox};
pub use park::{
    check_slot_contract, KeychainSlot, MemorySlot, ParkedLogin, ParkingSlot, DEFAULT_MAX_AGE_SECS,
    DEFAULT_MAX_REJECTED,
};

/// What the provider said when it sent a code.
pub struct CodeSent {
    /// The in-flight session the code is bound to, serialized however the CLI
    /// likes. Parked in the keychain; never logged.
    pub in_flight: String,
    /// The channel, when the provider offers more than one (`email`).
    pub channel: Option<String>,
    /// A masked destination the provider reported (`j•••@example.com`).
    pub sent_to_hint: Option<String>,
}

impl CodeSent {
    pub fn new(in_flight: impl Into<String>) -> Self {
        CodeSent {
            in_flight: in_flight.into(),
            channel: None,
            sent_to_hint: None,
        }
    }
}

/// The provider side of the login, supplied by each CLI.
pub trait OtpTransport {
    /// What a completed login yields: a cookie jar, a token bundle. The CLI
    /// stores it.
    type Session;

    /// Do whatever the provider needs up to and including "send the code".
    fn request_code(&self) -> Result<CodeSent, CliError>;

    /// Present `code` against the parked `in_flight` session. Return
    /// [`CliError::Auth`] when the provider rejects the code, so the flow can
    /// count it; any other error is passed through uncounted.
    fn redeem(&self, in_flight: &str, code: &OtpCode) -> Result<Self::Session, CliError>;
}

/// Time, as a seam: the parked login's age and the mailbox polling read it,
/// and tests must not sleep.
pub trait Clock {
    fn now_unix(&self) -> u64;
    fn sleep(&self, d: Duration);
}

/// The wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Asking a person for the code, as a seam.
pub trait CodePrompt {
    /// Show `label` and return what was typed.
    fn read_code(&self, label: &str) -> Result<String, CliError>;
}

/// A visible prompt on stderr, answered on stdin. Visible because the code is
/// single-use and the user has to check what they typed against the email.
pub struct TtyPrompt;

impl CodePrompt for TtyPrompt {
    fn read_code(&self, label: &str) -> Result<String, CliError> {
        use std::io::{BufRead, Write};
        eprint!("{label}: ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| CliError::Other(format!("reading the code: {e}")))?;
        Ok(line)
    }
}

/// How long to keep looking in the mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poll {
    /// Looks, each after waiting `interval`.
    pub attempts: u32,
    pub interval: Duration,
}

impl Default for Poll {
    /// Nine looks ten seconds apart: 90 seconds, past the delivery time of
    /// every code email observed, well inside a code's lifetime.
    fn default() -> Self {
        Poll {
            attempts: 9,
            interval: Duration::from_secs(10),
        }
    }
}

static SYSTEM_CLOCK: SystemClock = SystemClock;
static TTY_PROMPT: TtyPrompt = TtyPrompt;

/// The email-OTP login. Build one per `auth login`; see the module docs.
pub struct OtpLogin<'a, T: OtpTransport> {
    bin: &'a str,
    transport: &'a T,
    slot: &'a dyn ParkingSlot,
    mailbox: Option<(&'a dyn Mailbox, MailQuery)>,
    poll: Poll,
    interactive: bool,
    quiet: bool,
    max_age_secs: u64,
    max_rejected: u32,
    clock: &'a dyn Clock,
    prompt: &'a dyn CodePrompt,
}

impl<'a, T: OtpTransport> OtpLogin<'a, T> {
    /// `bin` names the CLI in resume hints (`<bin> auth login --code`).
    /// Non-interactive and chatty by default; set [`interactive`] from
    /// `CommonArgs::interactive()` and [`quiet`] from `--quiet`.
    ///
    /// [`interactive`]: OtpLogin::interactive
    /// [`quiet`]: OtpLogin::quiet
    pub fn new(bin: &'a str, transport: &'a T, slot: &'a dyn ParkingSlot) -> Self {
        OtpLogin {
            bin,
            transport,
            slot,
            mailbox: None,
            poll: Poll::default(),
            interactive: false,
            quiet: false,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            max_rejected: DEFAULT_MAX_REJECTED,
            clock: &SYSTEM_CLOCK,
            prompt: &TTY_PROMPT,
        }
    }

    /// Read the code from `mailbox`, looking for mail matching `query`.
    pub fn mailbox(mut self, mailbox: &'a dyn Mailbox, query: MailQuery) -> Self {
        self.mailbox = Some((mailbox, query));
        self
    }

    pub fn poll(mut self, poll: Poll) -> Self {
        self.poll = poll;
        self
    }

    /// Whether a prompt is possible (a TTY, not `--json`).
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Suppress the progress lines on stderr.
    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    /// How long a parked login stays resumable.
    pub fn max_age_secs(mut self, secs: u64) -> Self {
        self.max_age_secs = secs;
        self
    }

    /// Rejected codes a parked login survives (at least 1).
    pub fn max_rejected(mut self, n: u32) -> Self {
        self.max_rejected = n.max(1);
        self
    }

    pub fn clock(mut self, clock: &'a dyn Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn prompt(mut self, prompt: &'a dyn CodePrompt) -> Self {
        self.prompt = prompt;
        self
    }

    /// `auth login`: resume with `code` when one was given, else start.
    pub fn login(&self, code: Option<&OtpCode>) -> Result<T::Session, CliError> {
        match code {
            Some(code) => self.resume(code),
            None => self.start(),
        }
    }

    /// Request a code, park the session, then read, prompt for, or hand back
    /// the resume for the code.
    pub fn start(&self) -> Result<T::Session, CliError> {
        // Taken before the request, so the mailbox search can never match a
        // code email sent before this one.
        let requested_at = self.clock.now_unix();
        let sent = self.transport.request_code()?;
        let mut parked = ParkedLogin::new(sent.in_flight, requested_at);
        parked.channel = sent.channel;
        parked.sent_to_hint = sent.sent_to_hint;
        self.slot.save(&parked)?;
        let where_to = parked
            .sent_to_hint
            .clone()
            .unwrap_or_else(|| "the address on file".into());
        self.note(&format!("a verification code was sent to {where_to}"));

        if let Some((mailbox, query)) = &self.mailbox {
            match self.watch(*mailbox, query, requested_at) {
                Ok(Some(code)) => match self.redeem(parked, &code) {
                    Ok(session) => return Ok(session),
                    Err((err, Some(still))) if matches!(err, CliError::Auth(_)) => {
                        self.note(&format!(
                            "the code read from the mailbox was refused: {err}"
                        ));
                        parked = still;
                    }
                    Err((err, _)) => return Err(err),
                },
                Ok(None) => self.note(&format!(
                    "no code arrived in the mailbox within {}s",
                    self.poll.interval.as_secs() * u64::from(self.poll.attempts)
                )),
                Err(err) => self.note(&format!("could not read the mailbox: {err}")),
            }
        }

        if self.interactive {
            loop {
                let raw = self.prompt.read_code("Verification code")?;
                let code = OtpCode::parse(&raw)?;
                match self.redeem(parked, &code) {
                    Ok(session) => return Ok(session),
                    Err((err, Some(still))) if matches!(err, CliError::Auth(_)) => {
                        self.note(&err.to_string());
                        parked = still;
                    }
                    Err((err, _)) => return Err(err),
                }
            }
        }

        Err(CliError::Auth(format!(
            "a verification code was sent to {where_to} — finish with \
             `{bin} auth login --code <CODE>` within {mins} minutes",
            bin = self.bin,
            mins = self.max_age_secs / 60
        )))
    }

    /// `auth login --code`: finish the parked login with `code`.
    pub fn resume(&self, code: &OtpCode) -> Result<T::Session, CliError> {
        let parked = self.slot.load()?.ok_or_else(|| {
            CliError::Auth(format!(
                "no login is waiting for a code — run `{} auth login` first",
                self.bin
            ))
        })?;
        let now = self.clock.now_unix();
        if parked.is_stale(now, self.max_age_secs) {
            // Spent either way: leaving it would fail the next `--code` the
            // same way.
            self.slot.clear()?;
            // Blame the wait, not the code: the provider would reject the
            // session, which reads as "invalid code" and sends the user to
            // re-check a code that was never the problem.
            return Err(CliError::Auth(format!(
                "the login waiting for a code was started {} minutes ago and has expired — \
                 run `{} auth login` to request a new code",
                parked.age_secs(now) / 60,
                self.bin
            )));
        }
        self.redeem(parked, code).map_err(|(err, _)| err)
    }

    /// One redeem against a parked login. On failure, also returns the login
    /// as it now stands in the slot (`None` once it is discarded).
    fn redeem(
        &self,
        mut parked: ParkedLogin,
        code: &OtpCode,
    ) -> Result<T::Session, (CliError, Option<ParkedLogin>)> {
        match self.transport.redeem(&parked.session, code) {
            Ok(session) => {
                // The code is spent and the session is in hand: a failure to
                // clear the parked login must not throw the session away. A
                // leftover parked login only ever fails a later `--code`.
                if let Err(err) = self.slot.clear() {
                    self.note(&format!("could not clear the parked login: {err}"));
                }
                Ok(session)
            }
            Err(CliError::Auth(msg)) => {
                parked.rejected += 1;
                if parked.rejected >= self.max_rejected {
                    if let Err(err) = self.slot.clear() {
                        return Err((err, None));
                    }
                    return Err((
                        CliError::Auth(format!(
                            "{msg} — {} codes refused; the waiting login was discarded, \
                             run `{} auth login` to request a new code",
                            parked.rejected, self.bin
                        )),
                        None,
                    ));
                }
                if let Err(err) = self.slot.save(&parked) {
                    return Err((err, None));
                }
                let left = self.max_rejected - parked.rejected;
                Err((
                    CliError::Auth(format!(
                        "{msg} — {left} more attempt{s} before the waiting login is discarded; \
                         retry with `{bin} auth login --code <CODE>`",
                        s = if left == 1 { "" } else { "s" },
                        bin = self.bin
                    )),
                    Some(parked),
                ))
            }
            Err(other) => Err((other, Some(parked))),
        }
    }

    /// Look in the mailbox up to `poll.attempts` times. Stops at the first
    /// error: the mailbox reader may be blocked on a keychain approval, and
    /// another call would raise another dialog.
    fn watch(
        &self,
        mailbox: &dyn Mailbox,
        query: &MailQuery,
        since: u64,
    ) -> Result<Option<OtpCode>, CliError> {
        self.note(&format!(
            "watching the mailbox for it (up to {}s)",
            self.poll.interval.as_secs() * u64::from(self.poll.attempts)
        ));
        for _ in 0..self.poll.attempts {
            self.clock.sleep(self.poll.interval);
            if let Some(code) = mailbox.find_code(query, since)? {
                return Ok(Some(code));
            }
        }
        Ok(None)
    }

    fn note(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }
}

#[cfg(test)]
mod tests;
