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
//! renewal, and [`RefreshLock::hold`] wraps anything else that replaces or
//! removes the session (a login, a logout), which a renewal in flight would
//! otherwise overwrite when it stores.
//!
//! - **It cannot go stale.** The operating system releases the lock when its
//!   holder exits, however it exits, so nothing ever needs clearing.
//! - **The wait is bounded.** A holder can be stuck on a slow token endpoint or
//!   a keychain prompt nobody is there to answer, so a contender gives up after
//!   [`DEFAULT_WAIT`] with an error naming the lock, having sent nothing.
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

    /// Run `f` holding the lock, and release it when `f` returns.
    ///
    /// Without the lock `f` does not run: a holder that outlasts the wait, or a
    /// file that cannot be created or locked, is [`CliError::Other`] (exit 1)
    /// naming the path. Proceeding unlocked is what would risk the session.
    pub fn hold<T>(&self, f: impl FnOnce() -> Result<T, CliError>) -> Result<T, CliError> {
        let _held = self.acquire()?;
        f()
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
    fn acquire(&self) -> Result<File, CliError> {
        let file = self.open()?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::Error(e)) => {
                    return Err(CliError::Other(format!(
                        "locking {} before renewing the session: {e}",
                        self.path.display()
                    )))
                }
                Err(TryLockError::WouldBlock) => {
                    let waited = started.elapsed();
                    if waited >= self.wait {
                        return Err(CliError::Other(format!(
                            "another process has been renewing this session for over {:?} (it \
                             holds {}; a keychain prompt waiting for an answer keeps it there); \
                             nothing was sent, so try again in a minute",
                            self.wait,
                            self.path.display()
                        )));
                    }
                    std::thread::sleep(POLL_EVERY.min(self.wait - waited));
                }
            }
        }
    }

    fn open(&self) -> Result<File, CliError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| {
                CliError::Other(format!(
                    "creating {} for the token-refresh lock: {e}",
                    dir.display()
                ))
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
            .map_err(|e| CliError::Other(format!("opening {}: {e}", self.path.display())))
    }
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
            assert_eq!(err.exit_code(), 1);
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
