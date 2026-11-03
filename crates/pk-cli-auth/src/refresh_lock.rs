//! Renewing a rotating refresh token from more than one process.
//!
//! A provider that rotates refresh tokens retires each one as it is spent, and
//! many answer a second use of a retired one by revoking the whole session. A
//! family CLI is often several processes over one keychain item: a long-running
//! `<bin> mcp --http`, a stdio server, a command in a terminal. An in-process
//! mutex cannot keep them from presenting the same token, and the recovery is a
//! sign-in the owner has to sit through.
//!
//! [`RefreshLock`] is an exclusive advisory lock on an empty file. It is held
//! from the read of the stored session to the write of the renewed one, so
//! every grant presents the newest token: [`RefreshLock::renew`] does that for a
//! renewal.
//!
//! A login or a logout holds it around its keychain write only, so a renewal in
//! flight cannot overwrite that write when it stores. Never hold the lock across
//! a prompt, a browser capture or a wait for a code: a person can take far longer
//! than a contender waits, and the write is all the race needs. Three calls cover
//! what a person is owed around that write:
//!
//! - [`RefreshLock::before_signin`], before the browser opens or a secret is read,
//!   so a stuck or unusable lock fails before anyone does any work.
//! - [`RefreshLock::store_signin`], for the write. Past that point the new session
//!   exists only in memory, so an error says it was not stored.
//! - [`RefreshLock::forget_for_binary`], for a logout, whose delete runs with or
//!   without the lock: the lock must never be what keeps a credential on a machine
//!   someone is trying to remove it from.
//!
//! The lock only excludes processes that take it. Until every copy sharing the
//! keychain item is a release that does (a server not yet restarted, an installed
//! Claude Desktop extension, which bundles its own binary), an old copy can still
//! present a token a new one is renewing.
//!
//! - **It cannot go stale.** The operating system releases the lock when its
//!   holder exits, however it exits, so nothing ever needs clearing.
//! - **The wait is bounded.** A holder can be stuck on a slow token endpoint or
//!   a keychain prompt nobody is there to answer, so a contender gives up after
//!   [`DEFAULT_WAIT`] with [`CliError::Upstream`] (exit 5, retry later) naming the
//!   lock, having sent nothing.
//! - **It is not reentrant.** Each [`hold`](RefreshLock::hold) opens the file
//!   afresh, so a nested one waits on its own holder until it times out. Keep
//!   reads that can renew outside the closure.
//!
//! ```no_run
//! # use pk_cli_core::CliError;
//! # use pk_cli_auth::refresh_lock::RefreshLock;
//! # struct Session { refresh_token: String }
//! # fn read_keychain() -> Result<Session, CliError> { todo!() }
//! # fn post_grant(_: &Session) -> Result<Session, CliError> { todo!() }
//! # fn write_keychain(_: &Session) -> Result<(), CliError> { todo!() }
//! # let presented = String::new();
//! let session = RefreshLock::for_binary("svc")?.renew(
//!     read_keychain,
//!     // What the provider just refused was issued against `presented`. If the
//!     // stored token moved on, another process renewed while this one waited.
//!     |latest| latest.refresh_token == presented,
//!     |latest| {
//!         let fresh = post_grant(&latest)?;
//!         write_keychain(&fresh)?;
//!         Ok(fresh)
//!     },
//! )?;
//! # Ok::<(), CliError>(())
//! ```

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pk_cli_core::CliError;

/// How long a contender waits by default: past the 60 s timeout on
/// `pk_cli_http`'s clients, so it outlasts a holder whose grant takes all of it.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(75);

/// How many times as long as a renewal [`RefreshLock::store_signin`] waits.
pub const SIGNIN_PATIENCE: u32 = 4;

const POLL_EVERY: Duration = Duration::from_millis(100);

/// The lock every process of one CLI contends on before it renews or replaces
/// the session it shares with the others. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct RefreshLock {
    path: PathBuf,
    wait: Duration,
}

impl RefreshLock {
    /// `~/.config/<bin>/token-refresh.lock`.
    ///
    /// Anchored to the home directory alone, deliberately not `--config`'s
    /// directory or `XDG_CONFIG_HOME`: the keychain item is the same whatever
    /// those say, and a launchd agent does not inherit the shell's environment,
    /// so a lock that followed them would leave the server and a terminal
    /// contending on two different files. The file holds nothing.
    pub fn for_binary(bin: &str) -> Result<Self, CliError> {
        let home = home_from(|name| std::env::var_os(name)).ok_or_else(|| {
            CliError::Other(
                "cannot locate the home directory for the token-refresh lock ($HOME unset)".into(),
            )
        })?;
        Ok(Self::at(path_under(Path::new(&home), bin)))
    }

    /// A lock at an explicit path, for a test or a CLI that keeps its state
    /// elsewhere. Every process sharing a session must name the same path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            wait: DEFAULT_WAIT,
        }
    }

    /// Wait at most `wait` for the lock instead of [`DEFAULT_WAIT`].
    pub fn wait_at_most(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` holding the lock, and release it when `f` returns. Keep `f` to
    /// the keychain write it protects; see the [module docs](self).
    ///
    /// Without the lock `f` does not run. A holder that outlasts the wait is
    /// transient, so it is [`CliError::Upstream`] (exit 5); a file that cannot be
    /// created or locked is a local fault a retry will not fix, so it is
    /// [`CliError::Other`] (exit 1). Both name the path. Proceeding unlocked is
    /// what would risk the session.
    pub fn hold<T>(&self, f: impl FnOnce() -> Result<T, CliError>) -> Result<T, CliError> {
        let _held = self.acquire().map_err(Refused::into_hold_error)?;
        f()
    }

    /// [`hold`](Self::hold) without waiting: `Ok(None)` when another process holds
    /// the lock right now, without running `f`. For a caller that does something
    /// different when it is not first. A file that cannot be created or locked is
    /// the same error as `hold`.
    pub fn try_hold<T>(
        &self,
        f: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<Option<T>, CliError> {
        match self.acquire_within(Duration::ZERO, || {}) {
            Ok(_held) => f().map(Some),
            Err(Refused::Held(_)) => Ok(None),
            Err(broken) => Err(broken.into_hold_error()),
        }
    }

    /// Check the lock before a sign-in, or before reading a secret a person
    /// supplies, while failing costs nothing: a stuck or unusable lock is the same
    /// error as [`hold`](Self::hold). This reserves nothing, because the lock is
    /// released at once and [`store_signin`](Self::store_signin) takes it again.
    pub fn before_signin(&self) -> Result<(), CliError> {
        self.hold(|| Ok(()))
    }

    /// A sign-in's keychain write, under the lock. It waits [`SIGNIN_PATIENCE`]
    /// times as long as a renewal, with a line on stderr once it has to, because
    /// the alternative is another sign-in.
    ///
    /// Without the lock `store` does not run, and the error says that the sign-in
    /// worked and its session is lost, because that session exists only in this
    /// process's memory and nothing else may be done with it. Store anything that
    /// needs no lock first. An error from `store` passes through unchanged.
    pub fn store_signin<T>(
        &self,
        store: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        const LOST: &str = "the sign-in worked, but its session was not stored and cannot be kept";
        let wait = self.wait * SIGNIN_PATIENCE;
        let _held = self
            .acquire_within(wait, || {
                eprintln!(
                    "waiting up to {wait:?} for another process to release {} so this sign-in \
                     can be stored",
                    self.path.display()
                );
            })
            .map_err(|refused| match refused {
                Refused::Held(why) => CliError::Upstream(format!(
                    "{LOST}: {why}. Sign in again once that process finishes"
                )),
                Refused::Broken(why) => CliError::Other(format!("{LOST}: {why}")),
            })?;
        store()
    }

    /// A logout's keychain delete, for the lock [`for_binary`](Self::for_binary)
    /// names. `delete` runs however the lock turns out, because the lock must not
    /// be what keeps a credential on the machine:
    ///
    /// - Lock free: `delete`'s result.
    /// - Another process holds it: `delete` runs at once, which removes what is
    ///   stored now, and again once the lock is free, to remove whatever a renewal in
    ///   flight stored. Only a logout that gets that far is settled: one interrupted
    ///   during the wait can have the session stored again behind it, as stderr says.
    ///   `delete` must tolerate finding nothing, and the result is the first call's,
    ///   what the logout found when it started. Past the wait the result is
    ///   [`CliError::Upstream`] (exit 5); running the logout again settles it.
    /// - Lock unusable: `delete`'s result, with a warning on stderr. A process
    ///   resolving the same path cannot be holding it either.
    /// - No home directory: `delete` runs, and the result is [`CliError::Upstream`],
    ///   because this process resolved no path and one with a home directory can hold
    ///   the real lock.
    ///
    /// An error from `delete` passes through unchanged: the session is still stored.
    pub fn forget_for_binary<T>(
        bin: &str,
        delete: impl Fn() -> Result<T, CliError>,
    ) -> Result<T, CliError> {
        match Self::for_binary(bin) {
            Ok(lock) => lock.forget(delete),
            Err(no_home) => forget_without_home(&no_home, delete),
        }
    }

    /// [`forget_for_binary`](Self::forget_for_binary) for this lock.
    pub fn forget<T>(&self, delete: impl Fn() -> Result<T, CliError>) -> Result<T, CliError> {
        match self.acquire_within(Duration::ZERO, || {}) {
            Ok(_held) => return delete(),
            Err(Refused::Broken(why)) => return forget_unlocked(&why, delete),
            Err(Refused::Held(_)) => {}
        }
        let found = delete()?;
        eprintln!(
            "the stored session is deleted, but a renewal in another process may store it \
             again: waiting up to {:?} for {} to delete it once more. If you interrupt this, run \
             the logout again",
            self.wait,
            self.path.display()
        );
        match self.acquire() {
            Ok(_held) => {
                delete()?;
                Ok(found)
            }
            Err(Refused::Held(why)) => Err(CliError::Upstream(format!(
                "the stored session is deleted, but {why}, and a renewal in flight can store it \
                 again: run this logout again in a minute"
            ))),
            Err(Refused::Broken(why)) => forget_unlocked(&why, delete).map(|_| found),
        }
    }

    /// One renewal under the lock: `read` the stored session, ask `due` whether
    /// it still needs renewing, and if so `grant`, which posts the grant and
    /// stores the result. Returns what `grant` returned, or what `read` found
    /// when another process had already renewed.
    ///
    /// `due` is the caller's rule: a CLI that renews after the provider refused
    /// a token compares the stored refresh token with the one behind the
    /// refused token, and one that renews on a clock asks whether the stored
    /// session has expired. `grant` must store before it returns; that is what
    /// the next process reads.
    pub fn renew<S>(
        &self,
        read: impl FnOnce() -> Result<S, CliError>,
        due: impl FnOnce(&S) -> bool,
        grant: impl FnOnce(S) -> Result<S, CliError>,
    ) -> Result<S, CliError> {
        self.hold(|| {
            let latest = read()?;
            if due(&latest) {
                grant(latest)
            } else {
                Ok(latest)
            }
        })
    }

    /// The open file holds the lock until it is closed.
    fn acquire(&self) -> Result<File, Refused> {
        self.acquire_within(self.wait, || {})
    }

    /// `contended` runs once, the first time another process holds the lock.
    fn acquire_within(&self, wait: Duration, contended: impl FnOnce()) -> Result<File, Refused> {
        let file = self.open().map_err(Refused::Broken)?;
        let started = Instant::now();
        let mut contended = Some(contended);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::Error(e)) => {
                    return Err(Refused::Broken(format!(
                        "locking {}: {e}",
                        self.path.display()
                    )))
                }
                Err(TryLockError::WouldBlock) => {
                    if let Some(contended) = contended.take() {
                        contended();
                    }
                    let waited = started.elapsed();
                    if waited >= wait {
                        return Err(Refused::Held(format!(
                            "another process has held {} for over {wait:?}, renewing or replacing \
                             this session (a keychain prompt waiting for an answer keeps it \
                             there)",
                            self.path.display(),
                        )));
                    }
                    std::thread::sleep(POLL_EVERY.min(wait - waited));
                }
            }
        }
    }

    fn open(&self) -> Result<File, String> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| {
                format!("creating {} for the token-refresh lock: {e}", dir.display())
            })?;
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&self.path)
            .map_err(|e| format!("opening {}: {e}", self.path.display()))
    }
}

/// Why the lock was not taken. Each caller owes a person a different sentence.
enum Refused {
    /// Another process held it past the wait, which a retry can get past.
    Held(String),
    /// The file cannot be created or locked, which a retry will not fix.
    Broken(String),
}

impl Refused {
    fn into_hold_error(self) -> CliError {
        match self {
            Self::Held(why) => {
                CliError::Upstream(format!("{why}; nothing was sent, so try again in a minute"))
            }
            Self::Broken(why) => CliError::Other(why),
        }
    }
}

fn forget_unlocked<T>(
    why: &str,
    delete: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    let deleted = delete()?;
    eprintln!("warning: deleted the stored session without the token-refresh lock: {why}");
    Ok(deleted)
}

fn forget_without_home<T>(
    no_home: &CliError,
    delete: impl FnOnce() -> Result<T, CliError>,
) -> Result<T, CliError> {
    delete()?;
    Err(CliError::Upstream(format!(
        "the stored session is deleted, but {no_home}, so a renewal in another process cannot be \
         ruled out: run this logout again from an environment with a home directory"
    )))
}

fn path_under(home: &Path, bin: &str) -> PathBuf {
    home.join(".config").join(bin).join("token-refresh.lock")
}

/// The first of `HOME` and `USERPROFILE` that is set and not empty.
fn home_from<V: AsRef<OsStr>>(var: impl Fn(&str) -> Option<V>) -> Option<PathBuf> {
    ["HOME", "USERPROFILE"].iter().find_map(|name| {
        var(name)
            .filter(|value| !value.as_ref().is_empty())
            .map(|value| PathBuf::from(value.as_ref()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    const CONTENDER_DIR: &str = "PK_REFRESH_LOCK_TEST_CONTENDER";
    const HOLDER_DIR: &str = "PK_REFRESH_LOCK_TEST_HOLDER";
    const CONTENDERS: usize = 2;
    const FIRST_TOKEN: &str = "dummy-refresh-1";

    fn lock_in(dir: &Path) -> RefreshLock {
        RefreshLock::at(dir.join("state").join("token-refresh.lock"))
    }

    /// This test binary again, running one test: the way to get a second real
    /// process, with its own open file description, without a separate binary.
    fn this_binary_running(test: &str, env: &str, dir: &Path) -> Command {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", test, "--test-threads=1"])
            .env(env, dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        child
    }

    /// The session both contenders share, in files: `session` is the stored
    /// refresh token, and `grants` gets a line per grant posted.
    fn contend(dir: &Path) {
        let session = dir.join("session");
        let read = || Ok(fs::read_to_string(&session).unwrap());
        lock_in(dir)
            .renew(
                read,
                |latest| latest == FIRST_TOKEN,
                |latest| {
                    let line = format!("{latest}\n");
                    let mut grants = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("grants"))
                        .unwrap();
                    std::io::Write::write_all(&mut grants, line.as_bytes()).unwrap();
                    // Long enough that the other contender arrives mid-grant.
                    std::thread::sleep(Duration::from_millis(300));
                    let fresh = "dummy-refresh-2".to_string();
                    fs::write(&session, &fresh).unwrap();
                    Ok(fresh)
                },
            )
            .unwrap();
    }

    /// Both start together: each announces itself, then waits for the other.
    fn wait_for_the_other_contender(dir: &Path) {
        fs::write(dir.join(format!("ready-{}", std::process::id())), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let ready = fs::read_dir(dir)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("ready-")
                })
                .count();
            if ready >= CONTENDERS {
                return;
            }
            assert!(Instant::now() < deadline, "the other contender never came");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Does nothing unless run by `two_processes_post_one_grant`.
    #[test]
    fn contender_process() {
        if let Some(dir) = std::env::var_os(CONTENDER_DIR) {
            let dir = PathBuf::from(dir);
            wait_for_the_other_contender(&dir);
            contend(&dir);
        }
    }

    #[test]
    fn two_processes_post_one_grant() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("session"), FIRST_TOKEN).unwrap();
        let mut children: Vec<_> = (0..CONTENDERS)
            .map(|_| {
                this_binary_running(
                    "refresh_lock::tests::contender_process",
                    CONTENDER_DIR,
                    dir.path(),
                )
                .spawn()
                .unwrap()
            })
            .collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("grants")).unwrap(),
            format!("{FIRST_TOKEN}\n"),
            "the second process must use the first's token, not present the old one"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("session")).unwrap(),
            "dummy-refresh-2"
        );
    }

    #[test]
    fn contending_threads_post_one_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-refresh.lock");
        let stored = Arc::new(Mutex::new(FIRST_TOKEN.to_string()));
        let grants = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (path, stored, grants, start) = (
                    path.clone(),
                    Arc::clone(&stored),
                    Arc::clone(&grants),
                    Arc::clone(&start),
                );
                std::thread::spawn(move || {
                    start.wait();
                    RefreshLock::at(path).renew(
                        || Ok(stored.lock().unwrap().clone()),
                        |latest| latest == FIRST_TOKEN,
                        |_| {
                            grants.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(50));
                            *stored.lock().unwrap() = "dummy-refresh-2".into();
                            Ok("dummy-refresh-2".to_string())
                        },
                    )
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap().unwrap(), "dummy-refresh-2");
        }
        assert_eq!(grants.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_session_that_moved_on_is_returned_without_a_grant() {
        let dir = tempfile::tempdir().unwrap();
        let session = lock_in(dir.path())
            .renew(
                || Ok("dummy-refresh-2".to_string()),
                |latest| latest == FIRST_TOKEN,
                |_| panic!("the stored token changed, so no grant may be posted"),
            )
            .unwrap();
        assert_eq!(session, "dummy-refresh-2");
    }

    #[test]
    fn a_session_still_due_is_granted() {
        let dir = tempfile::tempdir().unwrap();
        let session = lock_in(dir.path())
            .renew(
                || Ok(FIRST_TOKEN.to_string()),
                |latest| latest == FIRST_TOKEN,
                |latest| Ok(format!("{latest}-renewed")),
            )
            .unwrap();
        assert_eq!(session, format!("{FIRST_TOKEN}-renewed"));
    }

    #[test]
    fn a_contended_lock_gives_up_with_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let lock = lock_in(dir.path());
        lock.hold(|| {
            let started = Instant::now();
            let err = lock_in(dir.path())
                .wait_at_most(Duration::from_millis(200))
                .hold(|| -> Result<(), CliError> { panic!("ran without the lock") })
                .unwrap_err();
            assert!(started.elapsed() >= Duration::from_millis(200));
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "it must not hang"
            );
            assert_eq!(err.exit_code(), 5, "a retry can succeed: {err}");
            let message = err.to_string();
            assert!(message.contains(lock.path().to_str().unwrap()), "{message}");
            assert!(message.contains("try again"), "{message}");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_lock_that_cannot_be_opened_fails_rather_than_running_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        fs::write(&not_a_dir, "").unwrap();
        let err = RefreshLock::at(not_a_dir.join("token-refresh.lock"))
            .hold(|| -> Result<(), CliError> { panic!("ran without the lock") })
            .unwrap_err();
        assert_eq!(err.exit_code(), 1, "a retry cannot fix this: {err}");
        assert!(err.to_string().contains("token-refresh lock"), "{err}");
    }

    /// Does nothing unless run by `a_dead_holder_releases_the_lock`.
    #[test]
    fn holder_process() {
        if let Some(dir) = std::env::var_os(HOLDER_DIR) {
            let dir = PathBuf::from(dir);
            lock_in(&dir)
                .hold(|| {
                    fs::write(dir.join("held"), "").unwrap();
                    std::thread::sleep(Duration::from_secs(120));
                    Ok(())
                })
                .unwrap();
        }
    }

    #[test]
    fn a_dead_holder_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut holder = this_binary_running(
            "refresh_lock::tests::holder_process",
            HOLDER_DIR,
            dir.path(),
        )
        .spawn()
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !dir.path().join("held").exists() {
            assert!(Instant::now() < deadline, "the holder never took the lock");
            std::thread::sleep(Duration::from_millis(5));
        }
        // It is really held until the holder dies.
        assert!(lock_in(dir.path())
            .wait_at_most(Duration::from_millis(50))
            .hold(|| Ok(()))
            .is_err());
        holder.kill().unwrap();
        holder.wait().unwrap();
        lock_in(dir.path())
            .wait_at_most(Duration::from_secs(10))
            .hold(|| Ok(()))
            .unwrap();
    }

    #[test]
    fn a_failed_grant_releases_the_lock_and_keeps_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = lock_in(dir.path())
            .renew(
                || Ok(FIRST_TOKEN.to_string()),
                |_| true,
                |_| Err(CliError::Auth("the provider refused it".into())),
            )
            .unwrap_err();
        assert_eq!(err, CliError::Auth("the provider refused it".into()));
        lock_in(dir.path())
            .wait_at_most(Duration::ZERO)
            .hold(|| Ok(()))
            .unwrap();
    }

    #[test]
    fn the_file_stays_empty_and_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let lock = lock_in(dir.path());
        lock.renew(|| Ok(FIRST_TOKEN.to_string()), |_| true, Ok)
            .unwrap();
        let metadata = fs::metadata(lock.path()).unwrap();
        assert_eq!(metadata.len(), 0, "the lock file must hold no secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }

    /// A lock held by this test, and a second handle on it that gives up fast.
    fn contended(dir: &tempfile::TempDir) -> (RefreshLock, RefreshLock) {
        let path = dir.path().join("token-refresh.lock");
        let holder = RefreshLock::at(&path);
        let contender = RefreshLock::at(&path).wait_at_most(Duration::from_millis(100));
        (holder, contender)
    }

    /// A path whose parent is a file, so the lock can never be opened.
    fn broken(dir: &tempfile::TempDir) -> RefreshLock {
        let file = dir.path().join("a-file");
        fs::write(&file, "").unwrap();
        RefreshLock::at(file.join("token-refresh.lock"))
    }

    #[test]
    fn try_hold_runs_when_free_and_steps_aside_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let (holder, contender) = contended(&dir);
        assert_eq!(contender.try_hold(|| Ok("ran")).unwrap(), Some("ran"));
        holder
            .hold(|| {
                let skipped = contender
                    .try_hold(|| -> Result<(), CliError> { panic!("another process holds it") })
                    .unwrap();
                assert_eq!(skipped, None);
                Ok(())
            })
            .unwrap();
        assert_eq!(broken(&dir).try_hold(|| Ok(())).unwrap_err().exit_code(), 1);
    }

    #[test]
    fn before_signin_fails_on_a_stuck_lock_and_reserves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (holder, contender) = contended(&dir);
        holder
            .hold(|| {
                let err = contender.before_signin().unwrap_err();
                assert_eq!(err.exit_code(), 5, "{err}");
                Ok(())
            })
            .unwrap();
        contender.before_signin().unwrap();
        contender.hold(|| Ok(())).unwrap();
        assert_eq!(broken(&dir).before_signin().unwrap_err().exit_code(), 1);
    }

    #[test]
    fn a_signin_stores_under_the_lock_and_keeps_stores_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let (_, lock) = contended(&dir);
        assert_eq!(lock.store_signin(|| Ok("stored")).unwrap(), "stored");
        let err = lock
            .store_signin(|| Err::<(), _>(CliError::Keychain("dummy-refused".into())))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            CliError::Keychain("dummy-refused".into()).to_string()
        );
    }

    #[test]
    fn a_signin_without_the_lock_says_it_was_not_stored_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (holder, contender) = contended(&dir);
        holder
            .hold(|| {
                let err = contender
                    .store_signin(|| -> Result<(), CliError> {
                        panic!("no write without the lock")
                    })
                    .unwrap_err();
                assert_eq!(err.exit_code(), 5, "{err}");
                let message = err.to_string();
                assert!(message.contains("the sign-in worked"), "{message}");
                assert!(message.contains("not stored"), "{message}");
                assert!(message.contains("Sign in again"), "{message}");
                assert!(!message.contains("nothing was sent"), "{message}");
                Ok(())
            })
            .unwrap();
        let err = broken(&dir)
            .store_signin(|| -> Result<(), CliError> { panic!("no write without the lock") })
            .unwrap_err();
        assert_eq!(err.exit_code(), 1, "{err}");
        assert!(err.to_string().contains("not stored"), "{err}");
    }

    #[test]
    fn a_signin_waits_longer_than_a_renewal_for_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (holder, contender) = contended(&dir);
        holder
            .hold(|| {
                let started = Instant::now();
                contender
                    .store_signin(|| -> Result<(), CliError> {
                        panic!("no write without the lock")
                    })
                    .unwrap_err();
                assert!(started.elapsed() >= Duration::from_millis(100) * SIGNIN_PATIENCE);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_logout_with_the_lock_returns_the_deletes_result() {
        let dir = tempfile::tempdir().unwrap();
        let (_, lock) = contended(&dir);
        assert!(lock.forget(|| Ok(true)).unwrap());
    }

    #[test]
    fn a_logout_past_a_held_lock_deletes_and_says_to_run_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let (holder, contender) = contended(&dir);
        holder
            .hold(|| {
                let deleted = std::cell::Cell::new(false);
                let err = contender
                    .forget(|| {
                        deleted.set(true);
                        Ok(())
                    })
                    .unwrap_err();
                assert!(deleted.get(), "the lock must not keep the credential");
                assert_eq!(err.exit_code(), 5, "{err}");
                let message = err.to_string();
                assert!(message.contains("is deleted"), "{message}");
                assert!(message.contains("run this logout again"), "{message}");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_logout_deletes_before_it_waits_and_again_once_the_lock_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token-refresh.lock");
        let holder = RefreshLock::at(&path);
        let deletes = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let (locked, go) = std::sync::mpsc::channel();
            let (seen, release) = std::sync::mpsc::channel();
            scope.spawn(move || {
                holder
                    .hold(|| {
                        locked.send(()).unwrap();
                        release.recv().unwrap();
                        Ok(())
                    })
                    .unwrap();
            });
            go.recv().unwrap();
            RefreshLock::at(&path)
                .wait_at_most(Duration::from_secs(30))
                .forget(|| {
                    let this_one = deletes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if this_one == 0 {
                        // The first delete happens with the lock still held.
                        seen.send(()).unwrap();
                    }
                    Ok(this_one == 0)
                })
                .map(|found| assert!(found, "the first delete's result, not the second's"))
                .unwrap();
        });
        assert_eq!(deletes.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_logout_past_a_broken_lock_deletes_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        assert!(broken(&dir).forget(|| Ok(true)).unwrap());
    }

    #[test]
    fn a_logout_with_no_home_deletes_and_does_not_read_as_settled() {
        let no_home = CliError::Other("dummy: no home directory".into());
        let mut deleted = false;
        let err = forget_without_home(&no_home, || {
            deleted = true;
            Ok(())
        })
        .unwrap_err();
        assert!(deleted, "the lock must not keep the credential");
        assert_eq!(err.exit_code(), 5, "{err}");
        assert!(err.to_string().contains("is deleted"), "{err}");
        assert!(
            err.to_string().contains("dummy: no home directory"),
            "{err}"
        );
    }

    #[test]
    fn a_logout_whose_delete_fails_reports_that_and_not_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (holder, contender) = contended(&dir);
        holder
            .hold(|| {
                let err = contender
                    .forget(|| Err::<(), _>(CliError::Keychain("dummy-refused".into())))
                    .unwrap_err();
                assert!(err.to_string().contains("dummy-refused"), "{err}");
                assert!(!err.to_string().contains("is deleted"), "{err}");
                Ok(())
            })
            .unwrap();
        let err = broken(&dir)
            .forget(|| Err::<(), _>(CliError::Keychain("dummy-refused".into())))
            .unwrap_err();
        assert!(err.to_string().contains("dummy-refused"), "{err}");
    }

    #[test]
    fn the_path_hangs_off_the_home_directory_and_nothing_else() {
        assert_eq!(
            path_under(Path::new("/home/dummy"), "svc"),
            Path::new("/home/dummy/.config/svc/token-refresh.lock")
        );
        let home = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            home_from(move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
        };
        assert_eq!(home(&[("HOME", "/h")]), Some(PathBuf::from("/h")));
        assert_eq!(
            home(&[("HOME", ""), ("USERPROFILE", "C:\\h")]),
            Some(PathBuf::from("C:\\h"))
        );
        assert_eq!(home(&[("XDG_CONFIG_HOME", "/x")]), None);
    }
}
