//! The Chrome profile a sign-in runs in: the CLI's kept profile, which
//! survives between sign-ins so the provider remembers the device, or a
//! scratch one removed when the window closes.
//!
//! The kept profile lives at `<data dir>/piekstra.<bin>/browser-profile`
//! (`~/Library/Application Support` on macOS, `$XDG_DATA_HOME` or
//! `~/.local/share` elsewhere). It is created `0700` and used only while it
//! is a real directory, owned by the current user, with that mode. One
//! sign-in uses it at a time: the run holds an advisory lock on a file in
//! it, and a Chrome still running in it (Chrome's own `SingletonLock` naming
//! a live pid) refuses the launch. Nothing outside the profile directory is
//! ever removed or rewritten.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::BrowserError;

/// The kept profile's directory name under `piekstra.<bin>`.
const KEPT_PROFILE: &str = "browser-profile";
/// The file a sign-in holds an advisory lock on while it uses the kept
/// profile.
#[cfg_attr(not(unix), allow(dead_code))]
const RUN_LOCK: &str = "pk-cli-browser.lock";
/// Chrome's lock: a symlink whose target is `<host>-<pid>`.
const SINGLETON_LOCK: &str = "SingletonLock";
/// What a Chrome that is no longer running leaves in its profile and the
/// next launch must not read: its singleton files (`SingletonSocket` and
/// `SingletonCookie` are symlinks into a temp dir; removing them removes
/// only the links) and the previous debug port.
const LEFTOVERS: [&str; 4] = [
    SINGLETON_LOCK,
    "SingletonSocket",
    "SingletonCookie",
    "DevToolsActivePort",
];

/// The profile Chrome runs in for one sign-in.
///
/// A scratch profile ([`ProfileDir::create`]) holds the live session only
/// while the window is open, and Chrome writes the debug port into it, so it
/// is created `0700`, exclusively (an existing path, planted or stale, is
/// refused rather than adopted), under an unpredictable name, checked to be
/// a real directory with that mode before Chrome is pointed at it, and
/// removed when dropped. A kept profile ([`ProfileDir::open_kept`]) stays
/// when dropped; the lock that keeps a second sign-in out of it is released.
#[derive(Debug)]
pub(crate) struct ProfileDir {
    path: PathBuf,
    kept: bool,
    // Held while a kept profile is in use; closing it releases the lock.
    _run_lock: Option<File>,
}

impl ProfileDir {
    pub fn create(prefix: &str) -> Result<Self, BrowserError> {
        Self::create_in(&std::env::temp_dir(), prefix)
    }

    pub fn create_in(parent: &Path, prefix: &str) -> Result<Self, BrowserError> {
        let path = parent.join(format!(
            "{}-browser-login-{}-{:016x}",
            fs_safe(prefix),
            std::process::id(),
            unpredictable()
        ));
        create_private_dir(&path)?;
        Ok(ProfileDir {
            path,
            kept: false,
            _run_lock: None,
        })
    }

    /// Open `bin`'s kept profile under `root` (the per-user data dir),
    /// creating it `0700` if it is missing, for one sign-in.
    ///
    /// Refused, as [`BrowserError::Profile`], when the directory is a
    /// symlink, is not a directory, is not owned by the current user or is
    /// not `0700`; when another sign-in holds it; or when Chrome's lock in
    /// it names a pid that is still running. A lock whose pid is gone is
    /// stale: it is removed, with the rest of what that Chrome left behind,
    /// and the sign-in goes ahead.
    pub fn open_kept(root: &Path, bin: &str, name: Option<&str>) -> Result<Self, BrowserError> {
        let path = kept_profile_path(root, bin, name);
        ensure_kept_dir(&path)?;
        let run_lock = lock_for_run(&path).map_err(|e| {
            BrowserError::Profile(format!(
                "locking the browser profile {}: {e}",
                path.display()
            ))
        })?;
        #[cfg(unix)]
        if run_lock.is_none() {
            return Err(BrowserError::Profile(format!(
                "{}; finish or cancel that one first, or pass --fresh-profile",
                held_message(bin, &path, None)
            )));
        }
        if let Some(pid) = chrome_holder(&path, process_alive)? {
            return Err(BrowserError::Profile(format!(
                "{}; close that window, then run this again (or pass --fresh-profile)",
                held_message(bin, &path, Some(pid))
            )));
        }
        remove_leftovers(&path)?;
        Ok(ProfileDir {
            path,
            kept: true,
            _run_lock: run_lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory to remove if the process has to exit at once (the
    /// second Ctrl-C): a scratch profile's, never a kept one's.
    pub fn scratch_path(&self) -> Option<PathBuf> {
        (!self.kept).then(|| self.path.clone())
    }
}

impl Drop for ProfileDir {
    /// A scratch profile left behind may hold the signed-in session, so a
    /// failure to remove it is reported even under `-q`: it is the one
    /// outcome the owner has to act on. A kept profile stays.
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        if let Err(e) = remove_dir_patiently(&self.path, 10, Duration::from_millis(100)) {
            eprintln!("{}", removal_warning(&self.path, &e));
        }
    }
}

/// Where the per-user data directory is on each platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOs,
    Windows,
    Xdg,
}

impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Xdg
        }
    }
}

/// The per-user data directory, from the environment `var` reads:
/// `$HOME/Library/Application Support` on macOS, `%LOCALAPPDATA%` on
/// Windows, else `$XDG_DATA_HOME` or `$HOME/.local/share`. A variable that
/// is unset, empty or not an absolute path counts as missing (the XDG rule),
/// so a session profile never lands relative to the working directory.
pub(crate) fn data_home_from(
    platform: Platform,
    var: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
    match platform {
        Platform::MacOs => absolute("HOME").map(|h| h.join("Library").join("Application Support")),
        Platform::Windows => absolute("LOCALAPPDATA"),
        Platform::Xdg => absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|h| h.join(".local").join("share"))),
    }
}

/// The real data directory the kept profiles live under. A test that
/// reached it would read or delete the owner's real profile, so in this
/// crate's tests it panics: every test passes a root of its own.
pub(crate) fn default_root() -> Result<PathBuf, BrowserError> {
    if cfg!(test) {
        panic!("a test reached the real data directory; give it a profile root or a fresh profile");
    }
    data_home_from(Platform::current(), |name| std::env::var_os(name)).ok_or_else(|| {
        BrowserError::Profile(
            "cannot locate the per-user data directory for the browser profile ($HOME, \
             $XDG_DATA_HOME or %LOCALAPPDATA% is unset or not an absolute path)"
                .into(),
        )
    })
}

/// Remove the browser profile `bin` keeps between sign-ins, signing the
/// browser out of every provider it was signed in to: the next
/// `auth login --browser` starts from an unknown device. A CLI calls it from
/// `auth logout`. `Ok(true)` when there was a profile to remove, `Ok(false)`
/// when there was none.
///
/// Only the profile directory is removed: never its parent, and never what
/// a link inside it points to. Refused, as [`BrowserError::Profile`], while
/// a sign-in or a running Chrome is using the profile (close the window,
/// then log out again), or when the path is a symlink, is not a directory or
/// belongs to another user. A CLI whose logout also clears other state does
/// that first, so a refusal here leaves the rest of the logout done.
pub fn forget_profile(bin: &str) -> Result<bool, BrowserError> {
    forget_in(&default_root()?, bin, None)
}

/// [`forget_profile`] for a profile kept under `name`
/// ([`Launch::profile_name`](crate::Launch::profile_name)): removes that
/// account's profile and leaves the CLI's others. A `name` a launch would
/// refuse is [`BrowserError::Invalid`].
pub fn forget_named_profile(bin: &str, name: &str) -> Result<bool, BrowserError> {
    if !profile_name_ok(name) {
        return Err(invalid_name(name));
    }
    forget_in(&default_root()?, bin, Some(name))
}

pub(crate) fn invalid_name(name: &str) -> BrowserError {
    BrowserError::Invalid(format!(
        "browser profile name `{name}` must be 1 to 64 ASCII letters, digits, `-` or `_`"
    ))
}

/// `<root>/piekstra.<bin>/browser-profile`, or `browser-profile-<name>` for
/// a named profile. `bin` is made filesystem-safe; `name` must already have
/// passed [`profile_name_ok`].
pub(crate) fn kept_profile_path(root: &Path, bin: &str, name: Option<&str>) -> PathBuf {
    let leaf = match name {
        None => KEPT_PROFILE.to_string(),
        Some(name) => format!("{KEPT_PROFILE}-{name}"),
    };
    root.join(format!("piekstra.{}", fs_safe(bin))).join(leaf)
}

/// A profile name is 1 to 64 ASCII letters, digits, `-` or `_`. It is
/// checked, never rewritten, so two names can never map onto one profile.
pub(crate) fn profile_name_ok(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Remove `bin`'s kept profile under `root` (the default one, or `name`'s).
/// `Ok(false)` when there was none. Refused when the path is a symlink, is
/// not a directory or is another user's, and while a sign-in or a running
/// Chrome is using the profile. Removes the profile directory only, never
/// its parent or anything a link in it points to.
pub(crate) fn forget_in(root: &Path, bin: &str, name: Option<&str>) -> Result<bool, BrowserError> {
    let path = kept_profile_path(root, bin, name);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(BrowserError::Profile(format!(
                "checking the browser profile {}: {e}",
                path.display()
            )))
        }
    };
    #[cfg(unix)]
    if let Some(problem) = DirFacts::of(&meta).problem(current_uid(), false) {
        return Err(BrowserError::Profile(format!(
            "the browser profile dir {} {problem}; refusing to remove it",
            path.display()
        )));
    }
    #[cfg(not(unix))]
    let _ = meta;
    let run_lock = lock_for_run(&path).map_err(|e| {
        BrowserError::Profile(format!(
            "checking whether the browser profile {} is in use: {e}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    if run_lock.is_none() {
        return Err(BrowserError::Profile(format!(
            "{}; finish or cancel it, then log out again",
            held_message(bin, &path, None)
        )));
    }
    if let Some(pid) = chrome_holder(&path, process_alive)? {
        return Err(BrowserError::Profile(format!(
            "{}; close that window, then log out again",
            held_message(bin, &path, Some(pid))
        )));
    }
    remove_dir_patiently(&path, 10, Duration::from_millis(100)).map_err(|e| {
        BrowserError::Profile(format!(
            "could not remove the browser profile {} ({e}); it may hold the signed-in session, \
             so delete it",
            path.display()
        ))
    })?;
    drop(run_lock);
    Ok(true)
}

fn held_message(bin: &str, path: &Path, chrome: Option<u32>) -> String {
    match chrome {
        None => format!(
            "another {bin} browser sign-in is using the browser profile {}",
            path.display()
        ),
        Some(pid) => format!(
            "a Chrome (pid {pid}) is still running in {bin}'s browser profile {}",
            path.display()
        ),
    }
}

/// Make sure the kept profile exists as a private directory: missing
/// parents and the profile itself are created `0700`, and what is there is
/// checked (not followed) before anything uses it.
fn ensure_kept_dir(path: &Path) -> Result<(), BrowserError> {
    let made = |e: std::io::Error, what: &Path| {
        BrowserError::Profile(format!("creating {}: {e}", what.display()))
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| made(e, parent))?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(made(e, path)),
        }
        let meta = std::fs::symlink_metadata(path).map_err(|e| {
            BrowserError::Profile(format!(
                "checking the browser profile dir {}: {e}",
                path.display()
            ))
        })?;
        if let Some(problem) = DirFacts::of(&meta).problem(current_uid(), true) {
            return Err(BrowserError::Profile(format!(
                "the browser profile dir {} {problem}; refusing to launch",
                path.display()
            )));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path).map_err(|e| made(e, path))
    }
}

/// What the kept profile's checks look at, from `symlink_metadata` so a
/// symlink is seen as one rather than followed.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct DirFacts {
    pub symlink: bool,
    pub dir: bool,
    pub owner: u32,
    pub mode: u32,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl DirFacts {
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        DirFacts {
            symlink: meta.file_type().is_symlink(),
            dir: meta.is_dir(),
            owner: meta.uid(),
            mode: meta.mode() & 0o777,
        }
    }

    /// Why this is not a profile directory the user `me` may use (with
    /// `private`, the `0700` mode is required too), or `None`.
    pub fn problem(&self, me: u32, private: bool) -> Option<String> {
        if self.symlink {
            Some("is a symlink".into())
        } else if !self.dir {
            Some("is not a directory".into())
        } else if self.owner != me {
            Some(format!(
                "is owned by uid {}, not by you (uid {me})",
                self.owner
            ))
        } else if private && self.mode != 0o700 {
            Some(format!(
                "has mode {:04o}, not 0700 (`chmod 700` it to use it)",
                self.mode
            ))
        } else {
            None
        }
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid(2) takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// Take the kept profile's run lock without waiting: `Some(file)` holds it
/// until the file is closed (the kernel releases it if the process dies, so
/// it is never stale), `None` means another sign-in holds it. The file is
/// opened without following a symlink and is not inherited by Chrome.
#[cfg(unix)]
fn lock_for_run(profile: &Path) -> std::io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(profile.join(RUN_LOCK))?;
    // SAFETY: flock(2) on a descriptor `file` owns for the whole call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(err)
    }
}

/// Elsewhere there is no run lock; Chrome's own singleton handling is what
/// keeps a second browser out.
#[cfg(not(unix))]
fn lock_for_run(_profile: &Path) -> std::io::Result<Option<File>> {
    Ok(None)
}

/// What Chrome's `SingletonLock` says about a profile.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChromeLock {
    /// No lock: no Chrome has the profile open.
    Free,
    /// A lock whose pid is not running (or that names none): left by a
    /// Chrome that was killed or crashed.
    Stale,
    /// A lock naming a running pid: a Chrome has the profile open.
    Held(u32),
}

/// Judge Chrome's lock from its symlink target (`None` when there is no
/// lock), `<host>-<pid>`, with `alive` saying whether a pid is running. The
/// host is not compared: the profile is in a per-user local directory, and
/// a Mac's host name changes with its network, which would make a live
/// Chrome's lock look foreign. A target with no pid is stale, as Chrome
/// itself treats it.
pub(crate) fn judge_chrome_lock(target: Option<&str>, alive: impl Fn(u32) -> bool) -> ChromeLock {
    let Some(target) = target else {
        return ChromeLock::Free;
    };
    let pid = target
        .rsplit_once('-')
        .and_then(|(_, pid)| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    match pid {
        Some(pid) if alive(pid) => ChromeLock::Held(pid),
        _ => ChromeLock::Stale,
    }
}

/// The pid of a Chrome still running in `profile`, from its `SingletonLock`.
fn chrome_holder(profile: &Path, alive: impl Fn(u32) -> bool) -> Result<Option<u32>, BrowserError> {
    let lock = profile.join(SINGLETON_LOCK);
    let target = match std::fs::read_link(&lock) {
        Ok(target) => Some(target.to_string_lossy().into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        // There, but not a symlink: it names no pid.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Some(String::new()),
        Err(e) => {
            return Err(BrowserError::Profile(format!(
                "reading {}: {e}",
                lock.display()
            )))
        }
    };
    Ok(match judge_chrome_lock(target.as_deref(), alive) {
        ChromeLock::Held(pid) => Some(pid),
        ChromeLock::Free | ChromeLock::Stale => None,
    })
}

/// Whether `pid` is a running process that could be a Chrome holding the
/// profile: never this process, which has not launched one yet.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return false;
    }
    match libc::pid_t::try_from(pid) {
        // SAFETY: signal 0 sends nothing; kill(2) only reports whether the
        // pid exists and may be signalled.
        Ok(pid) if pid > 0 => unsafe { libc::kill(pid, 0) == 0 },
        _ => false,
    }
}

/// Chrome's lock is a symlink only on unix; elsewhere it is not read.
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// Remove what a Chrome that is no longer running left in the profile
/// ([`LEFTOVERS`]): files and links inside the profile only.
fn remove_leftovers(profile: &Path) -> Result<(), BrowserError> {
    for name in LEFTOVERS {
        let path = profile.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(BrowserError::Profile(format!(
                    "removing the stale {}: {e}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

/// Remove `path`, retrying `attempts` times `pause` apart: Chrome's helper
/// processes can still be writing into the profile for a moment after the
/// browser process exits, which fails a single `remove_dir_all` with
/// "directory not empty". A path already gone counts as removed; the last
/// error is returned once the attempts run out. `remove_dir_all` removes a
/// symlink inside the tree, never what it points to.
pub(crate) fn remove_dir_patiently(
    path: &Path,
    attempts: u32,
    pause: Duration,
) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            std::thread::sleep(pause);
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("not removed")))
}

pub(crate) fn removal_warning(path: &Path, e: &std::io::Error) -> String {
    format!(
        "warning: could not remove the browser profile {} ({e}); it may hold the signed-in \
         session, so delete it",
        path.display()
    )
}

/// 64 bits from the standard library's randomly keyed hasher (seeded from
/// the OS), so the profile path cannot be guessed and pre-created.
pub(crate) fn unpredictable() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish()
}

fn fs_safe(prefix: &str) -> String {
    let safe: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "pk-cli".into()
    } else {
        safe
    }
}

pub(crate) fn create_private_dir(path: &Path) -> Result<(), BrowserError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(false)
            .create(path)
            .map_err(|e| {
                BrowserError::Launch(format!(
                    "creating the browser profile dir {} (it must not already exist): {e}",
                    path.display()
                ))
            })?;
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| BrowserError::Launch(format!("checking the browser profile dir: {e}")))?;
        if !meta.is_dir() || meta.permissions().mode() & 0o777 != 0o700 {
            let _ = std::fs::remove_dir_all(path);
            return Err(BrowserError::Launch(
                "the browser profile dir is not a private (0700) directory; refusing to launch"
                    .into(),
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path).map_err(|e| {
            BrowserError::Launch(format!(
                "creating the browser profile dir {} (it must not already exist): {e}",
                path.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests;
