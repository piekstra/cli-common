//! An item too long for the OS store, split across several and put back together on read.
//!
//! Windows's Credential Manager holds 2,560 bytes per credential, and `keyring` stores text
//! there as UTF-16, so an item is at most [`WINDOWS_ITEM_UNITS`] UTF-16 code units. A browser
//! session's tokens pass that: `keyring` answers `TooLong` and the sign-in fails at its last
//! step. On Windows, then, a longer value becomes parts, under its account's own name plus a
//! suffix, and a head item under the account itself that says where they are:
//!
//! ```text
//! session                    pk-split/1 b 3 90ab12cd34ef5678
//! session.pk-split-b-0       (the first 1,280 code units)
//! session.pk-split-b-1       (the next)
//! session.pk-split-b-2       (the rest)
//! ```
//!
//! - **A value that fits stays one plain item,** so what an older build stored still reads, and
//!   a small value costs no more than it did.
//! - **The head changes last.** A new value goes into the slot the head does not name, then the
//!   head moves to it, then the old slot is cleared. A failure or a crash partway leaves the old
//!   value readable. Two slots and no more, so a crash can only ever leave one old copy behind,
//!   which the next write clears. A part a failed delete left past a gap waits for
//!   `auth logout --forget`, which looks everywhere.
//! - **A read checks what it joined** against the head's hash, so a torn write reads as an error
//!   and is never handed to a provider as a credential. It reads twice, because a reader can
//!   meet a writer clearing the slot it was reading.
//! - **Two writers need no lock for this,** as one item did not. A write reads its parts back
//!   before pointing the head at them and once after, tries again if another write reached them,
//!   and clears a slot only while the head is still its own. The last write wins, as it does on
//!   one item. What remains is a clear already under way when another write starts into that
//!   slot, which reads as parts that do not belong together until the next sign-in. A CLI whose
//!   provider rotates holds `RefreshLock` around its writes anyway.
//!
//! macOS and Linux have no such limit, so there none of this runs and a call is the one
//! keychain operation it always was: an extra lookup there could mean an extra prompt.

use pk_cli_core::CliError;

use crate::{ItemStore, Secret};

/// What one Windows credential holds, as text: `CRED_MAX_CREDENTIAL_BLOB_SIZE`'s 2,560 bytes
/// at two bytes per UTF-16 code unit.
pub(crate) const WINDOWS_ITEM_UNITS: usize = 1_280;

/// The limit of this platform's store, in UTF-16 code units.
pub(crate) const ITEM_LIMIT: Option<usize> = if cfg!(windows) {
    Some(WINDOWS_ITEM_UNITS)
} else {
    None
};

/// How a head item opens. A value that opens this way is written as parts too, however short,
/// so a value can never be taken for a head.
const MARKER: &str = "pk-split/1 ";

/// The most parts one value may take, which also bounds what a damaged head can ask a read to
/// do: about 80 KB at Windows's limit, many times a browser session's tokens.
const MAX_PARTS: usize = 64;

/// How many times a write goes round when another write keeps reaching its parts.
const WRITE_TRIES: u32 = 4;

/// So two writers that met do not stay in step: each process waits its own time.
fn out_of_step() -> std::time::Duration {
    std::time::Duration::from_millis(20 + u64::from(std::process::id() % 80))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    A,
    B,
}

impl Slot {
    fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    fn letter(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    slot: Slot,
    parts: usize,
    hash: u64,
}

impl Head {
    fn text(self) -> String {
        format!(
            "{MARKER}{} {} {:016x}",
            self.slot.letter(),
            self.parts,
            self.hash
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut fields = text.strip_prefix(MARKER)?.split(' ');
        let slot = match fields.next()? {
            "a" => Slot::A,
            "b" => Slot::B,
            _ => return None,
        };
        let parts: usize = fields.next()?.parse().ok()?;
        let hash = u64::from_str_radix(fields.next()?, 16).ok()?;
        ((1..=MAX_PARTS).contains(&parts) && fields.next().is_none()).then_some(Self {
            slot,
            parts,
            hash,
        })
    }
}

/// FNV-1a. It catches a torn write and guards nothing: it sits beside the parts it hashes, in
/// the same store, so it tells a reader of that store nothing the parts do not.
fn hash_of(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn part_account(account: &str, slot: Slot, index: usize) -> String {
    format!("{account}.pk-split-{}-{index}", slot.letter())
}

/// `value` in pieces of at most `limit` UTF-16 code units, never through a surrogate pair.
fn pieces(value: &str, limit: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut units) = (0, 0);
    for (at, character) in value.char_indices() {
        if units + character.len_utf16() > limit {
            out.push(&value[start..at]);
            start = at;
            units = 0;
        }
        units += character.len_utf16();
    }
    out.push(&value[start..]);
    out
}

fn too_damaged(service: &str, account: &str, what: &str) -> CliError {
    CliError::Keychain(format!(
        "stored item `{account}` under `{service}` {what}; clear it with `auth logout --forget` \
         and log in again"
    ))
}

/// The head under `account`, if a head is what is there. A plain value, or nothing, is `None`.
fn head_of(item: Option<&Secret>) -> Result<Option<Head>, ()> {
    match item.map(Secret::expose) {
        Some(text) if text.starts_with(MARKER) => Head::parse(text).map(Some).ok_or(()),
        _ => Ok(None),
    }
}

/// After a write: a slot's parts from `from` until one is not there. A failure here leaves a
/// copy the next write replaces and a logout's [`clear_every_part`] finds, so it is not reported.
fn clear_run(store: &impl ItemStore, account: &str, slot: Slot, from: usize) {
    for index in from..MAX_PARTS {
        match store.delete(&part_account(account, slot, index)) {
            Ok(true) => {}
            Ok(false) | Err(_) => break,
        }
    }
}

/// At a logout: every place a part could be, since one a failed delete left has gaps around it.
/// A logout is rare and a Windows delete makes no prompt. Whether any went, and the first failure.
fn clear_every_part(store: &impl ItemStore, account: &str, slot: Slot) -> (bool, Option<CliError>) {
    let (mut removed, mut failure) = (false, None);
    for index in 0..MAX_PARTS {
        match store.delete(&part_account(account, slot, index)) {
            Ok(true) => removed = true,
            Ok(false) => {}
            Err(e) => {
                failure.get_or_insert(e);
            }
        }
    }
    (removed, failure)
}

pub(crate) fn get(
    store: &impl ItemStore,
    account: &str,
    limit: Option<usize>,
) -> Result<Option<Secret>, CliError> {
    if limit.is_none() {
        return store.get(account);
    }
    match joined(store, account) {
        Err(_) => joined(store, account),
        read => read,
    }
}

fn joined(store: &impl ItemStore, account: &str) -> Result<Option<Secret>, CliError> {
    let item = store.get(account)?;
    let head = head_of(item.as_ref())
        .map_err(|()| too_damaged(store.service(), account, "has a head that cannot be read"))?;
    match head {
        Some(head) => joined_from(store, account, head).map(Some),
        None => Ok(item),
    }
}

/// The value `head` names, if its parts are all there and belong together.
fn joined_from(store: &impl ItemStore, account: &str, head: Head) -> Result<Secret, CliError> {
    let service = store.service();
    let mut parts = Vec::with_capacity(head.parts);
    for index in 0..head.parts {
        let part = store
            .get(&part_account(account, head.slot, index))?
            .ok_or_else(|| too_damaged(service, account, "is missing a part"))?;
        parts.push(part);
    }
    // One allocation of the final size: growing a `String` would leave secret bytes behind in
    // the buffers it let go, which `Secret`'s zeroize never sees.
    let mut value = String::with_capacity(parts.iter().map(|p| p.expose().len()).sum());
    for part in &parts {
        value.push_str(part.expose());
    }
    let value = Secret::new(value);
    if hash_of(value.expose()) != head.hash {
        return Err(too_damaged(
            service,
            account,
            "has parts that do not belong together",
        ));
    }
    Ok(value)
}

pub(crate) fn set(
    store: &impl ItemStore,
    account: &str,
    secret: &Secret,
    limit: Option<usize>,
) -> Result<(), CliError> {
    let Some(limit) = limit else {
        return store.set(account, secret);
    };
    let value = secret.expose();
    let units: usize = value.chars().map(char::len_utf16).sum();
    if units <= limit && !value.starts_with(MARKER) {
        store.set(account, secret)?;
        // Nothing names the slots now, so nothing else will clear them, whatever was here before:
        // a head, one that cannot be read, or a plain value after a crash. Unless another write
        // has moved the head since, and one of them is its.
        if store.get(account)?.as_ref().map(Secret::expose) == Some(value) {
            clear_run(store, account, Slot::A, 0);
            clear_run(store, account, Slot::B, 0);
        }
        return Ok(());
    }
    let pieces = pieces(value, limit);
    if pieces.len() > MAX_PARTS {
        return Err(CliError::Keychain(format!(
            "item `{account}` under `{}` is too long to store here: {units} UTF-16 code \
             units, in {} parts of {limit}, past the {MAX_PARTS} kept",
            store.service(),
            pieces.len(),
        )));
    }
    let hash = hash_of(value);
    for attempt in 0..WRITE_TRIES {
        if attempt > 0 {
            std::thread::sleep(out_of_step());
        }
        // A head that cannot be read names no slot to keep: this write is what repairs it.
        let was = head_of(store.get(account)?.as_ref()).unwrap_or(None);
        let head = Head {
            slot: was.map_or(Slot::A, |head| head.slot.other()),
            parts: pieces.len(),
            hash,
        };
        for (index, piece) in pieces.iter().enumerate() {
            store.set(
                &part_account(account, head.slot, index),
                &Secret::new(piece.to_string()),
            )?;
        }
        // Another write in this slot has mixed its parts with these: point nobody at them.
        if joined_from(store, account, head).is_err() {
            continue;
        }
        let text = head.text();
        store.set(account, &Secret::new(text.as_str()))?;
        if store.get(account)?.as_ref().map(Secret::expose) != Some(text.as_str()) {
            // A later write has the head, and the slots are its to clear. The last write wins.
            return Ok(());
        }
        if joined_from(store, account, head).is_err() {
            continue;
        }
        // The value is stored. A copy left in the old slot is overwritten or cleared later, and
        // to fail here would tell a caller holding a rotated token that it was not saved.
        clear_run(store, account, head.slot, pieces.len());
        clear_run(store, account, head.slot.other(), 0);
        return Ok(());
    }
    Err(CliError::Keychain(format!(
        "item `{account}` under `{}` kept changing while this wrote it: another copy of this \
         command is writing it; run this again when that has finished",
        store.service()
    )))
}

pub(crate) fn delete(
    store: &impl ItemStore,
    account: &str,
    limit: Option<usize>,
) -> Result<bool, CliError> {
    if limit.is_none() {
        return store.delete(account);
    }
    // The head first, so a reader from here on finds nothing rather than a head with no parts.
    let mut removed = store.delete(account)?;
    let mut failure = None;
    // Both slots whatever the head said: it may be unreadable, or a crash may have left the other.
    for slot in [Slot::A, Slot::B] {
        let (went, failed) = clear_every_part(store, account, slot);
        removed |= went;
        failure = failure.or(failed);
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(removed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    const LIMIT: usize = WINDOWS_ITEM_UNITS;

    /// Windows's Credential Manager as `keyring` presents it: a value over the limit is refused,
    /// by UTF-16 code unit.
    #[derive(Default)]
    struct Fake {
        items: RefCell<BTreeMap<String, String>>,
        /// Refuse every write after this many, to stop a write partway.
        writes_left: Cell<Option<usize>>,
        /// Refuse a delete of this item.
        refuse_delete: RefCell<Option<String>>,
        /// The first read of this item finds nothing, and later ones find what is there.
        missing_once: RefCell<Option<String>>,
        /// Right after this item is written, another whole write of this value runs.
        then_write: RefCell<Option<(String, String)>>,
        reads: Cell<usize>,
    }

    impl Fake {
        fn raw(&self, account: &str) -> Option<String> {
            self.items.borrow().get(account).cloned()
        }
        fn accounts(&self) -> Vec<String> {
            self.items.borrow().keys().cloned().collect()
        }
    }

    impl ItemStore for Fake {
        fn service(&self) -> &str {
            "piekstra.test"
        }
        fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
            self.reads.set(self.reads.get() + 1);
            if self.missing_once.borrow().as_deref() == Some(account) {
                self.missing_once.replace(None);
                return Ok(None);
            }
            Ok(self.raw(account).map(Secret::new))
        }
        fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
            if let Some(left) = self.writes_left.get() {
                if left == 0 {
                    return Err(CliError::Keychain("simulated write failure".into()));
                }
                self.writes_left.set(Some(left - 1));
            }
            let units = secret.expose().encode_utf16().count();
            assert!(units <= LIMIT, "{account} is {units} units, past the limit");
            self.items
                .borrow_mut()
                .insert(account.into(), secret.expose().into());
            let meets = self
                .then_write
                .borrow()
                .as_ref()
                .is_some_and(|(after, _)| after == account);
            if meets {
                let (_, other) = self.then_write.take().unwrap();
                super::set(self, "session", &Secret::new(other), Some(LIMIT)).unwrap();
            }
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<bool, CliError> {
            if self.refuse_delete.borrow().as_deref() == Some(account) {
                return Err(CliError::Keychain("simulated delete failure".into()));
            }
            Ok(self.items.borrow_mut().remove(account).is_some())
        }
    }

    fn put(store: &Fake, value: &str) {
        set(store, "session", &Secret::new(value), Some(LIMIT)).unwrap();
    }

    fn read(store: &Fake) -> Option<String> {
        get(store, "session", Some(LIMIT))
            .unwrap()
            .map(|s| s.expose().to_string())
    }

    fn long(units: usize) -> String {
        (0..units)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect()
    }

    #[test]
    fn a_value_over_the_limit_round_trips_through_parts_each_within_it() {
        let store = Fake::default();
        let value = long(LIMIT * 3 + 7);
        put(&store, &value);
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
        let accounts = store.accounts();
        assert_eq!(accounts.len(), 5, "a head and four parts: {accounts:?}");
        assert!(store.raw("session").unwrap().starts_with(MARKER));
    }

    #[test]
    fn a_value_exactly_at_the_limit_stays_one_plain_item() {
        let store = Fake::default();
        let value = long(LIMIT);
        put(&store, &value);
        assert_eq!(store.accounts(), ["session"]);
        assert_eq!(store.raw("session").as_deref(), Some(value.as_str()));
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
    }

    #[test]
    fn one_unit_over_the_limit_takes_parts() {
        let store = Fake::default();
        let value = long(LIMIT + 1);
        put(&store, &value);
        assert_eq!(store.accounts().len(), 3);
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
    }

    /// What an older build stored, before any of this, must still read.
    #[test]
    fn a_plain_item_stored_by_an_older_build_reads_as_it_is() {
        let store = Fake::default();
        store
            .items
            .borrow_mut()
            .insert("session".into(), "{\"token\":\"dummy\"}".into());
        assert_eq!(read(&store).as_deref(), Some("{\"token\":\"dummy\"}"));
    }

    /// The limit is in UTF-16 code units and not bytes or chars: `keyring` encodes as UTF-16.
    /// A character outside the Basic Multilingual Plane takes two, which cannot be parted.
    #[test]
    fn characters_of_every_width_round_trip_and_no_pair_is_split() {
        let store = Fake::default();
        // U+1F600 takes two code units, and one is left over at each part's end when the
        // leading `x` puts the pairs out of step with the limit.
        let value = format!("x{}", "\u{1F600}".repeat(LIMIT));
        put(&store, &value);
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
        let value = "é".repeat(LIMIT * 2);
        put(&store, &value);
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
    }

    #[test]
    fn pieces_never_pass_the_limit_and_lose_nothing() {
        for value in [
            String::new(),
            "a".to_string(),
            long(10),
            long(11),
            "\u{1F600}".repeat(11),
            format!("a{}", "\u{1F600}".repeat(11)),
        ] {
            let pieces = pieces(&value, 10);
            assert_eq!(pieces.concat(), value);
            for piece in &pieces {
                assert!(piece.encode_utf16().count() <= 10, "{piece}");
            }
        }
    }

    #[test]
    fn a_shorter_value_replaces_a_longer_one_and_leaves_no_part_behind() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 4));
        put(&store, "short");
        assert_eq!(store.accounts(), ["session"]);
        assert_eq!(read(&store).as_deref(), Some("short"));
    }

    #[test]
    fn a_longer_value_replaces_a_shorter_one_in_either_slot() {
        let store = Fake::default();
        for units in [LIMIT * 2, LIMIT * 5, LIMIT + 1, LIMIT * 3] {
            let value = long(units);
            put(&store, &value);
            assert_eq!(read(&store).as_deref(), Some(value.as_str()));
            let parts = units.div_ceil(LIMIT);
            assert_eq!(store.accounts().len(), parts + 1, "{units} units");
        }
    }

    /// A rotated token being saved: to fail at any step must leave a value that reads, the old
    /// one until the head moves. The head is the last write, so it is the one that decides.
    #[test]
    fn a_write_that_fails_partway_leaves_the_old_value_readable() {
        let (old, new) = (long(LIMIT * 2), "n".repeat(LIMIT * 3));
        for fail_after in 0..4 {
            let store = Fake::default();
            put(&store, &old);
            store.writes_left.set(Some(fail_after));
            let err = set(&store, "session", &Secret::new(&*new), Some(LIMIT)).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{err}");
            store.writes_left.set(None);
            assert_eq!(read(&store).as_deref(), Some(old.as_str()), "{fail_after}");
            // And the next write goes through, past what the failed one left.
            put(&store, &new);
            assert_eq!(read(&store).as_deref(), Some(new.as_str()));
            assert_eq!(store.accounts().len(), 4, "{fail_after}");
        }
    }

    /// A crash left more parts in the slot than the next value needs.
    #[test]
    fn a_later_write_clears_what_a_crash_left_in_its_slot() {
        let store = Fake::default();
        for index in 0..4 {
            store.items.borrow_mut().insert(
                part_account("session", Slot::A, index),
                "left by a crash".into(),
            );
        }
        put(&store, &long(LIMIT + 1));
        assert_eq!(store.accounts().len(), 3);
    }

    #[test]
    fn a_missing_part_is_an_error_naming_the_item_never_a_short_value() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 3));
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        store
            .delete(&part_account("session", head.slot, 1))
            .unwrap();
        let err = get(&store, "session", Some(LIMIT)).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        let text = err.to_string();
        assert!(text.contains("`session`"), "{text}");
        assert!(text.contains("`piekstra.test`"), "{text}");
        assert!(text.contains("auth logout --forget"), "{text}");
    }

    #[test]
    fn parts_that_do_not_belong_together_are_an_error() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        store.items.borrow_mut().insert(
            part_account("session", head.slot, 1),
            "from another write".into(),
        );
        let err = get(&store, "session", Some(LIMIT)).unwrap_err();
        assert!(err.to_string().contains("belong together"), "{err}");
    }

    /// What the second look is for: a reader that met a writer clearing the slot it read.
    #[test]
    fn a_read_that_fails_once_finds_the_value_on_its_second_look() {
        let store = Fake::default();
        let value = long(LIMIT * 2);
        put(&store, &value);
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        store
            .missing_once
            .replace(Some(part_account("session", head.slot, 1)));
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
    }

    /// Two writers, both reading the same head and so both choosing the same slot: the second
    /// runs to the end after the first has written one part, and the first's next parts land
    /// among the second's in the slot the head now names.
    #[test]
    fn two_writers_in_one_slot_leave_one_whole_value() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        let first = "1".repeat(LIMIT * 3);
        let second = "2".repeat(LIMIT * 3);
        let slot = Head::parse(&store.raw("session").unwrap())
            .unwrap()
            .slot
            .other();
        store
            .then_write
            .replace(Some((part_account("session", slot, 0), second)));
        put(&store, &first);
        assert_eq!(read(&store).as_deref(), Some(first.as_str()));
        assert_eq!(store.accounts().len(), 4, "{:?}", store.accounts());
    }

    /// The first writer has moved the head and not yet cleared the old slot, which the second
    /// then fills and points the head at. The first must not clear what is now live.
    #[test]
    fn a_writer_does_not_clear_a_slot_another_write_has_taken() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        let first = "1".repeat(LIMIT * 2);
        let second = "2".repeat(LIMIT * 3);
        store
            .then_write
            .replace(Some(("session".into(), second.clone())));
        put(&store, &first);
        assert_eq!(read(&store).as_deref(), Some(second.as_str()));
    }

    /// A short write's clear must not take the parts a long write just pointed the head at.
    #[test]
    fn a_short_write_does_not_clear_the_slot_of_a_long_one_after_it() {
        let store = Fake::default();
        let second = long(LIMIT * 2);
        store
            .then_write
            .replace(Some(("session".into(), second.clone())));
        put(&store, "short");
        assert_eq!(read(&store).as_deref(), Some(second.as_str()));
    }

    /// The head says nothing of a slot once a short value has replaced it, so a crash there
    /// must not strand the retired tokens until a logout.
    #[test]
    fn a_short_write_clears_what_a_failed_clear_left_behind() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        store
            .refuse_delete
            .replace(Some(part_account("session", head.slot, 0)));
        put(&store, "short one");
        assert_eq!(store.accounts().len(), 3, "{:?}", store.accounts());
        store.refuse_delete.replace(None);
        put(&store, "short two");
        assert_eq!(store.accounts(), ["session"]);
    }

    #[test]
    fn a_short_write_clears_the_parts_of_a_head_that_cannot_be_read() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        store
            .items
            .borrow_mut()
            .insert("session".into(), format!("{MARKER}damaged"));
        put(&store, "short");
        assert_eq!(store.accounts(), ["session"]);
    }

    /// A long write clears the slot it did not use, whatever the head had said.
    #[test]
    fn a_long_write_clears_what_a_crash_left_in_the_other_slot() {
        let store = Fake::default();
        store
            .items
            .borrow_mut()
            .insert(part_account("session", Slot::B, 0), "retired".into());
        put(&store, &long(LIMIT * 2));
        assert_eq!(store.accounts().len(), 3, "{:?}", store.accounts());
        assert!(store.raw(&part_account("session", Slot::B, 0)).is_none());
    }

    /// A damaged item costs two reads and no more.
    #[test]
    fn a_failed_read_looks_once_more() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        store
            .delete(&part_account("session", head.slot, 1))
            .unwrap();
        store.reads.set(0);
        assert!(get(&store, "session", Some(LIMIT)).is_err());
        // Each look: the head and the parts up to the missing one.
        assert_eq!(store.reads.get(), 6);
    }

    #[test]
    fn a_damaged_head_is_an_error_and_a_write_or_a_logout_still_clears_it() {
        for damaged in [
            "pk-split/1 ",
            "pk-split/1 c 2 0000000000000000",
            "pk-split/1 a x 0000000000000000",
            "pk-split/1 a 0 0000000000000000",
            "pk-split/1 a 65 0000000000000000",
            "pk-split/1 a 2 nothex",
            "pk-split/1 a 2 0000000000000000 extra",
            "pk-split/2 is a marker this build does not know",
        ] {
            let store = Fake::default();
            store
                .items
                .borrow_mut()
                .insert("session".into(), damaged.into());
            if damaged.starts_with(MARKER) {
                let err = get(&store, "session", Some(LIMIT)).unwrap_err();
                assert!(err.to_string().contains("head"), "{damaged}: {err}");
                put(&store, "fresh");
                assert_eq!(read(&store).as_deref(), Some("fresh"));
                store
                    .items
                    .borrow_mut()
                    .insert("session".into(), damaged.into());
                assert!(delete(&store, "session", Some(LIMIT)).unwrap());
                assert!(store.accounts().is_empty());
            } else {
                assert_eq!(read(&store).as_deref(), Some(damaged));
            }
        }
    }

    /// A secret can open with anything, so this one must not be taken for a head.
    #[test]
    fn a_value_that_looks_like_a_head_round_trips() {
        let store = Fake::default();
        for value in [
            "pk-split/1 ",
            "pk-split/1 a 1 0000000000000000",
            "pk-split/1 a 99 nonsense",
        ] {
            put(&store, value);
            assert_eq!(read(&store).as_deref(), Some(value));
        }
    }

    #[test]
    fn a_delete_removes_the_head_and_every_part_and_says_whether_anything_was_there() {
        let store = Fake::default();
        assert!(!delete(&store, "session", Some(LIMIT)).unwrap());
        put(&store, &long(LIMIT * 3));
        assert!(delete(&store, "session", Some(LIMIT)).unwrap());
        assert!(store.accounts().is_empty());
        assert!(!delete(&store, "session", Some(LIMIT)).unwrap());
        assert_eq!(read(&store), None);
        put(&store, "plain");
        assert!(delete(&store, "session", Some(LIMIT)).unwrap());
        assert!(store.accounts().is_empty());
    }

    #[test]
    fn a_delete_finds_what_a_crash_left_in_the_slot_the_head_does_not_name() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 2));
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        store.items.borrow_mut().insert(
            part_account("session", head.slot.other(), 0),
            "left by a crash".into(),
        );
        assert!(delete(&store, "session", Some(LIMIT)).unwrap());
        assert!(store.accounts().is_empty());
    }

    /// A logout that could not remove a part says so, and running it again finishes the job.
    #[test]
    fn a_delete_that_fails_partway_says_so_and_a_second_one_finishes() {
        let store = Fake::default();
        put(&store, &long(LIMIT * 4));
        let head = Head::parse(&store.raw("session").unwrap()).unwrap();
        let stuck = part_account("session", head.slot, 1);
        *store.refuse_delete.borrow_mut() = Some(stuck.clone());
        assert!(delete(&store, "session", Some(LIMIT)).is_err());
        assert_eq!(store.accounts(), [stuck]);
        *store.refuse_delete.borrow_mut() = None;
        assert!(delete(&store, "session", Some(LIMIT)).unwrap());
        assert!(store.accounts().is_empty());
    }

    #[test]
    fn a_value_past_the_most_parts_is_refused_with_nothing_written() {
        let store = Fake::default();
        put(&store, "kept");
        let err = set(
            &store,
            "session",
            &Secret::new(long(LIMIT * MAX_PARTS + 1)),
            Some(LIMIT),
        )
        .unwrap_err();
        assert!(err.to_string().contains("too long to store"), "{err}");
        assert_eq!(store.accounts(), ["session"]);
        assert_eq!(read(&store).as_deref(), Some("kept"));
    }

    #[test]
    fn the_most_parts_fit() {
        let store = Fake::default();
        let value = long(LIMIT * MAX_PARTS);
        put(&store, &value);
        assert_eq!(read(&store).as_deref(), Some(value.as_str()));
        assert!(delete(&store, "session", Some(LIMIT)).unwrap());
        assert!(store.accounts().is_empty());
    }

    /// macOS and Linux: every extra keychain lookup there can be another prompt, so with no limit
    /// a value of any length, or shape, is the one call it always was.
    #[derive(Default)]
    struct Unlimited {
        items: RefCell<BTreeMap<String, String>>,
        calls: Cell<usize>,
    }

    impl ItemStore for Unlimited {
        fn service(&self) -> &str {
            "piekstra.test"
        }
        fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.items.borrow().get(account).cloned().map(Secret::new))
        }
        fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
            self.calls.set(self.calls.get() + 1);
            self.items
                .borrow_mut()
                .insert(account.into(), secret.expose().into());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<bool, CliError> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.items.borrow_mut().remove(account).is_some())
        }
    }

    #[test]
    fn with_no_limit_each_call_is_one_operation_on_one_item() {
        let store = Unlimited::default();
        for value in [long(LIMIT * 100), "pk-split/1 a 2 0000000000000000".into()] {
            store.calls.set(0);
            set(&store, "session", &Secret::new(&*value), None).unwrap();
            assert_eq!(store.calls.get(), 1);
            let back = get(&store, "session", None).unwrap().unwrap();
            assert_eq!(back.expose(), value);
            assert_eq!(store.calls.get(), 2);
            assert_eq!(store.items.borrow().len(), 1);
        }
        store.calls.set(0);
        assert!(delete(&store, "session", None).unwrap());
        assert_eq!(store.calls.get(), 1);
    }

    #[test]
    fn this_platform_s_limit_is_windows_s_alone() {
        assert_eq!(ITEM_LIMIT.is_some(), cfg!(windows));
        assert_eq!(WINDOWS_ITEM_UNITS * 2, 2_560);
    }
}
