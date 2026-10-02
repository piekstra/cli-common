//! The flow against a fake provider, a fake mailbox, a fake clock and a fake
//! prompt. Offline: no keychain, no network, no sleeping.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use super::*;

const T0: u64 = 1_800_000_000;
const BIN: &str = "demo";

/// A provider that issues the code `good` against the session `sess-<n>`,
/// where `n` counts the requests.
struct FakeProvider {
    good: &'static str,
    requests: Cell<u32>,
    redeemed: RefCell<Vec<(String, String)>>,
    /// Fail redeems with this instead of judging the code.
    outage: Cell<bool>,
    refuse_request: Cell<bool>,
}

impl FakeProvider {
    fn new(good: &'static str) -> Self {
        FakeProvider {
            good,
            requests: Cell::new(0),
            redeemed: RefCell::new(Vec::new()),
            outage: Cell::new(false),
            refuse_request: Cell::new(false),
        }
    }
}

impl OtpTransport for FakeProvider {
    type Session = String;

    fn request_code(&self) -> Result<CodeSent, CliError> {
        if self.refuse_request.get() {
            return Err(CliError::Upstream("portal down".into()));
        }
        self.requests.set(self.requests.get() + 1);
        let mut sent = CodeSent::new(format!("sess-{}", self.requests.get()));
        sent.channel = Some("email".into());
        sent.sent_to_hint = Some("u•••@example.com".into());
        Ok(sent)
    }

    fn redeem(&self, in_flight: &str, code: &OtpCode) -> Result<String, CliError> {
        self.redeemed
            .borrow_mut()
            .push((in_flight.to_string(), code.expose().to_string()));
        if self.outage.get() {
            return Err(CliError::Upstream("portal down".into()));
        }
        if code.expose() == self.good {
            Ok(format!("session-for-{in_flight}"))
        } else {
            Err(CliError::Auth("the portal refused that code".into()))
        }
    }
}

/// A clock that only moves when slept on.
struct FakeClock {
    now: Cell<u64>,
    slept: Cell<u32>,
}

impl FakeClock {
    fn at(now: u64) -> Self {
        FakeClock {
            now: Cell::new(now),
            slept: Cell::new(0),
        }
    }
}

impl Clock for FakeClock {
    fn now_unix(&self) -> u64 {
        self.now.get()
    }
    fn sleep(&self, d: Duration) {
        self.slept.set(self.slept.get() + 1);
        self.now.set(self.now.get() + d.as_secs());
    }
}

/// A mailbox whose code arrives on look number `arrives_on` (1-based).
struct FakeMailbox {
    code: &'static str,
    arrives_on: u32,
    looks: Cell<u32>,
    since_seen: Cell<u64>,
    broken: bool,
}

impl FakeMailbox {
    fn arriving(code: &'static str, arrives_on: u32) -> Self {
        FakeMailbox {
            code,
            arrives_on,
            looks: Cell::new(0),
            since_seen: Cell::new(0),
            broken: false,
        }
    }
    fn broken() -> Self {
        FakeMailbox {
            broken: true,
            ..FakeMailbox::arriving("000000", 1)
        }
    }
}

impl Mailbox for FakeMailbox {
    fn find_code(&self, _q: &MailQuery, since: u64) -> Result<Option<OtpCode>, CliError> {
        self.looks.set(self.looks.get() + 1);
        self.since_seen.set(since);
        if self.broken {
            return Err(CliError::Upstream("`gro` did not answer".into()));
        }
        if self.looks.get() >= self.arrives_on {
            Ok(Some(OtpCode::parse(self.code).unwrap()))
        } else {
            Ok(None)
        }
    }
}

/// Answers prompts from a script; panics if asked more than it has.
struct Script(RefCell<Vec<&'static str>>);

impl Script {
    fn of(lines: &[&'static str]) -> Self {
        Script(RefCell::new(lines.to_vec()))
    }
    fn none() -> Self {
        Script::of(&[])
    }
}

impl CodePrompt for Script {
    fn read_code(&self, _label: &str) -> Result<String, CliError> {
        let mut lines = self.0.borrow_mut();
        assert!(!lines.is_empty(), "prompted when no prompt was expected");
        Ok(lines.remove(0).to_string())
    }
}

fn query() -> MailQuery {
    MailQuery::new("from:no-reply@example.com").unwrap()
}

fn code(s: &str) -> OtpCode {
    OtpCode::parse(s).unwrap()
}

#[test]
fn the_mailbox_finishes_the_login_unattended() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let mailbox = FakeMailbox::arriving("482917", 3);
    let prompt = Script::none();
    let session = OtpLogin::new(BIN, &provider, &slot)
        .mailbox(&mailbox, query())
        .clock(&clock)
        .prompt(&prompt)
        .quiet(true)
        .login(None)
        .unwrap();
    assert_eq!(session, "session-for-sess-1");
    assert_eq!(mailbox.looks.get(), 3, "stops looking once the code is in");
    assert_eq!(
        mailbox.since_seen.get(),
        T0,
        "only mail after the request counts"
    );
    assert_eq!(slot.peek(), None, "a finished login leaves nothing parked");
}

#[test]
fn without_a_tty_or_mailbox_the_login_parks_and_exits_3_with_the_resume() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let prompt = Script::none();
    let err = OtpLogin::new(BIN, &provider, &slot)
        .clock(&clock)
        .prompt(&prompt)
        .quiet(true)
        .login(None)
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("demo auth login --code <CODE>"), "{msg}");
    assert!(msg.contains("u•••@example.com"), "{msg}");
    let parked = slot.peek().expect("parked for the resume");
    assert_eq!(parked.session, "sess-1");
    assert_eq!(parked.issued_at, T0);
    assert_eq!(parked.channel.as_deref(), Some("email"));
    assert!(provider.redeemed.borrow().is_empty());
}

#[test]
fn the_resume_redeems_against_the_parked_session_not_a_new_one() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let otp = OtpLogin::new(BIN, &provider, &slot)
        .clock(&clock)
        .quiet(true);
    otp.login(None).unwrap_err();
    clock.now.set(T0 + 120);
    let session = otp.login(Some(&code("482917"))).unwrap();
    assert_eq!(session, "session-for-sess-1");
    assert_eq!(
        provider.requests.get(),
        1,
        "the resume requests no new code"
    );
    assert_eq!(slot.peek(), None);
}

#[test]
fn a_tty_prompt_finishes_the_login_when_the_mailbox_has_nothing() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let mailbox = FakeMailbox::arriving("482917", 99);
    let prompt = Script::of(&["482 917\n"]);
    let session = OtpLogin::new(BIN, &provider, &slot)
        .mailbox(&mailbox, query())
        .poll(Poll {
            attempts: 2,
            interval: Duration::from_secs(5),
        })
        .interactive(true)
        .clock(&clock)
        .prompt(&prompt)
        .quiet(true)
        .login(None)
        .unwrap();
    assert_eq!(session, "session-for-sess-1");
    assert_eq!(mailbox.looks.get(), 2, "bounded by the poll");
    assert_eq!(clock.slept.get(), 2);
}

/// The GUI-dialog rail: a mailbox reader that fails may be blocked on a
/// keychain approval, so it is asked exactly once.
#[test]
fn a_failing_mailbox_is_read_once_then_the_flow_falls_back() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let mailbox = FakeMailbox::broken();
    let prompt = Script::none();
    let err = OtpLogin::new(BIN, &provider, &slot)
        .mailbox(&mailbox, query())
        .clock(&clock)
        .prompt(&prompt)
        .quiet(true)
        .login(None)
        .unwrap_err();
    assert_eq!(mailbox.looks.get(), 1);
    assert_eq!(err.exit_code(), 3, "parked for --code, not a mailbox error");
    assert!(slot.peek().is_some());
}

#[test]
fn a_refused_mailbox_code_falls_back_to_the_prompt_on_the_same_session() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let mailbox = FakeMailbox::arriving("111111", 1);
    let prompt = Script::of(&["482917"]);
    let session = OtpLogin::new(BIN, &provider, &slot)
        .mailbox(&mailbox, query())
        .interactive(true)
        .clock(&clock)
        .prompt(&prompt)
        .quiet(true)
        .login(None)
        .unwrap();
    assert_eq!(session, "session-for-sess-1");
    let redeemed = provider.redeemed.borrow();
    assert_eq!(redeemed.len(), 2);
    assert!(redeemed.iter().all(|(s, _)| s == "sess-1"));
}

#[test]
fn a_rejected_code_keeps_the_login_parked_and_counts() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::holding(ParkedLogin::new("sess-9", T0));
    let clock = FakeClock::at(T0 + 60);
    let otp = OtpLogin::new(BIN, &provider, &slot)
        .clock(&clock)
        .quiet(true);
    let err = otp.resume(&code("000000")).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert!(err.to_string().contains("2 more attempts"), "{err}");
    assert_eq!(slot.peek().unwrap().rejected, 1);
    // The right code still works against the same session.
    assert_eq!(otp.resume(&code("482917")).unwrap(), "session-for-sess-9");
}

#[test]
fn the_rejection_cap_discards_the_parked_login() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::holding(ParkedLogin::new("sess-9", T0));
    let clock = FakeClock::at(T0);
    let otp = OtpLogin::new(BIN, &provider, &slot)
        .clock(&clock)
        .quiet(true)
        .max_rejected(2);
    otp.resume(&code("000000")).unwrap_err();
    let err = otp.resume(&code("000001")).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert!(err.to_string().contains("discarded"), "{err}");
    assert_eq!(slot.peek(), None);
    // And the next --code says there is nothing to resume.
    let err = otp.resume(&code("482917")).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert!(err.to_string().contains("no login is waiting"), "{err}");
}

#[test]
fn a_provider_outage_neither_counts_nor_discards() {
    let provider = FakeProvider::new("482917");
    provider.outage.set(true);
    let slot = MemorySlot::holding(ParkedLogin::new("sess-9", T0));
    let clock = FakeClock::at(T0);
    let err = OtpLogin::new(BIN, &provider, &slot)
        .clock(&clock)
        .quiet(true)
        .resume(&code("482917"))
        .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert_eq!(slot.peek().unwrap().rejected, 0);
}

#[test]
fn a_stale_parked_login_blames_the_wait_and_is_spent() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::holding(ParkedLogin::new("sess-9", T0));
    let clock = FakeClock::at(T0 + 25 * 60);
    let err = OtpLogin::new(BIN, &provider, &slot)
        .clock(&clock)
        .quiet(true)
        .resume(&code("482917"))
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let msg = err.to_string();
    assert!(msg.contains("25 minutes ago"), "{msg}");
    assert!(!msg.contains("refused"), "{msg}");
    assert_eq!(slot.peek(), None);
    assert!(
        provider.redeemed.borrow().is_empty(),
        "no redeem on a dead session"
    );
}

#[test]
fn resuming_with_nothing_parked_is_exit_3() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let err = OtpLogin::new(BIN, &provider, &slot)
        .quiet(true)
        .resume(&code("482917"))
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert!(err.to_string().contains("demo auth login"), "{err}");
}

/// Validation before the keychain: a malformed `--code` is rejected by
/// `CodeArgs::resolve`, so the flow (and its slot) is never reached.
#[test]
fn a_malformed_code_never_touches_the_slot() {
    let slot = MemorySlot::new();
    let args = CodeArgs {
        code: Some("12;rm".into()),
    };
    let err = args.resolve().unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert_eq!(slot.touches(), 0);
}

#[test]
fn a_failed_request_parks_nothing() {
    let provider = FakeProvider::new("482917");
    provider.refuse_request.set(true);
    let slot = MemorySlot::new();
    let err = OtpLogin::new(BIN, &provider, &slot)
        .quiet(true)
        .login(None)
        .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert_eq!(slot.touches(), 0);
}

#[test]
fn a_mistyped_prompt_answer_is_exit_2_and_the_login_stays_parked() {
    let provider = FakeProvider::new("482917");
    let slot = MemorySlot::new();
    let clock = FakeClock::at(T0);
    let prompt = Script::of(&["oops!"]);
    let err = OtpLogin::new(BIN, &provider, &slot)
        .interactive(true)
        .clock(&clock)
        .prompt(&prompt)
        .quiet(true)
        .login(None)
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(slot.peek().is_some(), "resumable with --code");
    assert!(provider.redeemed.borrow().is_empty());
}
