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
fn nothing_stored_reads_no_key_and_writes_none() {
    let tmp = TempDir::new();
    let keys = Arc::new(MemoryKeys::new());
    let vault = tmp.vault_with(&keys);
    assert!(vault.get("adult1/org-1/p-1").unwrap().is_none());
    assert!(vault.names().unwrap().is_empty());
    assert!(!vault.delete("adult1/org-1/p-1").unwrap());
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
    assert_eq!(err.remedy(), Some(REMEDY_CLEAR));
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
    assert!(vault.names().is_err());
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
    // Once the entries are cleared, a new key can be made.
    assert!(vault.delete("a").unwrap());
    vault.put("b", &secret("y")).unwrap();
    assert_eq!(keys.writes(), 2);
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
