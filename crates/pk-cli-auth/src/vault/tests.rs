use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;

/// A directory of the test's own, removed when it ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pk-cli-vault-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    fn vault_with(&self, keys: &Arc<MemoryKeys>) -> Vault {
        Vault::new(Arc::clone(keys), self.0.join("vault"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn secret(text: &str) -> Secret {
    Secret::new(text)
}

#[test]
fn an_entry_reads_back_and_is_gone_once_deleted() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("adult1/org-1/p-1", &secret("one")).unwrap();
    vault.put("adult1/org-1/p-2", &secret("two")).unwrap();
    vault
        .put("adult1/org-1/p-1", &secret("one, renewed"))
        .unwrap();
    assert_eq!(
        vault.get("adult1/org-1/p-1").unwrap().unwrap().expose(),
        "one, renewed"
    );
    assert_eq!(
        vault.names().unwrap(),
        ["adult1/org-1/p-1", "adult1/org-1/p-2"]
    );
    assert!(vault.delete("adult1/org-1/p-1").unwrap());
    assert!(!vault.delete("adult1/org-1/p-1").unwrap());
    assert!(vault.get("adult1/org-1/p-1").unwrap().is_none());
    assert_eq!(vault.names().unwrap(), ["adult1/org-1/p-2"]);
}

#[test]
fn contains_finds_an_entry_without_the_key() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    assert!(!vault.contains("_client/sandbox").unwrap());
    vault.put("_client/sandbox", &secret("pair")).unwrap();
    // A new process: nothing cached, and the check reads no key.
    let fresh = tmp.vault_with(&keys);
    let reads = keys.reads();
    assert!(fresh.contains("_client/sandbox").unwrap());
    assert!(!fresh.contains("_client/production").unwrap());
    assert_eq!(keys.reads(), reads);
    assert!(fresh.delete("_client/sandbox").unwrap());
    assert!(!fresh.contains("_client/sandbox").unwrap());
}

#[test]
fn nothing_stored_reads_no_key_and_writes_none() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    assert!(vault.get("adult1/org-1/p-1").unwrap().is_none());
    assert!(vault.names().unwrap().is_empty());
    assert!(!vault.delete("adult1/org-1/p-1").unwrap());
    assert!(!vault.contains("adult1/org-1/p-1").unwrap());
    assert_eq!((keys.reads(), keys.writes()), (0, 0));
    assert!(!vault.dir().exists(), "a read creates nothing");
}

#[test]
fn the_key_is_made_once_and_read_once_per_process() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    for i in 0..5 {
        vault.put(&format!("entry-{i}"), &secret("x")).unwrap();
    }
    assert_eq!(keys.writes(), 1);
    let reads = keys.reads();

    // A second process: one more read, however many entries.
    let again = tmp.vault_with(&keys);
    assert_eq!(again.names().unwrap().len(), 5);
    for i in 0..5 {
        assert!(again.get(&format!("entry-{i}")).unwrap().is_some());
    }
    assert_eq!(keys.reads(), reads + 1);
    assert_eq!(keys.writes(), 1);
}

#[test]
fn two_first_writers_share_one_key() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let dir = tmp.0.join("vault");
    let writers: Vec<_> = (0..8)
        .map(|i| {
            let (keys, dir) = (Arc::clone(&keys), dir.clone());
            std::thread::spawn(move || {
                Vault::new(keys, dir)
                    .put(&format!("entry-{i}"), &Secret::new("x"))
                    .unwrap()
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(keys.writes(), 1, "one key for every writer");
    assert_eq!(tmp.vault_with(&keys).names().unwrap().len(), 8);
}

#[test]
fn a_file_copied_over_another_reads_as_an_error() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("for a")).unwrap();
    vault.put("b", &secret("for b")).unwrap();
    fs::copy(vault.entry_path("a"), vault.entry_path("b")).unwrap();
    let err = vault.get("b").unwrap_err();
    assert!(err.to_string().contains("does not decrypt"), "{err}");
    assert!(!err.to_string().contains("for a"), "{err}");
    assert_eq!(err.remedy(), Some(REMEDY_DELETE_ENTRY));
}

#[test]
fn a_changed_byte_reads_as_an_error_with_no_value_in_it() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("the value")).unwrap();
    let path = vault.entry_path("a");
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&path, bytes).unwrap();
    let err = vault.get("a").unwrap_err();
    assert!(!err.to_string().contains("the value"), "{err}");
    assert!(vault.names().unwrap().is_empty());
}

#[test]
fn one_damaged_entry_leaves_the_others_listed() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("x")).unwrap();
    vault.put("b", &secret("y")).unwrap();
    fs::write(vault.entry_path("a"), b"PKV1 damaged").unwrap();
    let listing = vault.list().unwrap();
    assert_eq!(listing.names, ["b"]);
    assert_eq!(listing.unreadable, [vault.entry_path("a")]);
    assert_eq!(vault.names().unwrap(), ["b"]);
    assert!(vault.get("b").unwrap().is_some());
}

#[test]
fn a_lost_key_is_an_error_and_never_replaced_while_entries_exist() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    tmp.vault_with(&keys).put("a", &secret("x")).unwrap();
    keys.forget();
    let vault = tmp.vault_with(&keys);
    let err = vault.get("a").unwrap_err();
    assert_eq!(err.exit_code(), 1, "{err}");
    assert!(err.to_string().contains(KEY_ACCOUNT), "{err}");
    assert!(vault.put("b", &secret("y")).is_err());
    assert_eq!(keys.writes(), 1, "no second key");
    assert_eq!(err.remedy(), Some(REMEDY_CLEAR));
    // Once the entries are cleared, a new key can be made.
    assert!(vault.delete("a").unwrap());
    vault.put("b", &secret("y")).unwrap();
    assert_eq!(keys.writes(), 2);
}

#[test]
fn clear_removes_every_entry_and_the_key() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("x")).unwrap();
    vault.put("b", &secret("y")).unwrap();
    fs::write(vault.dir().join(".stray.tmp"), b"").unwrap();
    assert_eq!(vault.clear().unwrap(), 2);
    assert!(keys.get(KEY_ACCOUNT).unwrap().is_none());
    assert!(vault.names().unwrap().is_empty());
    assert!(!vault.dir().join(".stray.tmp").exists());
    // A key that is not a key is cleared the same way.
    keys.set(KEY_ACCOUNT, &secret("not a key")).unwrap();
    assert!(vault.put("c", &secret("z")).is_err());
    assert_eq!(vault.clear().unwrap(), 0);
    vault.put("c", &secret("z")).unwrap();
    assert_eq!(vault.get("c").unwrap().unwrap().expose(), "z");
    // A vault that never existed: nothing to remove, nothing created.
    let other = TempDir::new();
    let fresh = other.vault_with(&Arc::new(MemoryKeys::new()));
    assert_eq!(fresh.clear().unwrap(), 0);
    assert!(!fresh.dir().exists());
}

#[test]
fn a_writer_holding_the_old_key_makes_a_new_one_after_a_clear() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let agent = tmp.vault_with(&keys);
    agent.put("adult1/session", &secret("one")).unwrap();
    // Another process forgets every sign-in.
    assert_eq!(tmp.vault_with(&keys).clear().unwrap(), 1);
    // The agent's renewal lands after it, with its key still cached.
    agent.put("adult1/session", &secret("two")).unwrap();
    let fresh = tmp.vault_with(&keys);
    assert_eq!(
        fresh.get("adult1/session").unwrap().unwrap().expose(),
        "two"
    );
    assert_eq!(keys.writes(), 2, "a second key, made after the clear");
}

#[test]
fn a_reader_holding_the_old_key_follows_a_new_one() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let reader = tmp.vault_with(&keys);
    reader.put("a", &secret("old")).unwrap();
    assert_eq!(reader.get("a").unwrap().unwrap().expose(), "old");
    let other = tmp.vault_with(&keys);
    other.clear().unwrap();
    other.put("a", &secret("new")).unwrap();
    other.put("b", &secret("b")).unwrap();
    assert_eq!(reader.get("a").unwrap().unwrap().expose(), "new");
    assert_eq!(reader.names().unwrap(), ["a", "b"]);
}

#[test]
fn items_move_in_read_back_and_only_then_leave() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    let old = MemoryKeys::new();
    old.set("smart/org-1/p-1", &secret("session one")).unwrap();
    old.set("smart/org-1/p-2", &secret("session two, older"))
        .unwrap();
    vault
        .put("adult1/org-1/p-2", &secret("session two"))
        .unwrap();
    let pairs = [
        ("smart/org-1/p-1", "adult1/org-1/p-1"),
        ("smart/org-1/p-2", "adult1/org-1/p-2"),
        ("smart/org-1/p-3", "adult1/org-1/p-3"),
    ];
    let done = vault.migrate_from(&old, &pairs).unwrap();
    assert_eq!(
        done,
        Migration {
            moved: 1,
            kept: 1,
            absent: 1
        }
    );
    assert_eq!(
        vault.get("adult1/org-1/p-1").unwrap().unwrap().expose(),
        "session one"
    );
    assert_eq!(
        vault.get("adult1/org-1/p-2").unwrap().unwrap().expose(),
        "session two"
    );
    assert!(old.get("smart/org-1/p-1").unwrap().is_none());
    assert!(old.get("smart/org-1/p-2").unwrap().is_none());
    // Again: nothing left to move, and no item read but absent ones.
    let reads = old.reads();
    let again = vault.migrate_from(&old, &pairs[..1]).unwrap();
    assert_eq!(again.kept, 1);
    assert_eq!(old.reads(), reads, "a kept entry reads no item");
}

#[test]
fn a_failed_move_keeps_the_item() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    tmp.vault_with(&keys).put("x", &secret("x")).unwrap();
    keys.forget();
    // A new process: the key is gone and an entry is there.
    let vault = tmp.vault_with(&keys);
    let old = MemoryKeys::new();
    old.set("item", &secret("the session")).unwrap();
    assert!(vault.migrate_from(&old, &[("item", "entry")]).is_err());
    assert_eq!(old.get("item").unwrap().unwrap().expose(), "the session");
}

#[test]
fn a_held_key_lock_is_waited_for_and_then_refused() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp
        .vault_with(&keys)
        .wait_at_most(Duration::from_millis(200));
    vault.ensure_dir().unwrap();
    let held = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(vault.dir().join(LOCK_FILE))
        .unwrap();
    held.lock().unwrap();
    let started = Instant::now();
    let err = vault.put("a", &secret("x")).unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(err.exit_code(), 5, "{err}");
    assert!(err.to_string().contains(LOCK_FILE), "{err}");
    assert_eq!(keys.writes(), 0);
    drop(held);
    vault.put("a", &secret("x")).unwrap();
}

#[test]
fn a_key_item_that_is_not_a_key_is_an_error() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    keys.set(KEY_ACCOUNT, &secret(r#"{"format":1,"key":"not hex"}"#))
        .unwrap();
    let err = tmp.vault_with(&keys).put("a", &secret("x")).unwrap_err();
    assert!(err.to_string().contains("not a vault key"), "{err}");
}

#[test]
fn no_name_or_value_is_on_disk_in_the_clear() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault
        .put("adult1/org-1/patient-77", &secret("refresh-token-value"))
        .unwrap();
    for entry in fs::read_dir(vault.dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !name.contains("adult1") && !name.contains("patient"),
            "{name}"
        );
        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("patient-77") && !text.contains("refresh-token"));
    }
}

#[cfg(unix)]
#[test]
fn the_directory_and_entries_are_the_owners_alone() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("x")).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(vault.dir()), 0o700);
    assert_eq!(mode(&vault.entry_path("a")), 0o600);
    let leftovers: Vec<_> = fs::read_dir(vault.dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temporary file is left behind");
}

/// After a clear the directory holds the lock and the key id, both the
/// owner's alone, and nothing that names or holds a secret.
#[cfg(unix)]
#[test]
fn a_clear_leaves_only_the_lock_and_the_key_id_both_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("x")).unwrap();
    // A lock file an earlier release created at the umask's mode.
    let lock = vault.dir().join(LOCK_FILE);
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(vault.clear().unwrap(), 1);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let mut left: Vec<String> = fs::read_dir(vault.dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [ID_FILE, LOCK_FILE]);
    assert_eq!(mode(&lock), 0o600);
    assert_eq!(mode(&vault.dir().join(ID_FILE)), 0o600);
    assert_eq!(fs::metadata(&lock).unwrap().len(), 0);
}

#[test]
fn json_round_trips_and_a_bad_entry_names_no_value() {
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Session {
        token: String,
    }
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    let session = Session {
        token: "t-1".into(),
    };
    vault.put_json("s", &session).unwrap();
    assert_eq!(vault.get_json::<Session>("s").unwrap(), Some(session));
    vault.put("s", &secret("not json, secret-ish")).unwrap();
    let err = vault.get_json::<Session>("s").unwrap_err();
    assert!(err.to_string().contains("line 1"), "{err}");
    assert!(!err.to_string().contains("secret-ish"), "{err}");
}

#[test]
fn debug_shows_no_key() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    vault.put("a", &secret("x")).unwrap();
    let shown = format!("{vault:?}");
    assert!(shown.contains("dir"), "{shown}");
    assert!(!shown.contains("key"), "{shown}");
}

#[test]
fn the_default_directory_is_under_the_home_config() {
    let dir = default_dir("example").unwrap();
    assert!(dir.ends_with(".config/example/vault"), "{}", dir.display());
}

/// A key store whose reads wait until it is opened, as a keychain read
/// waits on a dialog until a person answers it.
#[derive(Default)]
struct GatedKeys {
    inner: MemoryKeys,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
    reads: AtomicUsize,
}

impl GatedKeys {
    fn set_open(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.opened.notify_all();
    }
}

impl KeyStore for GatedKeys {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
        drop(open);
        self.inner.get(account)
    }

    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        self.inner.set(account, value)
    }

    fn delete(&self, account: &str) -> Result<bool, CliError> {
        self.inner.delete(account)
    }
}

#[test]
fn a_bounded_key_read_nobody_answers_waits_and_blocks_no_one() {
    let tmp = TempDir::new();
    let keys = Arc::new(GatedKeys::default());
    keys.set_open(true);
    Vault::new(Arc::clone(&keys), tmp.0.join("vault"))
        .put("a", &secret("one"))
        .unwrap();
    keys.set_open(false);
    let before = keys.reads.load(Ordering::SeqCst);
    let vault = Arc::new(Vault::new(Arc::clone(&keys), tmp.0.join("vault")));
    let started = std::time::Instant::now();
    assert_eq!(
        vault.load_key(Duration::from_millis(50)).unwrap(),
        KeyLoad::Waiting
    );
    // While the dialog is open, reads and writes fail at once instead of
    // blocking or asking again, and another load waits on the same read.
    let err = vault.get("a").unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(err.to_string().contains("dialog"), "{err}");
    assert_eq!(vault.put("b", &secret("two")).unwrap_err().exit_code(), 3);
    assert_eq!(
        vault.load_key(Duration::from_millis(50)).unwrap(),
        KeyLoad::Waiting
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(keys.reads.load(Ordering::SeqCst) - before, 1);
    // Answered: the key is cached, and nothing reads the keychain again.
    keys.set_open(true);
    assert_eq!(
        vault.load_key(Duration::from_secs(5)).unwrap(),
        KeyLoad::Loaded
    );
    assert_eq!(vault.get("a").unwrap().unwrap().expose(), "one");
    vault.put("b", &secret("two")).unwrap();
    assert_eq!(keys.reads.load(Ordering::SeqCst) - before, 1);
}

#[test]
fn a_bounded_read_that_answers_after_the_wait_is_taken_by_the_next_read() {
    let tmp = TempDir::new();
    let keys = Arc::new(GatedKeys::default());
    keys.set_open(true);
    Vault::new(Arc::clone(&keys), tmp.0.join("vault"))
        .put("a", &secret("one"))
        .unwrap();
    keys.set_open(false);
    let vault = Arc::new(Vault::new(Arc::clone(&keys), tmp.0.join("vault")));
    assert_eq!(
        vault.load_key(Duration::from_millis(20)).unwrap(),
        KeyLoad::Waiting
    );
    keys.set_open(true);
    // A plain read after the answer uses it rather than reading again.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let value = loop {
        match vault.get("a") {
            Ok(v) => break v,
            Err(e) if e.exit_code() == 3 && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(value.unwrap().expose(), "one");
}

#[test]
fn a_bounded_load_reads_nothing_for_an_empty_vault_and_names_a_lost_key() {
    let tmp = TempDir::new();
    let keys = Arc::new(GatedKeys::default());
    let vault = Arc::new(Vault::new(Arc::clone(&keys), tmp.0.join("vault")));
    assert_eq!(
        vault.load_key(Duration::from_millis(20)).unwrap(),
        KeyLoad::Empty
    );
    assert_eq!(keys.reads.load(Ordering::SeqCst), 0);
    // Entries sealed under a key this keychain does not hold.
    Vault::new(MemoryKeys::new(), tmp.0.join("vault"))
        .put("a", &secret("one"))
        .unwrap();
    keys.set_open(true);
    let vault = Arc::new(Vault::new(Arc::clone(&keys), tmp.0.join("vault")));
    assert_eq!(
        vault.load_key(Duration::from_secs(5)).unwrap(),
        KeyLoad::Missing
    );
}

#[test]
fn a_bounded_vault_rereads_a_replaced_key_without_blocking_anyone() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("vault");
    let keys = Arc::new(GatedKeys::default());
    keys.set_open(true);
    let server =
        Arc::new(Vault::new(Arc::clone(&keys), &dir).bound_key_reads(Duration::from_millis(50)));
    server.put("a", &secret("one")).unwrap();
    assert_eq!(server.get("a").unwrap().unwrap().expose(), "one");
    // Another process resets the vault and signs in again: a new key.
    let other = Vault::new(Arc::clone(&keys), &dir);
    other.clear().unwrap();
    other.put("a", &secret("two")).unwrap();
    // The server's next read cannot open the entry with its cached key and
    // asks the keychain for the new one, which waits on a dialog.
    keys.set_open(false);
    let before = keys.reads.load(Ordering::SeqCst);
    let started = std::time::Instant::now();
    let err = server.get("a").unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(err.to_string().contains("dialog"), "{err}");
    // Another thread using the vault is not held behind it.
    let peer = Arc::clone(&server);
    let other_thread = std::thread::spawn(move || peer.get("a").unwrap_err().exit_code());
    assert_eq!(other_thread.join().unwrap(), 3);
    assert_eq!(server.put("b", &secret("x")).unwrap_err().exit_code(), 3);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(keys.reads.load(Ordering::SeqCst) - before, 1);
    // Answered: the next read takes the new key, and asks nothing more.
    keys.set_open(true);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let value = loop {
        match server.get("a") {
            Ok(v) => break v,
            Err(e) if e.exit_code() == 3 && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(value.unwrap().expose(), "two");
    assert_eq!(keys.reads.load(Ordering::SeqCst) - before, 1);
}

#[test]
fn a_bounded_vaults_first_read_waits_at_most_the_bound() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("vault");
    let keys = Arc::new(GatedKeys::default());
    keys.set_open(true);
    Vault::new(Arc::clone(&keys), &dir)
        .put("a", &secret("one"))
        .unwrap();
    keys.set_open(false);
    let before = keys.reads.load(Ordering::SeqCst);
    let vault = Vault::new(Arc::clone(&keys), &dir).bound_key_reads(Duration::from_millis(50));
    let started = std::time::Instant::now();
    assert_eq!(vault.get("a").unwrap_err().exit_code(), 3);
    assert_eq!(vault.get("a").unwrap_err().exit_code(), 3);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(keys.reads.load(Ordering::SeqCst) - before, 1);
    keys.set_open(true);
    assert_eq!(
        vault.load_key(Duration::from_secs(5)).unwrap(),
        KeyLoad::Loaded
    );
    assert_eq!(vault.get("a").unwrap().unwrap().expose(), "one");
}
