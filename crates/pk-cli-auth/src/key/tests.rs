use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::*;
use crate::vault::{MemoryKeys, QuietLoad, Vault, KEY_ACCOUNT};

/// What the person does with the dialog.
#[derive(Clone, Copy, PartialEq)]
enum Person {
    AlwaysAllows,
    AllowsOnce,
    Denies,
    /// Nobody is there.
    Away,
}

/// A keychain that asks, as macOS does of a newly built binary. It counts
/// its dialogs and the reads that raised none.
struct AskingKeys {
    item: Mutex<Option<String>>,
    granted: Mutex<bool>,
    locked: Mutex<bool>,
    person: Person,
    dialogs: AtomicUsize,
    quiet: AtomicUsize,
}

impl AskingKeys {
    fn new(item: Option<&str>, person: Person) -> Arc<Self> {
        Arc::new(Self {
            item: Mutex::new(item.map(str::to_string)),
            granted: Mutex::new(false),
            locked: Mutex::new(false),
            person,
            dialogs: AtomicUsize::new(0),
            quiet: AtomicUsize::new(0),
        })
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.dialogs.load(Ordering::SeqCst),
            self.quiet.load(Ordering::SeqCst),
        )
    }

    fn item(&self) -> Option<Secret> {
        self.item.lock().unwrap().as_deref().map(Secret::new)
    }
}

impl KeyStore for AskingKeys {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        match self.get_asking(account)? {
            AskedRead::Found(item) => Ok(Some(item)),
            AskedRead::Absent => Ok(None),
            AskedRead::Refused { .. } => Err(CliError::Keychain("refused".into())),
        }
    }

    fn set(&self, _account: &str, value: &Secret) -> Result<(), CliError> {
        *self.item.lock().unwrap() = Some(value.expose().to_string());
        Ok(())
    }

    fn delete(&self, _account: &str) -> Result<bool, CliError> {
        Ok(self.item.lock().unwrap().take().is_some())
    }

    fn get_quiet(&self, _account: &str) -> Result<QuietRead, CliError> {
        self.quiet.fetch_add(1, Ordering::SeqCst);
        Ok(match self.item() {
            None if *self.locked.lock().unwrap() => QuietRead::AbsentLocked,
            None => QuietRead::Absent,
            Some(item) if *self.granted.lock().unwrap() => QuietRead::Found(item),
            Some(_) => QuietRead::NeedsPerson,
        })
    }

    fn get_asking(&self, _account: &str) -> Result<AskedRead, CliError> {
        let Some(item) = self.item() else {
            return Ok(AskedRead::Absent);
        };
        if !*self.granted.lock().unwrap() {
            self.dialogs.fetch_add(1, Ordering::SeqCst);
            match self.person {
                Person::AlwaysAllows => *self.granted.lock().unwrap() = true,
                Person::AllowsOnce => {}
                Person::Denies => return Ok(AskedRead::Refused { status: -128 }),
                Person::Away => {
                    std::thread::sleep(Duration::from_secs(60));
                    panic!("a test waited on a dialog nobody answers");
                }
            }
        }
        Ok(AskedRead::Found(item))
    }

    fn keychain_service(&self) -> Option<&str> {
        Some("piekstra.svc")
    }
}

/// A directory of the test's own, removed when it ends.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pk-cli-key-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    fn vault(&self, keys: Arc<impl KeyStore + 'static>) -> Vault {
        Vault::new(keys, self.0.join("vault"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A key item as the vault writes it.
fn key_item() -> String {
    format!("{{\"format\":1,\"key\":\"{}\"}}", "5a".repeat(32))
}

fn no_dialog() -> KeyArgs {
    KeyArgs {
        no_dialog: true,
        ..KeyArgs::default()
    }
}

fn check(keys: &Arc<AskingKeys>, args: &KeyArgs) -> KeyCheck {
    TempDir::new()
        .vault(Arc::clone(keys))
        .check_key(args)
        .unwrap()
}

#[test]
fn always_allow_reads_after_one_dialog_and_is_told_from_allow() {
    let keys = AskingKeys::new(Some(&key_item()), Person::AlwaysAllows);
    let first = check(&keys, &KeyArgs::default());
    assert!(first.read && first.failure().is_none());
    assert_eq!(first.dialog_needed, Some(true));
    assert_eq!(first.grant, Some(KeyGrant::Always));
    assert_eq!(keys.counts(), (1, 2), "one dialog, between two quiet reads");
    // The next run needs no dialog, and one quiet read is all it makes.
    let second = check(&keys, &KeyArgs::default());
    assert_eq!(
        (second.read, second.dialog_needed, second.grant),
        (true, Some(false), Some(KeyGrant::Always))
    );
    assert_eq!(keys.counts(), (1, 3));

    let keys = AskingKeys::new(Some(&key_item()), Person::AllowsOnce);
    let once = check(&keys, &KeyArgs::default());
    assert!(once.read && once.failure().is_none());
    assert_eq!(once.grant, Some(KeyGrant::Once));
    assert_eq!(keys.counts(), (1, 2));
    assert!(once.text().contains("once (the next read asks again"));
    // Asked again, as it said.
    assert_eq!(check(&keys, &KeyArgs::default()).dialog_needed, Some(true));
}

#[test]
fn a_refusal_is_exit_3_and_is_not_tried_again() {
    let keys = AskingKeys::new(Some(&key_item()), Person::Denies);
    let refused = check(&keys, &KeyArgs::default());
    assert!(!refused.read);
    assert_eq!(refused.outcome, KeyOutcome::Refused);
    assert_eq!(refused.source, KeySource::None);
    assert_eq!((refused.dialog_needed, refused.grant), (Some(true), None));
    assert_eq!(keys.counts(), (1, 1));
    let failure = refused.failure().unwrap();
    assert_eq!(failure.exit_code(), 3);
    assert_eq!(failure.reason(), Some(Reason::CredentialUnreadable));
    assert!(failure.message().contains("-128"), "{failure}");
    assert!(failure.remedy().unwrap().contains("Always Allow"));
}

#[test]
fn no_dialog_raises_none_and_says_one_is_needed() {
    let keys = AskingKeys::new(Some(&key_item()), Person::AlwaysAllows);
    let blocked = check(&keys, &no_dialog());
    assert!(!blocked.read);
    assert_eq!(blocked.outcome, KeyOutcome::NeedsDialog);
    assert_eq!(blocked.dialog_needed, Some(true));
    assert_eq!(blocked.failure().unwrap().exit_code(), 3);
    assert_eq!(keys.counts(), (0, 1));
    // Once a person has answered, it reads.
    assert!(check(&keys, &KeyArgs::default()).read);
    assert!(check(&keys, &no_dialog()).read);
    assert_eq!(keys.counts().0, 1);
}

#[test]
fn a_dialog_nobody_answers_ends_at_the_wait() {
    let keys = AskingKeys::new(Some(&key_item()), Person::Away);
    let wait = KeyArgs {
        wait: Some(1),
        ..KeyArgs::default()
    };
    let unanswered = check(&keys, &wait);
    assert!(!unanswered.read);
    assert_eq!(unanswered.outcome, KeyOutcome::NoAnswer);
    assert!(unanswered.elapsed_ms >= 1000, "{}", unanswered.elapsed_ms);
    let failure = unanswered.failure().unwrap();
    assert_eq!(failure.exit_code(), 3);
    assert!(failure.message().contains("1 s"), "{failure}");
    assert_eq!(keys.counts(), (1, 1));
}

#[test]
fn nothing_stored_is_no_failure_and_a_lost_key_is_one() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault(Arc::clone(&keys));
    let empty = vault.check_key(&KeyArgs::default()).unwrap();
    assert_eq!(
        (empty.read, empty.outcome, empty.source),
        (false, KeyOutcome::Absent, KeySource::None)
    );
    assert!(empty.failure().is_none());
    assert_eq!(empty.emit(true), 0);
    assert!(empty.text().contains("Nothing stored"));

    vault.put("entry", &Secret::new("x")).unwrap();
    keys.forget();
    let lost = vault.check_key(&KeyArgs::default()).unwrap();
    assert_eq!(lost.outcome, KeyOutcome::Absent);
    assert_eq!(lost.failure().unwrap().exit_code(), 1);
    assert_eq!(
        lost.failure().unwrap().remedy(),
        Some(crate::vault::REMEDY_CLEAR)
    );
}

#[test]
fn an_item_that_is_not_a_key_was_read_and_fails() {
    let keys = AskingKeys::new(Some("another item's text"), Person::AlwaysAllows);
    let bad = check(&keys, &KeyArgs::default());
    assert!(bad.read);
    assert_eq!(bad.outcome, KeyOutcome::Read);
    assert_eq!(bad.failure().unwrap().exit_code(), 1);
    let shown = format!("{} {}", bad.to_json(), bad.text());
    assert!(!shown.contains("another item"), "{shown}");
}

/// The vault's cached key does not answer for the keychain.
#[test]
fn the_check_reads_past_the_vaults_cache() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault(Arc::clone(&keys));
    vault.put("entry", &Secret::new("x")).unwrap();
    let before = keys.reads();
    assert!(vault.get("entry").unwrap().is_some());
    assert_eq!(keys.reads(), before, "the cached key");
    assert!(vault.check_key(&KeyArgs::default()).unwrap().read);
    assert_eq!(keys.reads(), before + 1);
    // And it left the cache as it was.
    assert!(vault.get("entry").unwrap().is_some());
    assert_eq!(keys.reads(), before + 1);
}

/// A store that cannot say whether it asks: one read, and no claim.
struct PlainKeys(MemoryKeys);

impl KeyStore for PlainKeys {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        self.0.get(account)
    }
    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        self.0.set(account, value)
    }
    fn delete(&self, account: &str) -> Result<bool, CliError> {
        self.0.delete(account)
    }
}

#[test]
fn a_store_that_cannot_say_is_read_once_and_claims_nothing() {
    let keys = Arc::new(PlainKeys(MemoryKeys::new()));
    keys.set(KEY_ACCOUNT, &Secret::new(key_item())).unwrap();
    let vault = TempDir::new().vault(Arc::clone(&keys));
    let read = vault.check_key(&KeyArgs::default()).unwrap();
    assert!(read.read);
    assert_eq!((read.dialog_needed, read.grant), (None, None));
    assert_eq!(read.service, None);
    assert_eq!(keys.0.reads(), 1);
    let usage = vault.check_key(&no_dialog()).unwrap_err();
    assert_eq!(usage.exit_code(), 2);
    assert_eq!(keys.0.reads(), 1, "refused before any read");
}

#[test]
fn a_failing_keychain_is_reported_with_its_own_exit_code() {
    struct Broken;
    impl KeyStore for Broken {
        fn get(&self, _: &str) -> Result<Option<Secret>, CliError> {
            Err(CliError::Keychain("no default keychain".into()))
        }
        fn set(&self, _: &str, _: &Secret) -> Result<(), CliError> {
            unreachable!()
        }
        fn delete(&self, _: &str) -> Result<bool, CliError> {
            unreachable!()
        }
        fn get_quiet(&self, account: &str) -> Result<QuietRead, CliError> {
            self.get(account).map(|_| QuietRead::Absent)
        }
    }
    let failed = TempDir::new()
        .vault(Arc::new(Broken))
        .check_key(&KeyArgs::default())
        .unwrap();
    assert_eq!(failed.outcome, KeyOutcome::Failed);
    assert_eq!(failed.failure().unwrap().exit_code(), 1);
    assert_eq!(failed.dialog_needed, None);
}

/// One document with the DTO and, on a failure, the error contract's
/// object, and never the key.
#[test]
fn the_json_is_one_document_with_no_key_in_it() {
    let keys = AskingKeys::new(Some(&key_item()), Person::AlwaysAllows);
    let ok = check(&keys, &KeyArgs::default()).to_json();
    assert_eq!(ok["schema"], "auth-key/v1");
    assert_eq!(ok["read"], true);
    assert_eq!(ok["source"], "keychain");
    assert_eq!(ok["outcome"], "read");
    assert_eq!(ok["service"], "piekstra.svc");
    assert_eq!(ok["account"], KEY_ACCOUNT);
    assert_eq!(ok["dialog_needed"], true);
    assert_eq!(ok["grant"], "always");
    assert!(ok["elapsed_ms"].is_u64());
    assert!(ok.get("error").is_none());
    assert_eq!(ok.get("signing").is_some(), cfg!(target_os = "macos"));
    assert!(!ok.to_string().contains("5a5a"), "{ok}");

    let keys = AskingKeys::new(Some(&key_item()), Person::Denies);
    let refused = check(&keys, &KeyArgs::default()).to_json();
    assert_eq!(refused["read"], false);
    assert_eq!(refused["source"], "none");
    assert_eq!(refused["outcome"], "refused");
    assert_eq!(refused["error"]["code"], "auth");
    assert_eq!(refused["error"]["reason"], "credential_unreadable");
    assert!(refused["error"]["remedy"].is_string());
    assert!(refused.get("grant").is_none());
    // A driver reads it back, minus what only the producer holds.
    let read: KeyCheck = serde_json::from_value(refused).unwrap();
    assert_eq!(read.outcome, KeyOutcome::Refused);
    assert!(read.failure().is_none());
}

#[test]
fn the_text_names_the_item_and_never_the_key() {
    let keys = AskingKeys::new(Some(&key_item()), Person::AlwaysAllows);
    let text = check(&keys, &KeyArgs::default()).text();
    for line in [
        "Read:          yes\n",
        "Source:        keychain (piekstra.svc, vault-key)\n",
        "Dialog needed: yes\n",
        "Grant:         always\n",
        "Elapsed:       ",
    ] {
        assert!(text.contains(line), "{text}");
    }
    assert!(!text.contains("5a5a"), "{text}");
}

/// An item a CLI keeps without a vault.
#[test]
fn any_item_can_be_checked() {
    let asking = AskingKeys::new(Some("a session"), Person::AlwaysAllows);
    let keys: Arc<dyn KeyStore> = Arc::clone(&asking) as _;
    let check = check_item(&keys, "session", &KeyArgs::default(), |found| {
        found
            .map(drop)
            .ok_or_else(|| CliError::Auth("no session stored".into()))
    })
    .unwrap();
    assert!(check.read && check.failure().is_none());
    assert_eq!(check.account, "session");
}

#[test]
fn the_flags_parse_and_the_conflicting_ones_do_not() {
    use clap::Parser;
    #[derive(Parser, Debug)]
    struct Probe {
        #[command(flatten)]
        key: KeyArgs,
    }
    let parse = |args: &[&str]| Probe::try_parse_from([&["probe"], args].concat());
    assert!(parse(&["--no-dialog"]).unwrap().key.no_dialog);
    assert_eq!(parse(&["--wait", "30"]).unwrap().key.wait, Some(30));
    for bad in [
        &["--wait", "0"][..],
        &["--wait", "soon"],
        &["--no-dialog", "--wait", "30"],
    ] {
        assert_eq!(parse(bad).unwrap_err().exit_code(), 2, "{bad:?}");
    }
}

#[test]
fn a_quiet_load_raises_no_dialog_and_caches_the_key_once_granted() {
    let tmp = TempDir::new();
    let keys = AskingKeys::new(None, Person::AlwaysAllows);
    // The CLI's earlier build stored an entry.
    *keys.granted.lock().unwrap() = true;
    tmp.vault(Arc::clone(&keys))
        .put("entry", &Secret::new("x"))
        .unwrap();
    // A new build, not granted.
    *keys.granted.lock().unwrap() = false;
    let vault = tmp.vault(Arc::clone(&keys));
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::NeedsPerson);
    assert_eq!(keys.counts().0, 0);
    assert!(vault.check_key(&KeyArgs::default()).unwrap().read);
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::Loaded);
    let (dialogs, quiet) = keys.counts();
    assert_eq!(vault.get("entry").unwrap().unwrap().expose(), "x");
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::Loaded);
    assert_eq!(keys.counts(), (dialogs, quiet), "from the cache");
    // The key gone while the entry is there.
    keys.delete(KEY_ACCOUNT).unwrap();
    assert_eq!(
        tmp.vault(Arc::clone(&keys)).load_key_quietly().unwrap(),
        QuietLoad::Missing
    );
}

/// A key with no entries (a sign-out leaves it): the next write reads it, so
/// a quiet load that said "empty" would send an unattended run into a dialog.
#[test]
fn a_quiet_load_of_an_empty_vault_answers_for_the_write_after_it() {
    let tmp = TempDir::new();
    let keys = AskingKeys::new(Some(&key_item()), Person::Away);
    let vault = tmp.vault(Arc::clone(&keys));
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::NeedsPerson);
    assert_eq!(keys.counts(), (0, 1));
    // Granted, the key is loaded, and the write raises nothing.
    *keys.granted.lock().unwrap() = true;
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::Loaded);
    vault.put("entry", &Secret::new("x")).unwrap();
    assert_eq!(keys.counts().0, 0);

    // No key either: the write makes one, and asks nobody.
    let tmp = TempDir::new();
    let keys = AskingKeys::new(None, Person::Away);
    let vault = tmp.vault(Arc::clone(&keys));
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::Empty);
    assert!(!vault.dir().exists(), "a load creates nothing");
}

/// macOS finds an item absent from a locked keychain without asking, and
/// asks to unlock it for the key a first write makes.
#[test]
fn a_quiet_load_with_no_key_in_a_locked_keychain_needs_a_person() {
    let tmp = TempDir::new();
    let keys = AskingKeys::new(None, Person::Away);
    *keys.locked.lock().unwrap() = true;
    let vault = tmp.vault(Arc::clone(&keys));
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::NeedsPerson);
    // `auth key` is about the read, which asks nobody.
    for args in [KeyArgs::default(), no_dialog()] {
        let absent = vault.check_key(&args).unwrap();
        assert_eq!(absent.outcome, KeyOutcome::Absent);
        assert_eq!(absent.dialog_needed, Some(false));
        assert!(absent.failure().is_none());
    }
    // With entries, the key is lost, locked or not.
    *keys.locked.lock().unwrap() = false;
    *keys.granted.lock().unwrap() = true;
    vault.put("entry", &Secret::new("x")).unwrap();
    keys.delete(KEY_ACCOUNT).unwrap();
    *keys.locked.lock().unwrap() = true;
    assert_eq!(
        tmp.vault(Arc::clone(&keys)).load_key_quietly().unwrap(),
        QuietLoad::Missing
    );
    assert_eq!(keys.counts().0, 0);
}

#[test]
fn a_quiet_load_says_when_the_store_cannot_be_read_quietly() {
    let tmp = TempDir::new();
    let plain = Arc::new(PlainKeys(MemoryKeys::new()));
    tmp.vault(Arc::clone(&plain))
        .put("entry", &Secret::new("x"))
        .unwrap();
    let reads = plain.0.reads();
    assert_eq!(
        tmp.vault(Arc::clone(&plain)).load_key_quietly().unwrap(),
        QuietLoad::Unsupported
    );
    assert_eq!(plain.0.reads(), reads);
}

/// A bounded read whose dialog is open: the quiet load does not ask beside
/// it.
#[test]
fn a_quiet_load_leaves_an_open_dialog_alone() {
    let tmp = TempDir::new();
    let keys = AskingKeys::new(None, Person::Away);
    *keys.granted.lock().unwrap() = true;
    tmp.vault(Arc::clone(&keys))
        .put("entry", &Secret::new("x"))
        .unwrap();
    *keys.granted.lock().unwrap() = false;
    let vault = tmp.vault(Arc::clone(&keys));
    let waited = vault.load_key(Duration::from_millis(50)).unwrap();
    assert_eq!(waited, crate::vault::KeyLoad::Waiting);
    let before = keys.counts();
    assert_eq!(vault.load_key_quietly().unwrap(), QuietLoad::NeedsPerson);
    assert_eq!(keys.counts(), before);
}
