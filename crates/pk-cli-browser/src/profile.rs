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
//! a live pid that runs a Chrome) refuses the launch. Nothing outside the
//! profile directory is ever removed or rewritten.
//!
//! The kept profile needs a unix system (macOS, Linux): the run lock is
//! `flock(2)` and Chrome's lock a symlink. Elsewhere every sign-in uses a
//! scratch profile ([`KEPT_SUPPORTED`]).

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
/// profile, and the record of the programs that ran there: one absolute
/// path per line ([`read_record`]).
#[cfg_attr(not(unix), allow(dead_code))]
const RUN_LOCK: &str = "pk-cli-browser.lock";
/// The most of the record that is read: far more than the two paths a
/// sign-in writes.
const RECORD_LIMIT: u64 = 16 * 1024;
/// Whether this platform can keep a profile between sign-ins with the
/// guarantees the docs give (owner-only, one sign-in at a time).
pub(crate) const KEPT_SUPPORTED: bool = cfg!(unix);

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
    // The record of what runs there is written through it.
    run_lock: Option<File>,
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
            run_lock: None,
        })
    }

    /// Open `bin`'s kept profile under `root` (the per-user data dir),
    /// creating it `0700` if it is missing, for one sign-in that launches
    /// `chrome`.
    ///
    /// Refused as [`BrowserError::Profile`] when the directory is a
    /// symlink, is not a directory, is not owned by the current user or is
    /// not `0700`, and as [`BrowserError::ProfileInUse`] when another
    /// sign-in holds it or Chrome's lock in it names a running Chrome: a
    /// program the profile's record names (what earlier sign-ins ran there),
    /// `chrome` itself, or one named like the Chromium family. A lock whose
    /// pid is gone, or now runs another program, is stale: it is removed,
    /// with the rest of what that Chrome left behind, the record is
    /// replaced by `chrome`'s path, and the sign-in goes ahead.
    pub fn open_kept(
        root: &Path,
        bin: &str,
        name: Option<&str>,
        chrome: &Path,
    ) -> Result<Self, BrowserError> {
        let path = kept_profile_path(root, bin, name);
        ensure_kept_dir(&path)?;
        let mut run_lock = match lock_for_run(&path)? {
            RunLock::Acquired(file) => file,
            RunLock::Busy => {
                return Err(BrowserError::ProfileInUse(format!(
                    "{}; finish or cancel that one first, or pass --fresh-profile",
                    held_message(bin, &path, None)
                )))
            }
            RunLock::Unsupported => return Err(unsupported()),
        };
        let launched = canonical(chrome);
        let mut known = read_record(&mut run_lock);
        known.push(launched.clone());
        if let Some(pid) = chrome_holder(&path, &known)? {
            return Err(BrowserError::ProfileInUse(format!(
                "{}; close that window, then run this again (or pass --fresh-profile). {}",
                held_message(bin, &path, Some(pid)),
                clear_by_hand(&path)
            )));
        }
        remove_leftovers(&path)?;
        write_record(&mut run_lock, &[launched]).map_err(|e| {
            BrowserError::Profile(format!(
                "recording the browser in {}: {e}",
                path.join(RUN_LOCK).display()
            ))
        })?;
        Ok(ProfileDir {
            path,
            kept: true,
            run_lock: Some(run_lock),
        })
    }

    /// Once Chrome is up, add the program it runs as, read from the pid its
    /// `SingletonLock` names, to the kept profile's record. What was
    /// spawned is recorded already; this covers a launcher that starts the
    /// browser under another program (Linux's `google-chrome` script, a
    /// wrapper set in `<PREFIX>_CHROME`), so a later check knows that
    /// browser by its own program. Best effort: without a lock or a
    /// readable program, the record keeps what was spawned.
    pub fn note_running_chrome(&mut self) {
        let Some(file) = self.run_lock.as_mut() else {
            return;
        };
        let pid = std::fs::read_link(self.path.join(SINGLETON_LOCK))
            .ok()
            .and_then(|target| lock_pid(&target.to_string_lossy()));
        let Some(exe) = pid.and_then(process_exe) else {
            return;
        };
        let exe = canonical(&exe);
        let mut programs = read_record(file);
        if !programs.contains(&exe) {
            programs.push(exe);
            let _ = write_record(file, &programs);
        }
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

/// Which profile a launch runs in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProfileChoice {
    /// The CLI's kept profile.
    Kept,
    /// A scratch profile, asked for (`--fresh-profile`).
    Scratch,
    /// A scratch profile because this platform cannot keep one; the launch
    /// says so in a note.
    ScratchUnsupported,
}

pub(crate) fn profile_choice(fresh: bool, kept_supported: bool) -> ProfileChoice {
    if fresh {
        ProfileChoice::Scratch
    } else if kept_supported {
        ProfileChoice::Kept
    } else {
        ProfileChoice::ScratchUnsupported
    }
}

/// The note a launch prints when it falls back to a scratch profile.
pub(crate) const UNSUPPORTED_NOTE: &str = "note: a kept browser profile needs macOS or Linux; \
     this sign-in uses a fresh profile, removed afterwards, so expect the provider's full sign-in";

fn unsupported() -> BrowserError {
    BrowserError::Profile(
        "a kept browser profile needs macOS or Linux (flock and Chrome's symlink lock); pass \
         --fresh-profile"
            .into(),
    )
}

/// Remove the browser profile `bin` keeps between sign-ins, signing the
/// browser out of every provider it was signed in to: the next
/// `auth login --browser` starts from an unknown device. A CLI calls it from
/// `auth logout`. `Ok(true)` when there was a profile to remove, `Ok(false)`
/// when there was none.
///
/// Only the profile directory is removed: never its parent, and never what
/// a link inside it points to. Refused as [`BrowserError::ProfileInUse`]
/// while a sign-in or a running Chrome is using the profile (close the
/// window, then log out again), and as [`BrowserError::Profile`] when the
/// path is a symlink, is not a directory or belongs to another user. A CLI
/// whose logout also clears other state does that first, so a refusal here
/// leaves the rest of the logout done.
///
/// It removes the owner's real profile, so a CLI's tests never reach it:
/// the logout takes this call through a seam the tests fill with
/// `forget_profile_in` (the `test-util` feature) and a temp dir.
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

/// [`forget_profile`] under `root` instead of the per-user data dir: the
/// profile at `<root>/piekstra.<bin>/browser-profile`. For a CLI's tests
/// only (the `test-util` feature), so a logout test runs against a temp dir
/// and never removes the owner's real profile.
#[cfg(any(test, feature = "test-util"))]
pub fn forget_profile_in(root: &Path, bin: &str) -> Result<bool, BrowserError> {
    forget_in(root, bin, None)
}

/// [`forget_named_profile`] under `root` instead of the per-user data dir.
/// For a CLI's tests only (the `test-util` feature).
#[cfg(any(test, feature = "test-util"))]
pub fn forget_named_profile_in(root: &Path, bin: &str, name: &str) -> Result<bool, BrowserError> {
    if !profile_name_ok(name) {
        return Err(invalid_name(name));
    }
    forget_in(root, bin, Some(name))
}

pub(crate) fn invalid_name(name: &str) -> BrowserError {
    BrowserError::Invalid(format!(
        "browser profile name `{name}` must be 1 to 64 ASCII letters, digits, `-` or `_`"
    ))
}

/// `<root>/piekstra.<bin>/browser-profile`, or
/// `browser-profile-<`[`name_dir`]`(name)>` for a named profile. `bin` is
/// made filesystem-safe; `name` must already have passed
/// [`profile_name_ok`].
pub(crate) fn kept_profile_path(root: &Path, bin: &str, name: Option<&str>) -> PathBuf {
    let leaf = match name {
        None => KEPT_PROFILE.to_string(),
        Some(name) => format!("{KEPT_PROFILE}-{}", name_dir(name)),
    };
    root.join(format!("piekstra.{}", fs_safe(bin))).join(leaf)
}

/// A profile name as a directory name that stays distinct on a filesystem
/// that folds case (APFS and NTFS do by default): lowercase letters, digits,
/// `-` and `_` stay as they are, and each uppercase letter becomes `+` and
/// its lowercase, so `Alice` is `+alice` and `alice` is `alice`. A name never
/// holds a `+` and the result holds no uppercase, so two names never share
/// a directory, whether or not the filesystem tells case apart.
pub(crate) fn name_dir(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 2);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('+');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A profile name is 1 to 64 ASCII letters, digits, `-` or `_`; upper and
/// lower case are told apart ([`name_dir`]). The account name is checked,
/// never rewritten into another name, so two names can never map onto one
/// profile.
pub(crate) fn profile_name_ok(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Remove `bin`'s kept profile under `root` (the default one, or `name`'s).
/// `Ok(false)` when there was none. Refused when the path is a symlink, is
/// not a directory or is another user's, and while a sign-in or a running
/// Chrome (a program the profile's record names, or one named like the
/// Chromium family) is using the profile. Removes the profile directory
/// only, never its parent or anything a link in it points to.
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
    let (run_lock, known) = match lock_for_run(&path)? {
        RunLock::Acquired(mut file) => {
            let known = read_record(&mut file);
            (Some(file), known)
        }
        RunLock::Busy => {
            return Err(BrowserError::ProfileInUse(format!(
                "{}; finish or cancel it, then log out again",
                held_message(bin, &path, None)
            )))
        }
        // No sign-in runs in a kept profile here (each uses a scratch one),
        // so none can be using this one.
        RunLock::Unsupported => (None, Vec::new()),
    };
    if let Some(pid) = chrome_holder(&path, &known)? {
        return Err(BrowserError::ProfileInUse(format!(
            "{}; close that window, then log out again. {}",
            held_message(bin, &path, Some(pid)),
            clear_by_hand(&path)
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

/// What to do when a lock is judged held but no Chrome is in fact open
/// there.
fn clear_by_hand(path: &Path) -> String {
    format!(
        "If no Chrome has it open, remove {} and try again",
        path.join(SINGLETON_LOCK).display()
    )
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

/// The kept profile's run lock, as [`lock_for_run`] found it.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum RunLock {
    /// Held until the file is closed. The kernel releases it if the process
    /// dies, so it is never stale.
    Acquired(File),
    /// Another sign-in holds it.
    Busy,
    /// This platform has no run lock.
    #[cfg_attr(unix, allow(dead_code))]
    Unsupported,
}

/// Take the kept profile's run lock without waiting. The file is opened
/// without following a symlink and is not inherited by Chrome.
#[cfg(unix)]
fn lock_for_run(profile: &Path) -> Result<RunLock, BrowserError> {
    lock_file(profile).map_err(|e| {
        BrowserError::Profile(format!(
            "locking the browser profile {}: {e}",
            profile.display()
        ))
    })
}

#[cfg(unix)]
fn lock_file(profile: &Path) -> std::io::Result<RunLock> {
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
        return Ok(RunLock::Acquired(file));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(RunLock::Busy)
    } else {
        Err(err)
    }
}

#[cfg(not(unix))]
fn lock_for_run(_profile: &Path) -> Result<RunLock, BrowserError> {
    Ok(RunLock::Unsupported)
}

/// The programs recorded as run in a kept profile, read from its run lock
/// file: each line that is an absolute path, in UTF-8. Anything else in the
/// file is skipped; a record is only ever a reason to treat a running
/// program as a Chrome, never to treat one as gone.
pub(crate) fn read_record(file: &mut File) -> Vec<PathBuf> {
    use std::io::{Read, Seek, SeekFrom};
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(0)).is_err()
        || file
            .by_ref()
            .take(RECORD_LIMIT)
            .read_to_end(&mut bytes)
            .is_err()
    {
        return Vec::new();
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect()
}

/// Replace the record with `programs`. A path that is not absolute, or not
/// UTF-8, or holds a line break, cannot be written as a line and is left
/// out; the program is then known by its family name only.
pub(crate) fn write_record(file: &mut File, programs: &[PathBuf]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut text = String::new();
    for program in programs.iter().filter(|p| p.is_absolute()) {
        if let Some(line) = program.to_str().filter(|l| !l.contains(['\n', '\r'])) {
            text.push_str(line);
            text.push('\n');
        }
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(text.as_bytes())?;
    file.flush()
}

/// `path` with its links resolved, or as given when it cannot be (it no
/// longer exists).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// What Chrome's `SingletonLock` says about a profile.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChromeLock {
    /// No lock: no Chrome has the profile open.
    Free,
    /// A lock whose pid is not running, or now runs a program that is not a
    /// Chrome (a pid reused after a reboot), or that names no pid: left by a
    /// Chrome that was killed or crashed.
    Stale,
    /// A lock naming a running Chrome: it has the profile open.
    Held(u32),
}

/// What the pid in Chrome's lock is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum PidNow {
    /// No process this user owns has it.
    Gone,
    /// A running process whose program is not a Chrome.
    NotChrome,
    /// A running Chrome.
    Chrome,
}

/// Judge Chrome's lock from its symlink target (`None` when there is no
/// lock), `<host>-<pid>`, with `pid_now` saying what that pid is now. Only
/// a running Chrome holds the profile. The host is not compared: the
/// profile is in a per-user local directory, and a Mac's host name changes
/// with its network, which would make a live Chrome's lock look foreign. A
/// target with no pid is stale, as Chrome itself treats it.
pub(crate) fn judge_chrome_lock(
    target: Option<&str>,
    pid_now: impl Fn(u32) -> PidNow,
) -> ChromeLock {
    let Some(target) = target else {
        return ChromeLock::Free;
    };
    match lock_pid(target) {
        Some(pid) if pid_now(pid) == PidNow::Chrome => ChromeLock::Held(pid),
        _ => ChromeLock::Stale,
    }
}

/// The pid in a `SingletonLock` target, `<host>-<pid>`: after the last `-`,
/// and never 0.
fn lock_pid(target: &str) -> Option<u32> {
    target
        .rsplit_once('-')
        .and_then(|(_, pid)| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
}

/// Whether `exe`, a running process's program, is a Chrome: one of `known`
/// (the programs the profile's record names and the one being launched;
/// links resolved on both sides), or named like the Chromium family
/// (`Google Chrome`, `Chromium`, `chrome`, `chromium-browser`,
/// `Microsoft Edge`, `msedge`). The name counts too for a Chrome whose
/// program was never recorded (one started before the record existed).
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn is_chrome_exe(exe: &Path, known: &[PathBuf]) -> bool {
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let exe_resolved = resolve(exe);
    if known.iter().any(|k| resolve(k) == exe_resolved) {
        return true;
    }
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    ["chrome", "chromium", "msedge", "microsoft edge"]
        .iter()
        .any(|family| name.contains(family))
}

/// The pid of a Chrome still running in `profile`, from its `SingletonLock`,
/// with `known` the programs a running pid's program is compared against.
fn chrome_holder(profile: &Path, known: &[PathBuf]) -> Result<Option<u32>, BrowserError> {
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
    Ok(
        match judge_chrome_lock(target.as_deref(), |pid| pid_now(pid, known)) {
            ChromeLock::Held(pid) => Some(pid),
            ChromeLock::Free | ChromeLock::Stale => None,
        },
    )
}

/// What `pid` is now. A process this user cannot signal cannot hold an
/// owner-only profile, so it counts as gone; this process is not a Chrome.
/// A running process whose program cannot be read counts as a Chrome,
/// rather than launch a second browser into a profile one may hold.
#[cfg(unix)]
fn pid_now(pid: u32, known: &[PathBuf]) -> PidNow {
    if pid == std::process::id() {
        return PidNow::NotChrome;
    }
    let alive = match libc::pid_t::try_from(pid) {
        // SAFETY: signal 0 sends nothing; kill(2) only reports whether the
        // pid exists and may be signalled.
        Ok(raw) if raw > 0 => unsafe { libc::kill(raw, 0) == 0 },
        _ => false,
    };
    if !alive {
        return PidNow::Gone;
    }
    match process_exe(pid) {
        Some(exe) if is_chrome_exe(&exe, known) => PidNow::Chrome,
        Some(_) => PidNow::NotChrome,
        None => PidNow::Chrome,
    }
}

/// Chrome's lock is a symlink only on unix; elsewhere it is not read.
#[cfg(not(unix))]
fn pid_now(_pid: u32, _known: &[PathBuf]) -> PidNow {
    PidNow::Gone
}

/// The program `pid` runs, from the kernel (`proc_pidpath`).
#[cfg(target_os = "macos")]
fn process_exe(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is writable for the length passed, and proc_pidpath
    // writes at most that many bytes, returning how many it wrote.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let len = usize::try_from(len).ok().filter(|n| *n > 0)?;
    buf.truncate(len);
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf)))
}

/// The program `pid` runs, from `/proc/<pid>/exe`, without the
/// ` (deleted)` Linux adds once the file is replaced (a browser updated
/// while it runs), so it still matches the recorded path.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_exe(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let bytes = exe.as_os_str().as_bytes();
    Some(match bytes.strip_suffix(b" (deleted)") {
        Some(kept) => PathBuf::from(std::ffi::OsString::from_vec(kept.to_vec())),
        None => exe,
    })
}

/// No way to read it here: a running pid then counts as a Chrome, and
/// nothing is added to the record.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn process_exe(_pid: u32) -> Option<PathBuf> {
    None
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
