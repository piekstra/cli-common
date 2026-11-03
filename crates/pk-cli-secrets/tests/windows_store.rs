//! The real Windows Credential Manager, which only a Windows runner has: what `split.rs`'s
//! tests prove against a fake must hold against the store itself. Run by
//! `.github/workflows/windows-credential.yml`. Elsewhere this compiles to nothing, because a
//! test that touched a real keychain would raise a dialog on a Mac.
#![cfg(windows)]

use pk_cli_secrets::{CredentialStore, Secret};

/// Longer than one credential holds, with characters of every UTF-16 width.
fn long_value() -> String {
    format!("x{}{}", "\u{1F600}".repeat(1_500), "é".repeat(2_000))
}

struct Throwaway(CredentialStore);

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = self.0.delete("session");
    }
}

fn store() -> Throwaway {
    Throwaway(CredentialStore::new(format!(
        "piekstra.pk-cli-secrets-test-{}",
        std::process::id()
    )))
}

#[test]
fn a_value_over_one_credential_round_trips_and_is_removed_whole() {
    let store = store();
    let value = long_value();
    assert!(value.encode_utf16().count() * 2 > 2_560);

    store.0.set("session", &Secret::new(&*value)).unwrap();
    let back = store.0.get("session").unwrap().expect("stored");
    assert_eq!(back.expose(), value);

    // Over itself, into the other slot.
    let again = format!("{value}y");
    store.0.set("session", &Secret::new(&*again)).unwrap();
    assert_eq!(store.0.get("session").unwrap().unwrap().expose(), again);

    // Down to one item, up again, and out.
    store.0.set("session", &Secret::new("short")).unwrap();
    assert_eq!(store.0.get("session").unwrap().unwrap().expose(), "short");
    store.0.set("session", &Secret::new(&*value)).unwrap();
    assert!(store.0.delete("session").unwrap());
    assert!(store.0.get("session").unwrap().is_none());
    assert!(!store.0.delete("session").unwrap());
}
