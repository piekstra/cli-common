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
//! - **The key** is read from the keychain once per `Vault` value and kept,
//!   and only when an entry exists or is written: a read with nothing
//!   stored costs no keychain read, so a logged-out run raises no dialog and
//!   a test pointed at an empty directory reads nothing. It is created by the
//!   first write, under a lock in the directory so two processes cannot
//!   create two keys, and never while entries exist: those would be lost to a new key, so a missing key
//!   with entries present is an error naming the remedy. A random key id
//!   beside the entries changes whenever a key is made or the vault cleared:
//!   a write holds the key lock and checks it, so a long-running process
//!   whose cached key another process cleared makes a new key rather than
//!   seal an entry nothing can read, and a read that fails to decrypt
//!   reloads the key once when the id moved.
//!
//! - **One vault per process.** A process that opens its secrets more than
//!   once (a server, a run that builds several clients) shares one
//!   `Arc<Vault>`, so it reads the key once. [`Vault::load_key`] makes that
//!   first read with a bound: on a thread of its own, holding none of the
//!   vault's locks. A keychain dialog nobody answers is then
//!   [`KeyLoad::Waiting`], and the vault's reads and writes that need the
//!   key fail at once (exit 3) until it is answered, rather than block.
//!   [`Vault::bound_key_reads`] holds every later keychain read of the key
//!   to a bound the same way, so a key another process replaced while a
//!   server ran is a timed-out read, not a hang.
//!   [`Vault::load_key_quietly`] raises no dialog at all and says when one
//!   is needed, for a run nobody is watching. [`Vault::check_key`] is `auth
//!   key`, the command that raises it for someone at the screen.
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
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use pk_cli_core::private_file::{self, Replace};
use pk_cli_core::CliError;
use pk_cli_secrets::{AskedRead, CredentialStore, QuietRead, Secret};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::key::{self, KeyArgs, KeyCheck};

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
///
/// A CLI's own store around a [`CredentialStore`] forwards the provided
/// methods too: their defaults cannot tell whether the keychain asks, so
/// `auth key` and [`Vault::load_key_quietly`] would not either.
pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Secret>, CliError>;
    fn set(&self, account: &str, value: &Secret) -> Result<(), CliError>;
    /// `true` when there was an item to delete.
    fn delete(&self, account: &str) -> Result<bool, CliError>;

    /// [`get`](Self::get), never asking the person anything
    /// ([`CredentialStore::get_quiet`]).
    fn get_quiet(&self, _account: &str) -> Result<QuietRead, CliError> {
        Ok(QuietRead::Unsupported)
    }

    /// [`get`](Self::get), saying when it was refused
    /// ([`CredentialStore::get_asking`]).
    fn get_asking(&self, account: &str) -> Result<AskedRead, CliError> {
        Ok(match self.get(account)? {
            Some(item) => AskedRead::Found(item),
            None => AskedRead::Absent,
        })
    }

    /// The keychain service the items are under, to name in a report.
    fn keychain_service(&self) -> Option<&str> {
        None
    }
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

    fn get_quiet(&self, account: &str) -> Result<QuietRead, CliError> {
        CredentialStore::get_quiet(self, account)
    }

    fn get_asking(&self, account: &str) -> Result<AskedRead, CliError> {
        CredentialStore::get_asking(self, account)
    }

    fn keychain_service(&self) -> Option<&str> {
        Some(self.service())
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

    fn get_quiet(&self, account: &str) -> Result<QuietRead, CliError> {
        Ok(match self.get(account)? {
            Some(item) => QuietRead::Found(item),
            None => QuietRead::Absent,
        })
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

    fn get_quiet(&self, account: &str) -> Result<QuietRead, CliError> {
        (**self).get_quiet(account)
    }

    fn get_asking(&self, account: &str) -> Result<AskedRead, CliError> {
        (**self).get_asking(account)
    }

    fn keychain_service(&self) -> Option<&str> {
        (**self).keychain_service()
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

/// A bounded key read's answer: the key, none, or an error.
type KeyRead = Result<Option<Key>, CliError>;

/// What [`Vault::load_key`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyLoad {
    /// The key is in this vault's cache: its reads and writes ask the
    /// keychain nothing more.
    Loaded,
    /// No entries: nothing needs the key, and none was read.
    Empty,
    /// Entries, but the keychain holds no key; a read reports the lost key.
    Missing,
    /// The keychain did not answer within the wait (on macOS, a dialog
    /// nobody has answered). The read goes on in the background. Until it
    /// answers, this vault's reads and writes that need the key fail at once
    /// (exit 3) instead of asking again or blocking, and a later
    /// [`Vault::load_key`] waits on the same read.
    Waiting,
}

impl KeyLoad {
    /// What a CLI that stops on this answer fails with, so that every CLI
    /// says the same and exits the same. `None` where the vault can be used,
    /// [`Empty`](Self::Empty) among them: what an empty vault means is the
    /// command's own answer.
    pub fn failure(self) -> Option<CliError> {
        match self {
            Self::Loaded | Self::Empty => None,
            Self::Missing => Some(lost("the vault")),
            Self::Waiting => Some(waiting_for_keychain()),
        }
    }
}

/// What [`Vault::load_key_quietly`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum QuietLoad {
    /// The key is in this vault's cache: its reads and writes ask the
    /// keychain nothing more.
    Loaded,
    /// No entries and no key: nothing needs it, and the first write makes
    /// one without asking.
    Empty,
    /// Entries, but the keychain holds no key; a read reports the lost key.
    Missing,
    /// A person is needed, and no dialog was raised. Either reading the key
    /// asks, with entries or without (`auth key` run at the screen raises
    /// that dialog), or there is no key and the keychain is locked, so the
    /// write that makes one would ask to unlock it.
    NeedsPerson,
    /// This keychain cannot be read without possibly asking; nothing was
    /// read. [`Vault::load_key`] bounds a read that may.
    Unsupported,
}

impl QuietLoad {
    /// What a CLI that stops on this answer fails with, so that every CLI
    /// says the same and exits the same: exit 3 (reason
    /// `credential_unreadable`) for [`NeedsPerson`](Self::NeedsPerson), 1
    /// for [`Missing`](Self::Missing) and
    /// [`Unsupported`](Self::Unsupported). `None` where the vault can be
    /// used, [`Empty`](Self::Empty) among them: what an empty vault means is
    /// the command's own answer.
    ///
    /// ```no_run
    /// # fn run(vault: &pk_cli_auth::vault::Vault) -> Result<(), pk_cli_core::CliError> {
    /// if let Some(failure) = vault.load_key_quietly()?.failure() {
    ///     return Err(failure);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// A later answer gets its words here, so a CLI that calls this needs no
    /// arm for it.
    pub fn failure(self) -> Option<CliError> {
        match self {
            Self::Loaded | Self::Empty => None,
            Self::Missing => Some(lost("the vault")),
            Self::NeedsPerson => Some(key::unread(
                "the vault's key cannot be used until someone answers the keychain at the screen"
                    .into(),
                "unlock the login keychain if it is locked, then run `auth key` at the screen \
                 and choose Always Allow",
            )),
            Self::Unsupported => Some(
                CliError::Keychain(
                    "this keychain cannot be read without possibly asking someone; nothing was \
                     read"
                        .into(),
                )
                .with_remedy(
                    "run this where someone can answer the keychain; on macOS or Windows, update \
                     this CLI",
                ),
            ),
        }
    }
}

/// A bounded key read left to answer in the background, with the vault's
/// key id from before it started. Its key is cached under that id, so a
/// vault cleared and rekeyed while it waited reads the key again rather
/// than seal with the one it got.
struct Pending {
    id: Option<String>,
    rx: mpsc::Receiver<KeyRead>,
}

/// Whether a bounded key read is still waiting on the keychain.
enum Settled {
    Idle,
    Waiting,
}

/// Secrets in owner-only encrypted files, under one keychain key. See the
/// [module docs](self).
pub struct Vault {
    keys: Arc<dyn KeyStore>,
    dir: PathBuf,
    key: Mutex<Option<Cached>>,
    /// A bounded key read still on its way. Locked after `key`.
    loading: Mutex<Option<Pending>>,
    wait: Duration,
    /// [`Vault::bound_key_reads`]: how long a keychain read of the key may
    /// take before it is left to answer in the background.
    read_bound: Option<Duration>,
}

/// The keychain store `bin`'s vault keeps its key in: the service
/// `piekstra.<bin>`, or the one `<PREFIX>_KEYCHAIN_SERVICE` names (a test's
/// empty service), `<PREFIX>` being `bin` uppercased with `-` as `_`.
pub fn keychain_for(bin: &str) -> CredentialStore {
    let prefix: String = bin
        .chars()
        .map(|c| match c {
            '-' | '.' | ' ' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect();
    match std::env::var(format!("{prefix}_KEYCHAIN_SERVICE")) {
        Ok(s) if !s.is_empty() => CredentialStore::new(s),
        _ => CredentialStore::for_binary(bin),
    }
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vault")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Vault {
    /// The CLI's vault: its key in [`keychain_for`]`(bin)` at
    /// [`KEY_ACCOUNT`], its entries in `~/.config/<bin>/vault/`.
    pub fn for_binary(bin: &str) -> Result<Self, CliError> {
        Ok(Self::new(keychain_for(bin), default_dir(bin)?))
    }

    /// A vault with its key in `keys` and its entries in `dir`: a CLI's
    /// keychain-service override, or a test's [`MemoryKeys`].
    pub fn new(keys: impl KeyStore + 'static, dir: impl Into<PathBuf>) -> Self {
        Self {
            keys: Arc::new(keys),
            dir: dir.into(),
            key: Mutex::new(None),
            loading: Mutex::new(None),
            wait: KEY_WAIT,
            read_bound: None,
        }
    }

    /// Hold every keychain read of the key to `wait`, as [`Vault::load_key`]
    /// holds the first: the first read for an entry, the read again after
    /// another process replaced the key, and a write's. Each runs on a
    /// thread of its own. One that does not answer in time fails with exit
    /// 3, naming the dialog, and is left to answer in the background; until
    /// it does, the vault's reads and writes that need the key fail at once
    /// instead of blocking or asking again. For a process nobody may be
    /// watching: a server, a scheduled run. Without it a read waits for the
    /// keychain's answer, as a command run at the screen should.
    pub fn bound_key_reads(mut self, wait: Duration) -> Self {
        self.read_bound = Some(wait);
        self
    }

    /// Read the key into this vault's cache now, waiting at most `wait` for
    /// the keychain. The read runs on a thread of its own and holds no lock
    /// of the vault's, so a dialog nobody answers is [`KeyLoad::Waiting`],
    /// never a hang, and never a hang for another thread using this vault.
    /// Call it on the one `Arc<Vault>` a process shares, before the first
    /// read: on macOS a person's "Allow" covers that one read, and the
    /// cached key then serves every later one. Reads nothing when the vault
    /// has no entries.
    pub fn load_key(&self, wait: Duration) -> Result<KeyLoad, CliError> {
        let deadline = Instant::now() + wait;
        {
            let mut cached = self.key.lock().unwrap();
            if let Settled::Idle = self.settle(&mut cached)? {
                if cached.is_some() {
                    return Ok(KeyLoad::Loaded);
                }
                if self.entry_files()?.is_empty() {
                    return Ok(KeyLoad::Empty);
                }
                let id = self.current_id()?;
                *self.loading.lock().unwrap() = Some(Pending {
                    id,
                    rx: self.spawn_read(),
                });
            }
        }
        loop {
            {
                let mut cached = self.key.lock().unwrap();
                if let Settled::Idle = self.settle(&mut cached)? {
                    return Ok(if cached.is_some() {
                        KeyLoad::Loaded
                    } else {
                        KeyLoad::Missing
                    });
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(KeyLoad::Waiting);
            }
            std::thread::sleep(left.min(Duration::from_millis(20)));
        }
    }

    /// Read the key into this vault's cache without raising a dialog, and
    /// say when that needs a person ([`CredentialStore::get_quiet`], with its
    /// note on other threads). For a scheduled or background run, which then
    /// reports at once instead of raising a dialog nobody is there to
    /// answer. Unlike [`load_key`](Self::load_key) it reads an empty vault's
    /// key too, so a write after [`QuietLoad::Loaded`] or
    /// [`QuietLoad::Empty`] raises no dialog either.
    pub fn load_key_quietly(&self) -> Result<QuietLoad, CliError> {
        let mut cached = self.key.lock().unwrap();
        if let Settled::Waiting = self.settle(&mut cached)? {
            return Ok(QuietLoad::NeedsPerson);
        }
        if cached.is_some() {
            return Ok(QuietLoad::Loaded);
        }
        let id = self.current_id()?;
        Ok(match self.keys.get_quiet(KEY_ACCOUNT)? {
            QuietRead::Found(item) => {
                *cached = Some(Cached {
                    key: parse_key(&item)?,
                    id,
                });
                QuietLoad::Loaded
            }
            QuietRead::Absent if self.entry_files()?.is_empty() => QuietLoad::Empty,
            QuietRead::AbsentLocked if self.entry_files()?.is_empty() => QuietLoad::NeedsPerson,
            QuietRead::Absent | QuietRead::AbsentLocked => QuietLoad::Missing,
            QuietRead::NeedsPerson => QuietLoad::NeedsPerson,
            // `Unsupported`, as any answer not known here is taken to be.
            _ => QuietLoad::Unsupported,
        })
    }

    /// `auth key`: read the vault's key from the keychain once, past this
    /// vault's cache and leaving it as it was, and say what the keychain did
    /// ([`key`](crate::key)). Whether or not there are entries: the point is
    /// the keychain's answer. A key that is missing while there are entries,
    /// or is not a key, is the report's failure (exit 1), and with neither
    /// key nor entries there is none.
    pub fn check_key(&self, args: &KeyArgs) -> Result<KeyCheck, CliError> {
        key::check_item(&self.keys, KEY_ACCOUNT, args, |found| match found {
            Some(item) => parse_key(item).map(drop),
            None if self.entry_files()?.is_empty() => Ok(()),
            None => Err(key_lost(&self.dir)),
        })
    }

    /// A keychain read of the key on a thread of its own, holding none of
    /// the vault's locks.
    fn spawn_read(&self) -> mpsc::Receiver<KeyRead> {
        let (tx, rx) = mpsc::channel();
        let keys = Arc::clone(&self.keys);
        std::thread::spawn(move || {
            let _ = tx.send(read_key_from(&*keys));
        });
        rx
    }

    /// The keychain's key, for a caller holding the key lock: read on the
    /// caller's thread, or under [`Vault::bound_key_reads`] on a thread of
    /// its own, waiting at most the bound and then left to `loading` with
    /// `id`, the vault's key id read before this call.
    fn fetch_key(&self, id: &Option<String>) -> KeyRead {
        let Some(bound) = self.read_bound else {
            return self.read_key();
        };
        let rx = self.spawn_read();
        match rx.recv_timeout(bound) {
            Ok(read) => read,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                *self.loading.lock().unwrap() = Some(Pending { id: id.clone(), rx });
                Err(waiting_for_keychain())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CliError::Keychain(
                "the keychain read of the vault's key ended without an answer".into(),
            )),
        }
    }

    /// Take a bounded key read that has answered into the cache, under the
    /// key id from before it started. `Waiting` while it has not.
    fn settle(&self, cached: &mut Option<Cached>) -> Result<Settled, CliError> {
        let mut loading = self.loading.lock().unwrap();
        let Some(pending) = loading.as_ref() else {
            return Ok(Settled::Idle);
        };
        match pending.rx.try_recv() {
            Ok(read) => {
                let id = loading.take().and_then(|p| p.id);
                if let Some(key) = read? {
                    *cached = Some(Cached { key, id });
                }
                Ok(Settled::Idle)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(Settled::Waiting),
            Err(mpsc::TryRecvError::Disconnected) => {
                *loading = None;
                Ok(Settled::Idle)
            }
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

    /// Whether an entry `name` is stored. It looks for the entry's file and
    /// never reads the keychain, so a command that only reports whether a
    /// secret is there (`info`, a status line) raises no prompt for a newly
    /// built binary. It does not decrypt: an entry sealed with a lost or
    /// replaced key is still there, and [`get`](Self::get) reports it.
    pub fn contains(&self, name: &str) -> Result<bool, CliError> {
        let path = self.entry_path(name);
        match fs::metadata(&path) {
            Ok(meta) => Ok(meta.is_file()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error("reading", &path, &e)),
        }
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
        // A read still waiting would answer with the deleted key.
        *self.loading.lock().unwrap() = None;
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
        if let Settled::Waiting = self.settle(&mut cached)? {
            return Err(waiting_for_keychain());
        }
        if let Some(c) = cached.as_ref() {
            return Ok(Some(c.key.clone()));
        }
        let id = self.current_id()?;
        let Some(key) = self.fetch_key(&id)? else {
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
        if let Settled::Waiting = self.settle(&mut cached)? {
            return Err(waiting_for_keychain());
        }
        let id = self.current_id()?;
        if cached.as_ref().is_some_and(|c| c.id == id) {
            return Ok(None);
        }
        *cached = None;
        let Some(key) = self.fetch_key(&id)? else {
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
        if let Settled::Waiting = self.settle(&mut cached)? {
            return Err(waiting_for_keychain());
        }
        let id = self.current_id()?;
        if let Some(c) = cached.as_ref().filter(|c| c.id == id) {
            return Ok(c.key.clone());
        }
        *cached = None;
        if let Some(key) = self.fetch_key(&id)? {
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
        // The new id first: a process holding an older key reads the
        // keychain again even if this one stops at the read-back.
        let id = Some(self.write_id()?);
        // Read it back: a store that dropped the write would otherwise seal
        // entries with a key nothing can find again.
        match self.fetch_key(&id)? {
            Some(stored) if *stored == *fresh => {}
            _ => {
                return Err(CliError::Keychain(
                    "the vault's new key did not read back from the keychain".into(),
                ))
            }
        }
        *cached = Some(Cached {
            key: fresh.clone(),
            id,
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

    fn read_key(&self) -> KeyRead {
        read_key_from(&*self.keys)
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

/// The vault's key as `keys` holds it, or `None` when it holds none.
fn read_key_from(keys: &dyn KeyStore) -> KeyRead {
    keys.get(KEY_ACCOUNT)?
        .map(|item| parse_key(&item))
        .transpose()
}

/// The key a key item holds.
fn parse_key(item: &Secret) -> Result<Key, CliError> {
    let item: KeyItem = serde_json::from_str(item.expose()).map_err(|_| bad_key())?;
    if item.format != KEY_FORMAT {
        return Err(bad_key());
    }
    let mut key: Key = Zeroizing::new([0u8; 32]);
    unhex(&item.key, key.as_mut()).ok_or_else(bad_key)?;
    Ok(key)
}

fn waiting_for_keychain() -> CliError {
    key::unread(
        "the keychain has not yet answered whether this program may read the vault's key; \
         on macOS a dialog asking for it is open"
            .into(),
        "answer the keychain dialog, choosing Always Allow so it does not ask again, then try \
         again",
    )
}

fn key_lost(dir: &Path) -> CliError {
    lost(&format!("the vault at {}", dir.display()))
}

fn lost(vault: &str) -> CliError {
    CliError::Keychain(format!(
        "{vault} has entries but its key ({KEY_ACCOUNT}) is not in the keychain"
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
