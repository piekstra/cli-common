//! A login parked mid-second-factor, and where it is kept.
//!
//! A verification code is minted against the session that requested it. A
//! fresh login presenting that code shows the provider a code its new session
//! never issued: it rejects it, the user assumes a typo, and the retry burns
//! another code. Providers rate-limit codes, so a few rounds of that end in a
//! lockout. So the in-flight session is parked before the code is waited on,
//! and `--code` resumes *that* session instead of authenticating again.

use std::cell::{Cell, RefCell};
use std::fmt;

use pk_cli_core::CliError;
use pk_cli_secrets::CredentialStore;
use serde::{Deserialize, Serialize};

/// How long a parked login stays resumable by default.
///
/// Codes are typically good for 5–15 minutes. The window is a little longer
/// than the longest plausible lifetime: it exists to tell "you waited too
/// long" apart from "that code is wrong", not to enforce the provider's
/// policy. The provider is the authority on whether a code is valid, and the
/// CLI must not reject one the provider would accept.
pub const DEFAULT_MAX_AGE_SECS: u64 = 20 * 60;

/// Rejected codes a parked login survives by default before it is discarded.
///
/// A rejected code keeps the login parked so a mistyped code can be retyped
/// without requesting (and burning) a new one. The cap stops a script that
/// keeps feeding wrong codes from walking the account into the provider's
/// lockout.
pub const DEFAULT_MAX_REJECTED: u32 = 3;

/// An in-flight login waiting on a verification code.
///
/// The field names match the parked login `insperity-cli` already stores, so
/// a CLI adopting this reads its existing item unchanged. `rejected` is new
/// and defaults to 0.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParkedLogin {
    /// The provider session that requested the code (cookie jar, token, or
    /// whatever the provider's handshake carries), opaque to this crate.
    pub session: String,
    /// Unix seconds when the code was requested.
    pub issued_at: u64,
    /// The channel the code was sent over (`email`), so a resume can tell the
    /// provider the same thing the request did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// A non-identifying hint the provider gave about where the code went
    /// (`j•••@example.com`). Never the full address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_to_hint: Option<String>,
    /// Codes the provider has rejected against this session so far.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rejected: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ParkedLogin {
    pub fn new(session: impl Into<String>, issued_at: u64) -> Self {
        ParkedLogin {
            session: session.into(),
            issued_at,
            channel: None,
            sent_to_hint: None,
            rejected: 0,
        }
    }

    /// Seconds since the code was requested. Saturating, so a clock that
    /// moved backwards reads as "just now" rather than underflowing.
    pub fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.issued_at)
    }

    pub fn is_stale(&self, now: u64, max_age_secs: u64) -> bool {
        self.age_secs(now) > max_age_secs
    }
}

impl fmt::Debug for ParkedLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParkedLogin")
            .field("session", &"***")
            .field("issued_at", &self.issued_at)
            .field("channel", &self.channel)
            .field("sent_to_hint", &self.sent_to_hint)
            .field("rejected", &self.rejected)
            .finish()
    }
}

/// Where a parked login lives between `auth login` and `auth login --code`.
///
/// [`KeychainSlot`] keeps it as its own keychain item. A CLI that keeps its
/// whole credential set in one item (DESIGN.md §1.7) implements this over
/// that item instead, so parking costs no extra keychain prompt.
///
/// The flow relies on these rules; [`check_slot_contract`] tests the ones
/// that can be checked generically, for a CLI to run against its own slot.
pub trait ParkingSlot {
    /// The parked login, or `Ok(None)` when nothing is parked. Absence is not
    /// an error: the flow turns `None` into "no login is waiting for a code"
    /// (exit 3), and an `Err` here would replace that with a keychain error.
    fn load(&self) -> Result<Option<ParkedLogin>, CliError>;
    /// Store `parked`, replacing whatever was parked. Called once when the
    /// code is requested and again after each refused code (with the new
    /// `rejected` count).
    fn save(&self, parked: &ParkedLogin) -> Result<(), CliError>;
    /// Remove the parked login, and nothing else. A bundle-backed slot must
    /// clear only its parked-login field: the flow calls this on every
    /// successful redeem, and the stored password and device trust in the
    /// same item must survive it. `Ok(())` when nothing is parked.
    fn clear(&self) -> Result<(), CliError>;
}

/// Exercise `slot` against the [`ParkingSlot`] rules, panicking on the first
/// broken one. For tests: run it against an in-memory form of a CLI's own
/// bundle-backed slot. Leaves the slot empty. Whether `clear` spares the
/// rest of the bundle cannot be checked from here; assert that in the CLI.
pub fn check_slot_contract(slot: &dyn ParkingSlot) {
    slot.clear().expect("clear on an empty slot is Ok");
    assert_eq!(
        slot.load().expect("load on an empty slot is Ok"),
        None,
        "an empty slot loads as None"
    );
    let first = ParkedLogin::new("contract-session-1", 1_000);
    slot.save(&first).expect("save");
    assert_eq!(slot.load().expect("load").as_ref(), Some(&first));
    let mut second = ParkedLogin::new("contract-session-2", 2_000);
    second.rejected = 2;
    second.channel = Some("email".into());
    slot.save(&second).expect("save over a parked login");
    assert_eq!(
        slot.load().expect("load").as_ref(),
        Some(&second),
        "save replaces the parked login whole"
    );
    slot.clear().expect("clear");
    assert_eq!(slot.load().expect("load"), None, "clear removes it");
    slot.clear().expect("clear is idempotent");
}

/// A parked login as one JSON keychain item under `account`.
pub struct KeychainSlot<'a> {
    store: &'a CredentialStore,
    account: &'a str,
}

impl<'a> KeychainSlot<'a> {
    pub fn new(store: &'a CredentialStore, account: &'a str) -> Self {
        KeychainSlot { store, account }
    }
}

impl ParkingSlot for KeychainSlot<'_> {
    fn load(&self) -> Result<Option<ParkedLogin>, CliError> {
        self.store.get_json(self.account)
    }
    fn save(&self, parked: &ParkedLogin) -> Result<(), CliError> {
        self.store.set_json(self.account, parked)
    }
    fn clear(&self) -> Result<(), CliError> {
        self.store.delete(self.account).map(|_| ())
    }
}

/// An in-memory slot, for tests (here and in the CLIs that adopt this): a
/// test that read the real keychain would prompt on every macOS run of an
/// ad-hoc-signed test binary. Counts every access so a test can assert that
/// a path never touched the slot.
#[derive(Default)]
pub struct MemorySlot {
    item: RefCell<Option<ParkedLogin>>,
    touches: Cell<usize>,
}

impl MemorySlot {
    pub fn new() -> Self {
        MemorySlot::default()
    }

    /// A slot already holding `parked`.
    pub fn holding(parked: ParkedLogin) -> Self {
        MemorySlot {
            item: RefCell::new(Some(parked)),
            touches: Cell::new(0),
        }
    }

    /// The current item, without counting as a touch.
    pub fn peek(&self) -> Option<ParkedLogin> {
        self.item.borrow().clone()
    }

    /// Loads, saves and clears so far.
    pub fn touches(&self) -> usize {
        self.touches.get()
    }

    fn touch(&self) {
        self.touches.set(self.touches.get() + 1);
    }
}

impl ParkingSlot for MemorySlot {
    fn load(&self) -> Result<Option<ParkedLogin>, CliError> {
        self.touch();
        Ok(self.item.borrow().clone())
    }
    fn save(&self, parked: &ParkedLogin) -> Result<(), CliError> {
        self.touch();
        *self.item.borrow_mut() = Some(parked.clone());
        Ok(())
    }
    fn clear(&self) -> Result<(), CliError> {
        self.touch();
        *self.item.borrow_mut() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    #[test]
    fn the_stored_shape_matches_the_insperity_layout() {
        // What insperity-cli parks today: no `rejected` field.
        let legacy = r#"{"session":"jar","issued_at":1800000000,"channel":"email","sent_to_hint":"j•••@example.com"}"#;
        let parked: ParkedLogin = serde_json::from_str(legacy).unwrap();
        assert_eq!(parked.rejected, 0);
        assert_eq!(parked.channel.as_deref(), Some("email"));
        // And a fresh park serializes back to the same keys.
        let back: serde_json::Value = serde_json::to_value(&parked).unwrap();
        assert_eq!(
            back,
            serde_json::from_str::<serde_json::Value>(legacy).unwrap()
        );
    }

    #[test]
    fn absent_optionals_stay_absent() {
        let v = serde_json::to_value(ParkedLogin::new("s", T0)).unwrap();
        assert!(v.get("channel").is_none());
        assert!(v.get("sent_to_hint").is_none());
        assert!(v.get("rejected").is_none());
    }

    #[test]
    fn staleness_is_measured_against_the_window() {
        let p = ParkedLogin::new("s", T0);
        assert!(!p.is_stale(T0 + DEFAULT_MAX_AGE_SECS, DEFAULT_MAX_AGE_SECS));
        assert!(p.is_stale(T0 + DEFAULT_MAX_AGE_SECS + 1, DEFAULT_MAX_AGE_SECS));
        assert_eq!(p.age_secs(T0 - 50), 0, "a backwards clock reads as now");
    }

    #[test]
    fn debug_never_prints_the_session() {
        let p = ParkedLogin::new("sid=secret-cookie", T0);
        assert!(!format!("{p:?}").contains("secret-cookie"));
    }

    #[test]
    fn the_memory_slot_meets_the_contract() {
        check_slot_contract(&MemorySlot::new());
    }

    #[test]
    fn the_memory_slot_counts_touches() {
        let slot = MemorySlot::new();
        assert_eq!(slot.load().unwrap(), None);
        slot.save(&ParkedLogin::new("s", T0)).unwrap();
        assert!(slot.peek().is_some());
        slot.clear().unwrap();
        assert_eq!(slot.touches(), 3);
        assert!(slot.peek().is_none());
    }
}
