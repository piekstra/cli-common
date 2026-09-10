//! Secret handling for the piekstra CLI family (SPEC v1 §1.7).
//!
//! Runtime secrets live only in the OS keychain, under the service name
//! `piekstra.<binary>`. Getting a secret *into* the keychain is a setup-time
//! concern (`auth login` / `auth set-credential`), which ingest via stdin or
//! a named env var — never a `--value` flag (that leaks into `ps`, shell
//! history, and pasted transcripts).
//!
//! Secrets never appear in `Debug`/`Display` output and are zeroized on drop.
//!
//! # One keychain item per credential set
//!
//! On macOS, every keychain item a freshly built binary reads is a permission
//! prompt (the grant is per item, per code identity — see the signing notes
//! in AGENTS.md). A CLI that keeps a token, its refresh token, a username and
//! a region as four items therefore asks four times after every rebuild, and
//! reads as "flaky". The rule is one item per credential set: a JSON blob
//! under one account, read with [`CredentialStore::get_json`] and written
//! with [`CredentialStore::set_json`]. A CLI that started with the per-field
//! layout moves off it with [`CredentialStore::migrate_from`] (for a service
//! rename) or its own first-read migration (for a re-shaping).

use std::fmt;
use std::io::Read;

use keyring::Entry;
use pk_cli_core::CliError;
use serde::de::DeserializeOwned;
use serde::Serialize;
use zeroize::Zeroize;

/// The `--stdin` / `--from-env <VAR>` ingestion flags, flattenable into any
/// credential-writing subcommand.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct SecretSourceArgs {
    /// Read the secret from stdin (trailing newline trimmed).
    #[arg(long)]
    pub stdin: bool,
    /// Read the secret from a named environment variable.
    #[arg(long, value_name = "VAR")]
    pub from_env: Option<String>,
}

impl SecretSourceArgs {
    /// Resolve the secret from the chosen source. `prompt_label` enables an
    /// interactive no-echo prompt as the fallback when neither flag is given;
    /// pass `None` to require an explicit source (headless commands).
    pub fn read(&self, prompt_label: Option<&str>) -> Result<Secret, CliError> {
        match (self.stdin, &self.from_env) {
            (true, Some(_)) => Err(CliError::Usage(
                "pass exactly one of --stdin or --from-env".into(),
            )),
            (true, None) => read_stdin(),
            (false, Some(var)) => read_from_env(var),
            (false, None) => match prompt_label {
                Some(label) => {
                    use std::io::IsTerminal;
                    if std::io::stdin().is_terminal() {
                        Secret::prompt(label)
                    } else {
                        read_stdin()
                    }
                }
                None => Err(CliError::Usage(
                    "provide the secret via --stdin or --from-env <VAR>".into(),
                )),
            },
        }
    }
}

/// Read exactly one secret from stdin (all of it, trailing newline trimmed).
/// The scriptable ingress path: `op read … | <bin> auth set-credential --stdin`.
pub fn read_stdin() -> Result<Secret, CliError> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| CliError::Other(format!("reading secret from stdin: {e}")))?;
    // Trim a single trailing newline (and CR) so heredocs/echo pipes work.
    let trimmed = buf.strip_suffix('\n').unwrap_or(&buf);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    Ok(Secret::new(trimmed.to_string()))
}

/// Read one secret from a named environment variable (`--from-env APP_PASSWORD`).
/// Bounded-scope ingress for `op run --`-style invocations.
pub fn read_from_env(var: &str) -> Result<Secret, CliError> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(Secret::new(v)),
        Ok(_) => Err(CliError::Usage(format!("${var} is set but empty"))),
        Err(_) => Err(CliError::Usage(format!("${var} is not set"))),
    }
}

/// A secret string that refuses to reveal itself via `Debug`/`Display` and is
/// zeroized from memory when dropped. Read it only at the point of use, with
/// [`Secret::expose`], and never log the result.
pub struct Secret {
    inner: String,
}

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret {
            inner: value.into(),
        }
    }

    /// Borrow the underlying secret. Use at the call site only — never log it.
    pub fn expose(&self) -> &str {
        &self.inner
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// No-echo interactive prompt. Caller must have already confirmed a TTY.
    pub fn prompt(label: &str) -> Result<Secret, CliError> {
        let v = rpassword::prompt_password(format!("{label}: "))
            .map_err(|e| CliError::Other(format!("reading password: {e}")))?;
        Ok(Secret::new(v))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***redacted***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***redacted***")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

/// OS-keychain-backed credential store. The only runtime source of secrets.
/// `for_binary("fpl")` yields the family service name `piekstra.fpl`.
pub struct CredentialStore {
    service: String,
}

impl CredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        CredentialStore {
            service: service.into(),
        }
    }

    /// Family convention: service name `piekstra.<binary>` (SPEC v1 §1.7).
    pub fn for_binary(binary: &str) -> Self {
        CredentialStore::new(format!("piekstra.{binary}"))
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self, account: &str) -> Result<Entry, CliError> {
        Entry::new(&self.service, account)
            .map_err(|e| CliError::Keychain(format!("opening keychain entry: {e}")))
    }

    /// Keychain only. `None` if no entry exists.
    pub fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(Secret::new(p))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CliError::Keychain(format!("reading credential: {e}"))),
        }
    }

    /// Store (or overwrite) a credential in the keychain.
    pub fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
        self.entry(account)?
            .set_password(secret.expose())
            .map_err(|e| CliError::Keychain(format!("storing credential: {e}")))
    }

    /// Delete a credential. Returns `true` if something was removed, `false`
    /// if there was nothing stored.
    pub fn delete(&self, account: &str) -> Result<bool, CliError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(CliError::Keychain(format!("deleting credential: {e}"))),
        }
    }

    /// Read a typed JSON item — the one-item-per-credential-set layout (see
    /// the crate docs). `None` only when no item exists.
    ///
    /// An item that is present but does not parse as `T` is an **error**
    /// naming the account, never `None`: one blob now carries the whole
    /// session, so a swallowed parse failure would surface as an unexplained
    /// logout — and, worse, the next `auth login` would silently overwrite
    /// whatever the unreadable item held.
    pub fn get_json<T: DeserializeOwned>(&self, account: &str) -> Result<Option<T>, CliError> {
        get_json(self, account)
    }

    /// Store (or overwrite) a value as one JSON item.
    pub fn set_json<T: Serialize>(&self, account: &str, value: &T) -> Result<(), CliError> {
        set_json(self, account, value)
    }

    /// Move items from `legacy` into this store, for the family-wide rename
    /// to `piekstra.<bin>` (SPEC §1.7: existing entries migrated on first
    /// run). `accounts` pairs each legacy account name with its name here;
    /// returns how many items were copied.
    ///
    /// Per pair, the order is read old → write new → delete old, so a failure
    /// partway leaves the legacy item intact and the next run retries. When
    /// this store already holds the account, that item is kept (a write under
    /// the new layout is the newer credential) and the legacy copy is still
    /// removed; such pairs are not counted. The migration therefore
    /// converges: once every legacy item is gone, a call costs one
    /// no-entry lookup per pair and no macOS prompt, so it is safe to run on
    /// every start. A pair whose two sides are the same item is skipped.
    pub fn migrate_from(
        &self,
        legacy: &CredentialStore,
        accounts: &[(&str, &str)],
    ) -> Result<usize, CliError> {
        migrate(legacy, self, accounts)
    }
}

/// The three operations a keychain item supports, as a seam: the typed-JSON
/// and migration logic is written against this so it can be exercised
/// against an in-memory store in tests. A test that read the real keychain
/// would prompt on every macOS run of an ad-hoc-signed test binary.
trait ItemStore {
    fn service(&self) -> &str;
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError>;
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError>;
    fn delete(&self, account: &str) -> Result<bool, CliError>;
}

impl ItemStore for CredentialStore {
    fn service(&self) -> &str {
        CredentialStore::service(self)
    }
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        CredentialStore::get(self, account)
    }
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
        CredentialStore::set(self, account, secret)
    }
    fn delete(&self, account: &str) -> Result<bool, CliError> {
        CredentialStore::delete(self, account)
    }
}

fn get_json<T: DeserializeOwned>(
    store: &impl ItemStore,
    account: &str,
) -> Result<Option<T>, CliError> {
    match store.get(account)? {
        None => Ok(None),
        Some(raw) => decode(store.service(), account, raw.expose()).map(Some),
    }
}

fn set_json<T: Serialize>(
    store: &impl ItemStore,
    account: &str,
    value: &T,
) -> Result<(), CliError> {
    store.set(account, &encode(value)?)
}

/// The item codec, keychain-free: a value becomes the JSON text of one item.
fn encode<T: Serialize>(value: &T) -> Result<Secret, CliError> {
    serde_json::to_string(value)
        .map(Secret::new)
        .map_err(|e| CliError::Other(format!("serializing credential item: {e}")))
}

/// The inverse of [`encode`]. The message names the item so the user can
/// tell *which* stored credential is unreadable, and says what to do about
/// it (the item is theirs to clear; this crate never deletes on a parse
/// failure).
fn decode<T: DeserializeOwned>(service: &str, account: &str, raw: &str) -> Result<T, CliError> {
    serde_json::from_str(raw).map_err(|e| {
        CliError::Keychain(format!(
            "stored item `{account}` under `{service}` is not the shape this build expects ({e}); \
             clear it with `auth logout --forget` and log in again"
        ))
    })
}

fn migrate(
    from: &impl ItemStore,
    to: &impl ItemStore,
    accounts: &[(&str, &str)],
) -> Result<usize, CliError> {
    let mut moved = 0;
    for (old, new) in accounts {
        if from.service() == to.service() && old == new {
            continue;
        }
        let Some(secret) = from.get(old)? else {
            continue;
        };
        if to.get(new)?.is_none() {
            to.set(new, &secret)?;
            moved += 1;
        }
        from.delete(old)?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    /// In-memory stand-in for the OS keychain. `fail_set` simulates a write
    /// that errors, for the migration-ordering rail.
    struct MemStore {
        service: String,
        items: RefCell<HashMap<String, String>>,
        fail_set: Cell<bool>,
    }

    impl MemStore {
        fn new(service: &str) -> Self {
            MemStore {
                service: service.into(),
                items: RefCell::new(HashMap::new()),
                fail_set: Cell::new(false),
            }
        }
        fn with(self, account: &str, value: &str) -> Self {
            self.items.borrow_mut().insert(account.into(), value.into());
            self
        }
        fn raw(&self, account: &str) -> Option<String> {
            self.items.borrow().get(account).cloned()
        }
    }

    impl ItemStore for MemStore {
        fn service(&self) -> &str {
            &self.service
        }
        fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
            Ok(self.raw(account).map(Secret::new))
        }
        fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
            if self.fail_set.get() {
                return Err(CliError::Keychain("simulated write failure".into()));
            }
            self.items
                .borrow_mut()
                .insert(account.into(), secret.expose().into());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<bool, CliError> {
            Ok(self.items.borrow_mut().remove(account).is_some())
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Session {
        token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh: Option<String>,
        expires_at: u64,
    }

    fn session() -> Session {
        Session {
            token: "tok".into(),
            refresh: Some("ref".into()),
            expires_at: 1_800_000_000,
        }
    }

    #[test]
    fn secret_redacts_but_exposes_on_demand() {
        let s = Secret::new("super-secret-token");
        assert_eq!(format!("{s}"), "***redacted***");
        assert_eq!(format!("{s:?}"), "Secret(***redacted***)");
        assert_eq!(s.expose(), "super-secret-token");
        assert!(!s.is_empty());
        assert!(Secret::new("").is_empty());
    }

    #[test]
    fn store_service_convention() {
        assert_eq!(CredentialStore::for_binary("fpl").service(), "piekstra.fpl");
    }

    #[test]
    fn env_ingestion() {
        std::env::set_var("PK_CLI_TEST_SECRET", "hunter2");
        assert_eq!(
            read_from_env("PK_CLI_TEST_SECRET").unwrap().expose(),
            "hunter2"
        );
        assert!(read_from_env("PK_CLI_TEST_UNSET_VAR").is_err());
    }

    #[test]
    fn json_codec_round_trips() {
        let blob = encode(&session()).unwrap();
        assert!(blob.expose().starts_with('{'));
        let back: Session = decode("piekstra.x", "session", blob.expose()).unwrap();
        assert_eq!(back, session());
    }

    #[test]
    fn decode_names_the_item_and_is_a_keychain_error() {
        let err = decode::<Session>("piekstra.tplc", "session", "{not json").unwrap_err();
        assert!(matches!(err, CliError::Keychain(_)));
        let msg = err.to_string();
        assert!(msg.contains("`session`"), "{msg}");
        assert!(msg.contains("`piekstra.tplc`"), "{msg}");
        // A bare legacy value (a token string stored before the JSON layout)
        // is the realistic failure and must not read as valid.
        assert!(decode::<Session>("s", "a", "aas_et/raw-token").is_err());
        assert!(decode::<Session>("s", "a", "").is_err());
    }

    #[test]
    fn get_json_is_none_only_when_absent() {
        let store = MemStore::new("piekstra.x");
        assert_eq!(get_json::<Session>(&store, "session").unwrap(), None);
    }

    #[test]
    fn set_json_then_get_json() {
        let store = MemStore::new("piekstra.x");
        set_json(&store, "session", &session()).unwrap();
        assert_eq!(
            get_json::<Session>(&store, "session").unwrap(),
            Some(session())
        );
        // Overwrite is a plain set.
        let newer = Session {
            token: "tok2".into(),
            ..session()
        };
        set_json(&store, "session", &newer).unwrap();
        assert_eq!(get_json::<Session>(&store, "session").unwrap(), Some(newer));
    }

    #[test]
    fn get_json_refuses_to_call_an_unreadable_item_absent() {
        let store = MemStore::new("piekstra.x").with("session", "not-json");
        let err = get_json::<Session>(&store, "session").unwrap_err();
        assert!(matches!(err, CliError::Keychain(_)));
        // And it is still there for the user to deal with.
        assert_eq!(store.raw("session").as_deref(), Some("not-json"));
    }

    #[test]
    fn migrate_copies_then_deletes_and_counts() {
        let old = MemStore::new("tplc")
            .with("token", "k")
            .with("username", "u");
        let new = MemStore::new("piekstra.tplc");
        let n = migrate(
            &old,
            &new,
            &[
                ("token", "token"),
                ("username", "user"),
                ("missing", "missing"),
            ],
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(new.raw("token").as_deref(), Some("k"));
        assert_eq!(new.raw("user").as_deref(), Some("u"));
        assert!(new.raw("missing").is_none());
        assert!(
            old.items.borrow().is_empty(),
            "legacy items must be removed"
        );
        // Second run is a no-op: nothing left to move, nothing written.
        assert_eq!(migrate(&old, &new, &[("token", "token")]).unwrap(), 0);
    }

    #[test]
    fn migrate_keeps_an_existing_destination_and_still_retires_the_legacy_copy() {
        let old = MemStore::new("tplc").with("session", "stale");
        let new = MemStore::new("piekstra.tplc").with("session", "fresh");
        let n = migrate(&old, &new, &[("session", "session")]).unwrap();
        assert_eq!(n, 0);
        assert_eq!(new.raw("session").as_deref(), Some("fresh"));
        assert!(old.raw("session").is_none());
    }

    #[test]
    fn migrate_leaves_legacy_intact_when_the_new_write_fails() {
        let old = MemStore::new("tplc").with("token", "k");
        let new = MemStore::new("piekstra.tplc");
        new.fail_set.set(true);
        assert!(migrate(&old, &new, &[("token", "token")]).is_err());
        assert_eq!(old.raw("token").as_deref(), Some("k"));
        assert!(new.raw("token").is_none());
        // Once the write works, the retry completes the move.
        new.fail_set.set(false);
        assert_eq!(migrate(&old, &new, &[("token", "token")]).unwrap(), 1);
        assert!(old.raw("token").is_none());
    }

    #[test]
    fn migrate_skips_a_pair_that_is_the_same_item() {
        // Same service, same account: "delete old" would delete the only copy.
        let store = MemStore::new("piekstra.x").with("session", "k");
        assert_eq!(
            migrate(&store, &store, &[("session", "session")]).unwrap(),
            0
        );
        assert_eq!(store.raw("session").as_deref(), Some("k"));
        // Same service, different account is a legitimate rename.
        assert_eq!(migrate(&store, &store, &[("session", "main")]).unwrap(), 1);
        assert!(store.raw("session").is_none());
        assert_eq!(store.raw("main").as_deref(), Some("k"));
    }
}
