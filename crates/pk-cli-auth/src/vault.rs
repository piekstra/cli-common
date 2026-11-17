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
//!   with entries present is an error naming the remedy.
//!
//! Tests give [`Vault::new`] a [`MemoryKeys`] and a directory of their own;
//! a CLI's tests give it their `<PREFIX>_KEYCHAIN_SERVICE` override and a
//! temporary directory, so nothing reads the real keychain or vault.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
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

/// What to do about an entry the vault cannot read, for a CLI to replace.
pub const REMEDY_CLEAR: &str =
    "delete the vault directory's entries and sign in again; nothing else can read them";

/// Where the vault's key item lives: the keychain, or memory in a test.
pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError>;
    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError>;
}

impl KeyStore for CredentialStore {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        CredentialStore::get(self, account)
    }

    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        CredentialStore::set(self, account, value)
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
}

impl<K: KeyStore + ?Sized> KeyStore for std::sync::Arc<K> {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        (**self).get(account)
    }

    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError> {
        (**self).set(account, value)
    }
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

/// Secrets in owner-only encrypted files, under one keychain key. See the
/// [module docs](self).
pub struct Vault {
    keys: Box<dyn KeyStore>,
    dir: PathBuf,
    key: Mutex<Option<Key>>,
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
        }
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
        let Some(key) = self.key(false)? else {
            return Err(key_lost(&self.dir));
        };
        let plain = open(&key, &file_stem(&path), &sealed).ok_or_else(|| unreadable(&path))?;
        if plain.name != name {
            return Err(unreadable(&path));
        }
        Ok(Some(Secret::new(plain.value)))
    }

    /// Store `value` under `name`, replacing what was there.
    pub fn put(&self, name: &str, value: &Secret) -> Result<(), CliError> {
        self.ensure_dir()?;
        let key = self.key(true)?.ok_or_else(|| key_lost(&self.dir))?;
        let path = self.entry_path(name);
        let plain = Zeroizing::new(
            serde_json::to_vec(&PlainRef {
                name,
                value: value.expose(),
            })
            .map_err(|e| CliError::Other(format!("encoding a vault entry: {e}")))?,
        );
        let sealed = seal(&key, &file_stem(&path), &plain)?;
        write_atomically(&self.dir, &path, &sealed)
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

    /// Every entry's name, sorted. Reads the keychain only when there are
    /// entries.
    pub fn names(&self) -> Result<Vec<String>, CliError> {
        let entries = self.entry_files()?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let key = self.key(false)?.ok_or_else(|| key_lost(&self.dir))?;
        let mut names = Vec::with_capacity(entries.len());
        for path in entries {
            let sealed = fs::read(&path).map_err(|e| io_error("reading", &path, &e))?;
            let plain = open(&key, &file_stem(&path), &sealed).ok_or_else(|| unreadable(&path))?;
            names.push(plain.name);
        }
        names.sort();
        Ok(names)
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

    /// The key, read once per process. With `create`, a vault that has no
    /// key and no entries gets one, made under the directory's lock.
    fn key(&self, create: bool) -> Result<Option<Key>, CliError> {
        let mut cached = self.key.lock().unwrap();
        if let Some(key) = cached.as_ref() {
            return Ok(Some(key.clone()));
        }
        if let Some(key) = self.read_key()? {
            *cached = Some(key.clone());
            return Ok(Some(key));
        }
        if !create {
            return Ok(None);
        }
        let lock_path = self.dir.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| io_error("opening", &lock_path, &e))?;
        lock.lock()
            .map_err(|e| io_error("locking", &lock_path, &e))?;
        // Another process may have made the key while this one waited.
        if let Some(key) = self.read_key()? {
            *cached = Some(key.clone());
            return Ok(Some(key));
        }
        if !self.entry_files()?.is_empty() {
            return Ok(None);
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
        *cached = Some(fresh.clone());
        Ok(Some(fresh))
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

fn write_atomically(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix)
        .map_err(|e| CliError::Other(format!("drawing a file name from the system: {e}")))?;
    let temp = dir.join(format!(".{}.{}.tmp", file_stem(path), hex(&suffix)));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|e| io_error("creating", &temp, &e))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| io_error("writing", &temp, &e))?;
        fs::rename(&temp, path).map_err(|e| io_error("replacing", path, &e))?;
        #[cfg(unix)]
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_error("syncing", dir, &e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
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
    .with_remedy(REMEDY_CLEAR)
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
