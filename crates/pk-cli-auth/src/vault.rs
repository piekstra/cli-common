//! One keychain item per CLI, however many secrets it keeps.
//!
//! macOS asks once per keychain item for each newly built or upgraded binary
//! (the partition list holds the binary's `cdhash:` when it has no team ID;
//! see AGENTS.md's signing notes). A CLI that keeps one item per sign-in
//! therefore asks once per sign-in after every upgrade, and an unattended run
//! after an upgrade waits on a dialog nobody answers. A [`Vault`] keeps one
//! item, [`KEY_ACCOUNT`], holding a random 256-bit key, and every secret in
//! its own file encrypted with that key (XChaCha20-Poly1305), so a new binary
//! asks once, whatever the number of secrets.
//!
//! - **Where.** `~/.config/<bin>/vault/` ([`Vault::for_binary`]), anchored
//!   to the home directory alone, as [`RefreshLock`](crate::refresh_lock)
//!   is, so a launchd agent and a terminal open the same vault. The directory
//!   is owner-only (0700) and each entry file 0600.
//! - **What a file holds.** `PKV1`, a 24-byte random nonce, and the
//!   ciphertext of `{"name", "value"}`. The file is named by the SHA-256 of
//!   the entry's name, so no name is on disk in the clear. The hash is not
//!   keyed (a read of a missing entry would otherwise need the key), so
//!   someone who can list the owner-only directory can test a guessed name,
//!   never read one. The file name is
//!   the authenticated data, and the decrypted name must be the one asked
//!   for, so a file copied over another's reads as an error, never as the
//!   other entry.
//! - **Writes** go to a temporary file, synced, then renamed over the entry,
//!   so a crash leaves the old entry or the new one. Serialise writes to one
//!   entry the way its CLI already does (a session under the refresh lock):
//!   the vault keeps entries apart, so a write to one never touches another.
//! - **The key** is read from the keychain once per process, and only when an
//!   entry exists or is written: a read with nothing stored costs no keychain
//!   read, so a logged-out run raises no dialog and a test pointed at an
//!   empty directory reads nothing. It is created by the first write, under a
//!   lock in the directory so two processes cannot create two keys, and never
//!   while entries exist: those would be lost to a new key, so a missing key
//!   with entries present is an error naming the remedy. A random key id
//!   beside the entries changes whenever a key is made or the vault cleared:
//!   a write holds the key lock and checks it, so a long-running process
//!   whose cached key another process cleared makes a new key rather than
//!   seal an entry nothing can read, and a read that fails to decrypt
//!   reloads the key once when the id moved.
//!
//! - **Moving in and resetting.** [`Vault::migrate_from`] moves a CLI's
//!   old keychain items in, deleting each only once its copy reads back.
//!   [`Vault::clear`] removes every entry and the key, the reset after a
//!   lost key; one damaged entry needs only [`Vault::delete`], and
//!   [`Vault::list`] names it while listing the others.
//!
//! Tests give [`Vault::new`] a [`MemoryKeys`] and a directory of their own;
//! a CLI's tests give it their `<PREFIX>_KEYCHAIN_SERVICE` override and a
//! temporary directory, so nothing reads the real keychain or vault.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use pk_cli_core::private_file::{self, Replace};
use pk_cli_core::CliError;
use pk_cli_secrets::{CredentialStore, Secret};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// The keychain account holding the vault's key.
pub const KEY_ACCOUNT: &str = "vault-key";

const MAGIC: &[u8; 4] = b"PKV1";
const NONCE_LEN: usize = 24;
const EXTENSION: &str = "pkv";
const KEY_FORMAT: u32 = 1;
const LOCK_FILE: &str = ".key.lock";
/// A random id that changes whenever the key is made or the vault cleared,
/// so a process holding a cached key sees it was replaced.
const ID_FILE: &str = ".key.id";

/// The remedy of an entry that does not decrypt, for a CLI to replace with
/// its own command: remove that one entry ([`Vault::delete`]) and sign in
/// again. The CLI's other entries are unaffected.
pub const REMEDY_DELETE_ENTRY: &str =
    "remove this sign-in and sign in again; the others are unaffected";

/// The remedy of a key that is missing or is not a key, for a CLI to
/// replace with its own command: [`Vault::clear`] and sign in again, since
/// nothing can read the entries without their key.
pub const REMEDY_CLEAR: &str =
    "remove every sign-in this CLI keeps and sign in again: none can be read without the vault's key";

/// How long a first write, or [`Vault::clear`], waits by default for
/// another process holding the key lock.
pub const KEY_WAIT: Duration = Duration::from_secs(30);

/// Where the vault's key item lives: the keychain, or memory in a test.
/// Also the store [`Vault::migrate_from`] moves items out of.
pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError>;
    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError>;
    /// `true` when there was an item to delete.
    fn delete(&self, account: &str) -> Result<bool, CliError>;
}

impl KeyStore for CredentialStore {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        CredentialStore::get(self, account)
    }

    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        CredentialStore::set(self, account, value)
    }

    fn delete(&self, account: &str) -> Result<bool, CliError> {
        CredentialStore::delete(self, account)
    }
}

/// A key store in memory, for tests. It counts reads and writes, so a test
/// can show which operations reach the keychain.
#[derive(Default)]
pub struct MemoryKeys {
    items: Mutex<HashMap<String, String>>,
    reads: Mutex<usize>,
    writes: Mutex<usize>,
}

impl MemoryKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times the key item was read.
    pub fn reads(&self) -> usize {
        *self.reads.lock().unwrap()
    }

    /// How many times the key item was written.
    pub fn writes(&self) -> usize {
        *self.writes.lock().unwrap()
    }

    /// Drop the key item, as a keychain reset would.
    pub fn forget(&self) {
        self.items.lock().unwrap().clear();
    }
}

impl KeyStore for MemoryKeys {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        *self.reads.lock().unwrap() += 1;
        Ok(self.items.lock().unwrap().get(account).map(Secret::new))
    }

    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        *self.writes.lock().unwrap() += 1;
        self.items
            .lock()
            .unwrap()
            .insert(account.to_string(), value.expose().to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<bool, CliError> {
        Ok(self.items.lock().unwrap().remove(account).is_some())
    }
}

impl<K: KeyStore + ?Sized> KeyStore for std::sync::Arc<K> {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        (**self).get(account)
    }

    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        (**self).set(account, value)
    }

    fn delete(&self, account: &str) -> Result<bool, CliError> {
        (**self).delete(account)
    }
}

/// What [`Vault::list`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The names of the entries that read, sorted.
    pub names: Vec<String>,
    /// The files that do not decrypt with the key, sorted. Each holds one
    /// entry, which only signing in again replaces.
    pub unreadable: Vec<PathBuf>,
}

/// What [`Vault::migrate_from`] did, pair by pair.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Migration {
    /// Copied into the vault and deleted from the old store.
    pub moved: usize,
    /// Already in the vault: that entry kept and the old item deleted.
    pub kept: usize,
    /// In neither: nothing to do.
    pub absent: usize,
}

#[derive(Serialize, Deserialize)]
struct KeyItem {
    format: u32,
    /// 32 bytes, hex.
    key: String,
}

/// What an entry seals, as read back.
#[derive(Deserialize)]
struct Plain {
    name: String,
    value: String,
}

/// What an entry seals, as written: borrowed, so no copy of the value is
/// left behind unzeroed.
#[derive(Serialize)]
struct PlainRef<'a> {
    name: &'a str,
    value: &'a str,
}

type Key = Zeroizing<[u8; 32]>;

/// The key as this process read it, with the vault's key id at the time.
struct Cached {
    key: Key,
    id: Option<String>,
}

/// Secrets in owner-only encrypted files, under one keychain key. See the
/// [module docs](self).
pub struct Vault {
    keys: Box<dyn KeyStore>,
    dir: PathBuf,
    key: Mutex<Option<Cached>>,
    wait: Duration,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// The CLI's vault: its key in `piekstra.<bin>` at [`KEY_ACCOUNT`], its
    /// entries in `~/.config/<bin>/vault/`.
    pub fn for_binary(bin: &str) -> Result<Self, CliError> {
        Ok(Self::new(
            CredentialStore::for_binary(bin),
            default_dir(bin)?,
        ))
    }

    /// A vault with its key in `keys` and its entries in `dir`: a CLI's
    /// keychain-service override, or a test's [`MemoryKeys`].
    pub fn new(keys: impl KeyStore + 'static, dir: impl Into<PathBuf>) -> Self {
        Self {
            keys: Box::new(keys),
            dir: dir.into(),
            key: Mutex::new(None),
            wait: KEY_WAIT,
        }
    }

    /// Wait at most `wait` for the key lock instead of [`KEY_WAIT`].
    pub fn wait_at_most(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The entry `name`, or `None` when nothing is stored under it. Reads
    /// the keychain only when the entry exists.
    pub fn get(&self, name: &str) -> Result<Option<Secret>, CliError> {
        let path = self.entry_path(name);
        let sealed = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("reading", &path, &e)),
        };
        let Some(key) = self.read_side_key()? else {
            return Err(key_lost(&self.dir));
        };
        let stem = file_stem(&path);
        let read = |key: &Key| open(key, &stem, &sealed).filter(|plain| plain.name == name);
        let plain = match read(&key) {
            Some(plain) => plain,
            // Another process may have cleared the vault and made a new key.
            None => self
                .reload_if_replaced()?
                .and_then(|newer| read(&newer))
                .ok_or_else(|| unreadable(&path))?,
        };
        Ok(Some(Secret::new(plain.value)))
    }

    /// Store `value` under `name`, replacing what was there.
    pub fn put(&self, name: &str, value: &Secret) -> Result<(), CliError> {
        self.ensure_dir()?;
        // Held through the write, so a `clear` in another process cannot
        // remove the key between this check and the entry landing.
        let _lock = self.lock_key()?;
        let key = self.write_side_key()?;
        let path = self.entry_path(name);
        let plain = Zeroizing::new(
            serde_json::to_vec(&PlainRef {
                name,
                value: value.expose(),
            })
            .map_err(|e| CliError::Other(format!("encoding a vault entry: {e}")))?,
        );
        let sealed = seal(&key, &file_stem(&path), &plain)?;
        write_atomically(&path, &sealed)
    }

    /// Delete the entry `name`. `true` when one was there. Needs no key.
    pub fn delete(&self, name: &str) -> Result<bool, CliError> {
        let path = self.entry_path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error("deleting", &path, &e)),
        }
    }

    /// Every entry's name, sorted, leaving out entries that do not decrypt
    /// ([`list`](Self::list) names those). Reads the keychain only when
    /// there are entries.
    pub fn names(&self) -> Result<Vec<String>, CliError> {
        Ok(self.list()?.names)
    }

    /// The entries' names, sorted, and the files that do not decrypt: one
    /// damaged entry leaves the others listed. A key that is missing while
    /// entries exist is still an error, since no entry can be read.
    pub fn list(&self) -> Result<Listing, CliError> {
        let entries = self.entry_files()?;
        if entries.is_empty() {
            return Ok(Listing::default());
        }
        let key = self.read_side_key()?.ok_or_else(|| key_lost(&self.dir))?;
        let listing = list_with(&key, &entries)?;
        if listing.unreadable.is_empty() {
            return Ok(listing);
        }
        // Another process may have cleared the vault and made a new key.
        match self.reload_if_replaced()? {
            Some(newer) => list_with(&newer, &self.entry_files()?),
            None => Ok(listing),
        }
    }

    /// Remove every entry and the key item: the reset after a lost or
    /// damaged key, and a CLI's "forget every sign-in". The next write makes
    /// a new key. Takes the key lock, so it does not race a first write; run
    /// it when no other run of the CLI is using the vault. How many entries
    /// were removed.
    ///
    /// The directory keeps two files, neither secret, both `0600`. One is
    /// `.key.lock`, empty: removing a lock file another run is waiting on
    /// would give the two of them different locks. The other is `.key.id`,
    /// renewed here: a process that cached the old key sees it changed and
    /// makes a new key on its next write, rather than sealing with the
    /// deleted one. A removed id could not tell such a process that its key
    /// is gone if it cached the key before any id existed.
    pub fn clear(&self) -> Result<usize, CliError> {
        let mut removed = 0;
        // The lock before the cache, the order `put` takes them in.
        let held = if self.dir.exists() {
            Some(self.lock_key()?)
        } else {
            None
        };
        let mut cached = self.key.lock().unwrap();
        if held.is_some() {
            for path in self.entry_files()? {
                fs::remove_file(&path).map_err(|e| io_error("deleting", &path, &e))?;
                removed += 1;
            }
            for path in self.temp_files()? {
                let _ = fs::remove_file(&path);
            }
            // A new id, so a process that cached the old key makes a new
            // one on its next write instead of sealing with the old.
            self.write_id()?;
        }
        self.keys.delete(KEY_ACCOUNT)?;
        *cached = None;
        Ok(removed)
    }

    /// Move items from `from` (the keychain items a CLI kept before the
    /// vault) into entries here. `pairs` maps each item's account to its
    /// entry's name. Per pair: an entry already here is kept (it is the
    /// newer: nothing writes the old item once a CLI has moved) and the item
    /// deleted; otherwise the item is read, written here, read back and
    /// compared, and only then deleted. A failure leaves the item in place,
    /// so the next run picks up where this one stopped; a run with nothing
    /// left to move reads no item, only looks up absent ones, which raises
    /// no dialog on macOS. Hold whatever lock the CLI writes these secrets
    /// under (a session's refresh lock) around the call.
    ///
    /// Each item read by a newly built binary costs one macOS dialog, once.
    /// The previous release reads only the items, so once they are moved a
    /// downgrade finds no sign-in. They are not kept for it: a renewal after
    /// the move rotates the refresh token, and the item's would be stale.
    pub fn migrate_from(
        &self,
        from: &dyn KeyStore,
        pairs: &[(&str, &str)],
    ) -> Result<Migration, CliError> {
        let mut done = Migration::default();
        for &(account, name) in pairs {
            if self.get(name)?.is_some() {
                from.delete(account)?;
                done.kept += 1;
                continue;
            }
            let Some(item) = from.get(account)? else {
                done.absent += 1;
                continue;
            };
            self.put(name, &item)?;
            let same = self
                .get(name)?
                .is_some_and(|back| back.expose() == item.expose());
            if !same {
                return Err(CliError::Other(
                    "a secret written to the vault did not read back the same; the keychain item \
                     is kept"
                        .into(),
                ));
            }
            from.delete(account)?;
            done.moved += 1;
        }
        Ok(done)
    }

    /// The entry `name` as JSON. One that is present but does not parse as
    /// `T` is an error, never `None`, as [`CredentialStore::get_json`].
    pub fn get_json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, CliError> {
        let Some(secret) = self.get(name)? else {
            return Ok(None);
        };
        serde_json::from_str(secret.expose())
            .map(Some)
            .map_err(|e| {
                CliError::Other(format!(
                    "a vault entry does not read: line {}, column {}",
                    e.line(),
                    e.column()
                ))
            })
    }

    /// Store `value` as JSON under `name`.
    pub fn put_json<T: Serialize>(&self, name: &str, value: &T) -> Result<(), CliError> {
        let text = Zeroizing::new(
            serde_json::to_string(value)
                .map_err(|e| CliError::Other(format!("encoding a vault entry: {e}")))?,
        );
        self.put(name, &Secret::new(text.as_str()))
    }

    fn entry_path(&self, name: &str) -> PathBuf {
        let digest = Sha256::new()
            .chain_update(b"pk-cli-vault/1\0")
            .chain_update(name.as_bytes())
            .finalize();
        self.dir.join(format!("{}.{EXTENSION}", hex(&digest)))
    }

    fn entry_files(&self) -> Result<Vec<PathBuf>, CliError> {
        let read = match fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error("listing", &self.dir, &e)),
        };
        let mut files = Vec::new();
        for entry in read {
            let path = entry
                .map_err(|e| io_error("listing", &self.dir, &e))?
                .path();
            if path.extension().is_some_and(|x| x == EXTENSION) {
                files.push(path);
            }
        }
        Ok(files)
    }

    /// The key for a read: the one this process cached, else the
    /// keychain's. `None` when the keychain has none.
    fn read_side_key(&self) -> Result<Option<Key>, CliError> {
        let mut cached = self.key.lock().unwrap();
        if let Some(c) = cached.as_ref() {
            return Ok(Some(c.key.clone()));
        }
        let id = self.current_id()?;
        let Some(key) = self.read_key()? else {
            return Ok(None);
        };
        *cached = Some(Cached {
            key: key.clone(),
            id,
        });
        Ok(Some(key))
    }

    /// After a read failed to decrypt: the keychain's key when the vault's
    /// key id moved since this process cached its key (another process
    /// cleared the vault and made a new one), else `None`.
    fn reload_if_replaced(&self) -> Result<Option<Key>, CliError> {
        let mut cached = self.key.lock().unwrap();
        let id = self.current_id()?;
        if cached.as_ref().is_some_and(|c| c.id == id) {
            return Ok(None);
        }
        *cached = None;
        let Some(key) = self.read_key()? else {
            return Ok(None);
        };
        *cached = Some(Cached {
            key: key.clone(),
            id,
        });
        Ok(Some(key))
    }

    /// The key for a write, holding the key lock: the cached one while the
    /// vault's key id is unchanged, else the keychain's, else a new one when
    /// the vault has no entries. Entries without a key are an error.
    fn write_side_key(&self) -> Result<Key, CliError> {
        let mut cached = self.key.lock().unwrap();
        let id = self.current_id()?;
        if let Some(c) = cached.as_ref().filter(|c| c.id == id) {
            return Ok(c.key.clone());
        }
        *cached = None;
        if let Some(key) = self.read_key()? {
            *cached = Some(Cached {
                key: key.clone(),
                id,
            });
            return Ok(key);
        }
        if !self.entry_files()?.is_empty() {
            return Err(key_lost(&self.dir));
        }
        let mut fresh: Key = Zeroizing::new([0u8; 32]);
        getrandom::fill(fresh.as_mut()).map_err(|e| {
            CliError::Other(format!("drawing the vault's key from the system: {e}"))
        })?;
        let item = Zeroizing::new(
            serde_json::to_string(&KeyItem {
                format: KEY_FORMAT,
                key: hex(fresh.as_ref()).to_string(),
            })
            .map_err(|e| CliError::Other(format!("encoding the vault's key: {e}")))?,
        );
        self.keys.set(KEY_ACCOUNT, &Secret::new(item.as_str()))?;
        // Read it back: a store that dropped the write would otherwise seal
        // entries with a key nothing can find again.
        match self.read_key()? {
            Some(stored) if *stored == *fresh => {}
            _ => {
                return Err(CliError::Keychain(
                    "the vault's new key did not read back from the keychain".into(),
                ))
            }
        }
        let id = self.write_id()?;
        *cached = Some(Cached {
            key: fresh.clone(),
            id: Some(id),
        });
        Ok(fresh)
    }

    /// The vault's key id, `None` before the first key.
    fn current_id(&self) -> Result<Option<String>, CliError> {
        let path = self.dir.join(ID_FILE);
        match fs::read_to_string(&path) {
            Ok(id) => Ok(Some(id.trim().to_string())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_error("reading", &path, &e)),
        }
    }

    /// Give the vault a new key id; under the key lock.
    fn write_id(&self) -> Result<String, CliError> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw)
            .map_err(|e| CliError::Other(format!("drawing a key id from the system: {e}")))?;
        let id = hex(&raw);
        write_atomically(&self.dir.join(ID_FILE), id.as_bytes())?;
        Ok(id)
    }

    /// The key lock, waited for at most `self.wait`: a holder stalled on a
    /// keychain dialog must not hang an unattended run. Released on drop.
    /// The lock file is created `0600`, as `RefreshLock`'s is; one that
    /// v0.33.0 or v0.33.1 created at the umask's mode is narrowed.
    fn lock_key(&self) -> Result<File, CliError> {
        let path = self.dir.join(LOCK_FILE);
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|e| io_error("opening", &path, &e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file
                .metadata()
                .is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
            {
                let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
            }
        }
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::Error(e)) => return Err(io_error("locking", &path, &e)),
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= self.wait {
                        return Err(CliError::Upstream(format!(
                            "another process has held {} for over {:?}, making or removing the \
                             vault's key (a keychain prompt waiting for an answer keeps it there)",
                            path.display(),
                            self.wait
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn temp_files(&self) -> Result<Vec<PathBuf>, CliError> {
        let read = fs::read_dir(&self.dir).map_err(|e| io_error("listing", &self.dir, &e))?;
        Ok(read
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|x| x == "tmp"))
            .collect())
    }

    fn read_key(&self) -> Result<Option<Key>, CliError> {
        let Some(secret) = self.keys.get(KEY_ACCOUNT)? else {
            return Ok(None);
        };
        let item: KeyItem = serde_json::from_str(secret.expose()).map_err(|_| bad_key())?;
        if item.format != KEY_FORMAT {
            return Err(bad_key());
        }
        let mut key: Key = Zeroizing::new([0u8; 32]);
        unhex(&item.key, key.as_mut()).ok_or_else(bad_key)?;
        Ok(Some(key))
    }

    fn ensure_dir(&self) -> Result<(), CliError> {
        fs::create_dir_all(&self.dir).map_err(|e| io_error("creating", &self.dir, &e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| io_error("restricting", &self.dir, &e))?;
        }
        Ok(())
    }
}

/// `~/.config/<bin>/vault`.
pub fn default_dir(bin: &str) -> Result<PathBuf, CliError> {
    let home = crate::refresh_lock::home_from(|name| std::env::var_os(name)).ok_or_else(|| {
        CliError::Other("cannot locate the home directory for the vault ($HOME unset)".into())
    })?;
    Ok(home.join(".config").join(bin).join("vault"))
}

/// The names in `entries` that decrypt with `key`, and the files that do not.
fn list_with(key: &Key, entries: &[PathBuf]) -> Result<Listing, CliError> {
    let mut listing = Listing::default();
    for path in entries {
        let sealed = fs::read(path).map_err(|e| io_error("reading", path, &e))?;
        match open(key, &file_stem(path), &sealed) {
            Some(plain) => listing.names.push(plain.name),
            None => listing.unreadable.push(path.clone()),
        }
    }
    listing.names.sort();
    listing.unreadable.sort();
    Ok(listing)
}

fn seal(key: &Key, stem: &str, plain: &[u8]) -> Result<Vec<u8>, CliError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce)
        .map_err(|e| CliError::Other(format!("drawing a nonce from the system: {e}")))?;
    let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_ref()));
    let sealed = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: stem.as_bytes(),
            },
        )
        .map_err(|_| CliError::Other("sealing a vault entry failed".into()))?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

fn open(key: &Key, stem: &str, sealed: &[u8]) -> Option<Plain> {
    let body = sealed.strip_prefix(MAGIC.as_slice())?;
    if body.len() < NONCE_LEN {
        return None;
    }
    let (nonce, ciphertext) = body.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key.as_ref()));
    let plain = Zeroizing::new(
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: stem.as_bytes(),
                },
            )
            .ok()?,
    );
    serde_json::from_slice(&plain).ok()
}

/// Through the family's one owner-only writer: a `0600` temporary file,
/// synced, renamed over `path`, the directory synced.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    private_file::write(path, bytes, Replace::Always).map_err(|e| io_error("writing", path, &e))
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str, out: &mut [u8]) -> Option<()> {
    if text.len() != out.len() * 2 {
        return None;
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(())
}

fn io_error(doing: &str, path: &Path, e: &std::io::Error) -> CliError {
    CliError::Other(format!("{doing} {}: {e}", path.display()))
}

fn unreadable(path: &Path) -> CliError {
    CliError::Other(format!(
        "the vault entry {} does not decrypt with this vault's key",
        path.display()
    ))
    .with_remedy(REMEDY_DELETE_ENTRY)
}

fn key_lost(dir: &Path) -> CliError {
    CliError::Keychain(format!(
        "the vault at {} has entries but its key ({KEY_ACCOUNT}) is not in the keychain",
        dir.display()
    ))
    .with_remedy(REMEDY_CLEAR)
}

fn bad_key() -> CliError {
    CliError::Keychain(format!(
        "the keychain item {KEY_ACCOUNT} is not a vault key"
    ))
    .with_remedy(REMEDY_CLEAR)
}

#[cfg(test)]
mod tests;
