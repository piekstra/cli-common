//! Finding and launching Chrome, and tearing it down.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::BrowserError;
use crate::host::parse_url;
use crate::interrupt;

/// How long Chrome gets to write `DevToolsActivePort` after launch.
const PORT_DEADLINE: Duration = Duration::from_secs(20);
/// How long Chrome gets to shut down on SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Where Chrome (or a Chromium build with the same DevTools protocol) is
/// installed, in search order.
const CANDIDATES: [&str; 9] = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
    "/usr/bin/microsoft-edge",
];

/// The same apps installed for one user, under `~/Applications`.
const USER_CANDIDATES: [&str; 2] = [
    "Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "Applications/Chromium.app/Contents/MacOS/Chromium",
];

/// The environment variable that points `bin` at a Chrome binary:
/// `<PREFIX>_CHROME`, the prefix being the binary name uppercased with `-`
/// and `.` as `_` (`sfi` → `SFI_CHROME`, `example-cli` → `EXAMPLE_CLI_CHROME`).
pub(crate) fn chrome_env_var(bin: &str) -> String {
    let prefix: String = bin
        .chars()
        .map(|c| match c {
            '-' | '.' | ' ' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect();
    format!("{prefix}_CHROME")
}

/// Locate Chrome for `bin`: the path in `<PREFIX>_CHROME` when set (which
/// must exist), else the first standard install location that does.
pub(crate) fn find_chrome(bin: &str) -> Result<PathBuf, BrowserError> {
    let var = chrome_env_var(bin);
    let mut candidates: Vec<PathBuf> = CANDIDATES.iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.extend(USER_CANDIDATES.iter().map(|rel| home.join(rel)));
    }
    choose_chrome(&var, std::env::var_os(&var).map(PathBuf::from), &candidates)
}

fn choose_chrome(
    var: &str,
    override_path: Option<PathBuf>,
    candidates: &[PathBuf],
) -> Result<PathBuf, BrowserError> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path);
        }
        return Err(BrowserError::ChromeNotFound(format!(
            "{var} points at {}, which is not a file",
            path.display()
        )));
    }
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            BrowserError::ChromeNotFound(format!(
                "no Chrome found — install Google Chrome, or set {var} to a Chrome binary"
            ))
        })
}

/// The owner-only scratch profile Chrome runs in, removed when dropped.
///
/// It holds the live session while the window is open, and Chrome writes
/// the debug port into it, so it is created `0700`, exclusively (an
/// existing path, planted or stale, is refused rather than adopted), under
/// an unpredictable name, and checked to be a real directory with that mode
/// before Chrome is pointed at it.
#[derive(Debug)]
pub(crate) struct ProfileDir {
    path: PathBuf,
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
        Ok(ProfileDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProfileDir {
    /// A profile left behind may hold the signed-in session, so a failure
    /// to remove it is reported even under `-q`: it is the one outcome the
    /// owner has to act on.
    fn drop(&mut self) {
        if let Err(e) = remove_dir_patiently(&self.path, 10, Duration::from_millis(100)) {
            eprintln!("{}", removal_warning(&self.path, &e));
        }
    }
}

/// Remove `path`, retrying `attempts` times `pause` apart: Chrome's helper
/// processes can still be writing into the profile for a moment after the
/// browser process exits, which fails a single `remove_dir_all` with
/// "directory not empty". A path already gone counts as removed; the last
/// error is returned once the attempts run out.
fn remove_dir_patiently(path: &Path, attempts: u32, pause: Duration) -> std::io::Result<()> {
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

fn removal_warning(path: &Path, e: &std::io::Error) -> String {
    format!(
        "warning: could not remove the browser profile {} ({e}); it may hold the signed-in \
         session, so delete it",
        path.display()
    )
}

/// 64 bits from the standard library's randomly keyed hasher (seeded from
/// the OS), so the profile path cannot be guessed and pre-created.
fn unpredictable() -> u64 {
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

fn create_private_dir(path: &Path) -> Result<(), BrowserError> {
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

/// Chrome writes `<profile>/DevToolsActivePort` once its debug server
/// listens: the port on line 1, the browser target's websocket path on
/// line 2. `None` until both are there and well-formed (the file can be
/// read half-written).
pub(crate) fn parse_active_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port = lines
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)?;
    let path = lines.next()?.trim();
    let id = path.strip_prefix("/devtools/browser/")?;
    let id_ok = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    id_ok.then(|| (port, path.to_string()))
}

/// Whether `url` may be handed to Chrome as a start page: `https` with a
/// host. Anything else, notably a string Chrome would parse as a switch
/// (`--…`), is refused.
pub(crate) fn start_url_ok(url: &str) -> bool {
    url.starts_with("https://") && parse_url(url).is_some_and(|u| u.scheme == "https")
}

/// The arguments Chrome is launched with: a debug port Chrome picks itself
/// (no fixed port to collide with or to guess), the scratch profile, and
/// first-run suppression, then the start pages. Nothing else on purpose: a
/// launch trimmed further (background networking or services disabled)
/// reads as automation to bot checks such as Cloudflare Turnstile, and none
/// of the flags a WebDriver launch adds are here, so the page sees
/// `navigator.webdriver == false`.
pub(crate) fn chrome_args(profile: &Path, urls: &[String]) -> Vec<OsString> {
    let mut user_data_dir = OsString::from("--user-data-dir=");
    user_data_dir.push(profile.as_os_str());
    let mut args = vec![
        OsString::from("--remote-debugging-port=0"),
        user_data_dir,
        OsString::from("--no-first-run"),
        OsString::from("--no-default-browser-check"),
    ];
    args.extend(urls.iter().map(OsString::from));
    args
}

/// How to launch the sign-in window. Build with [`Launch::new`], add the
/// start page(s), then [`Launch::spawn`].
#[derive(Debug, Clone)]
pub struct Launch {
    pub(crate) bin: String,
    urls: Vec<String>,
    chrome: Option<PathBuf>,
    pub(crate) quiet: bool,
    ctrl_c: bool,
    pub(crate) login_hint: Option<String>,
}

impl Launch {
    /// A launch for the CLI `bin` (its binary name: the `<PREFIX>_CHROME`
    /// override, the profile name and the messages use it).
    pub fn new(bin: impl Into<String>) -> Self {
        Launch {
            bin: bin.into(),
            urls: Vec::new(),
            chrome: None,
            quiet: false,
            ctrl_c: true,
            login_hint: None,
        }
    }

    /// Open `url` (an `https` URL) as a tab. Call once per tab; at least
    /// one is required.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.urls.push(url.into());
        self
    }

    /// Use this Chrome binary instead of searching for one.
    pub fn chrome(mut self, path: impl Into<PathBuf>) -> Self {
        self.chrome = Some(path.into());
        self
    }

    /// Suppress the status lines on stderr (the CLI's `-q`).
    pub fn quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    /// Whether to install the crate's Ctrl-C handler (default on). A CLI
    /// with its own handler turns this off and calls
    /// [`cancel`](crate::cancel) from it.
    pub fn ctrl_c(mut self, install: bool) -> Self {
        self.ctrl_c = install;
        self
    }

    /// The command the timeout and closed-window errors tell the owner to
    /// re-run. Default `<bin> auth login --browser`.
    pub fn login_hint(mut self, hint: impl Into<String>) -> Self {
        self.login_hint = Some(hint.into());
        self
    }

    /// Launch Chrome and wait (up to 20 s) for its debug port. The returned
    /// [`Browser`] kills Chrome and removes the profile when dropped, on
    /// every path including errors after this point.
    pub fn spawn(self) -> Result<Browser, BrowserError> {
        if self.urls.is_empty() {
            return Err(BrowserError::Invalid(
                "a browser sign-in needs at least one start URL".into(),
            ));
        }
        if let Some(bad) = self.urls.iter().find(|u| !start_url_ok(u)) {
            return Err(BrowserError::Invalid(format!(
                "start URL `{bad}` is not an https URL"
            )));
        }
        let chrome = match &self.chrome {
            Some(path) => path.clone(),
            None => find_chrome(&self.bin)?,
        };
        if self.ctrl_c {
            if let Err(e) = interrupt::install() {
                if !self.quiet {
                    eprintln!(
                        "warning: Ctrl-C cleanup handler not installed ({e}); an interrupt \
                         during the sign-in may leave the Chrome window open"
                    );
                }
            }
        }

        let profile = ProfileDir::create(&self.bin)?;
        let child = Command::new(&chrome)
            .args(chrome_args(profile.path(), &self.urls))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| BrowserError::Launch(format!("launching {}: {e}", chrome.display())))?;
        let live = interrupt::register(child.id(), profile.path().to_path_buf());
        let mut browser = Browser {
            child,
            live,
            port: 0,
            ws_path: String::new(),
            launch: self,
            profile,
        };
        let (port, path) = browser.read_active_port()?;
        browser.port = port;
        browser.ws_path = path;
        Ok(browser)
    }
}

/// A running scratch-profile Chrome. Dropping it stops Chrome (SIGTERM,
/// then SIGKILL after a grace period) and removes its profile.
pub struct Browser {
    child: Child,
    live: u64,
    pub(crate) port: u16,
    pub(crate) ws_path: String,
    pub(crate) launch: Launch,
    // Last: removed after the child has been stopped by `Drop::drop`.
    profile: ProfileDir,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("pid", &self.child.id())
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Browser {
    /// Whether Chrome has exited (the owner quit it, or it crashed).
    pub(crate) fn exited(&mut self) -> bool {
        self.try_reap().is_some()
    }

    fn try_reap(&mut self) -> Option<ExitStatus> {
        let child = &mut self.child;
        interrupt::reap_with(self.live, || child.try_wait().ok().flatten())
    }

    fn read_active_port(&mut self) -> Result<(u16, String), BrowserError> {
        let file = self.profile.path().join("DevToolsActivePort");
        let deadline = Instant::now() + PORT_DEADLINE;
        loop {
            if let Some(found) = std::fs::read_to_string(&file)
                .ok()
                .as_deref()
                .and_then(parse_active_port)
            {
                return Ok(found);
            }
            if let Some(status) = self.try_reap() {
                return Err(BrowserError::Launch(format!(
                    "Chrome exited before opening its debug port ({status})"
                )));
            }
            if interrupt::canceled() {
                return Err(BrowserError::Canceled);
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Launch(
                    "Chrome did not open its debug port within 20 seconds".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) {
        if self.try_reap().is_some() {
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            // SAFETY: kill(2) on our own child, which has not been reaped
            // (`try_reap` just said so), so the pid is still ours.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            let grace = Instant::now() + STOP_GRACE;
            while Instant::now() < grace {
                if self.try_reap().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        // SIGKILL is not ignorable, so this ends within milliseconds; the
        // bound keeps a drop from ever hanging the CLI on a process it
        // cannot reap.
        let bound = Instant::now() + STOP_GRACE;
        while self.try_reap().is_none() && Instant::now() < bound {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop();
        interrupt::unregister(self.live);
        // `profile` drops after this, removing the directory.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_variable_follows_the_family_prefix() {
        assert_eq!(chrome_env_var("sfi"), "SFI_CHROME");
        assert_eq!(chrome_env_var("m1f"), "M1F_CHROME");
        assert_eq!(chrome_env_var("example-cli"), "EXAMPLE_CLI_CHROME");
    }

    #[test]
    fn chrome_is_found_from_the_override_or_the_first_install() {
        let dir = std::env::temp_dir().join(format!("pk-cli-browser-find-{}", unpredictable()));
        std::fs::create_dir(&dir).unwrap();
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::write(&second, "").unwrap();
        let missing = dir.join("missing");

        // The first candidate that exists wins.
        let found = choose_chrome("X_CHROME", None, &[first.clone(), second.clone()]).unwrap();
        assert_eq!(found, second);
        // The override wins over the candidates when it exists...
        std::fs::write(&first, "").unwrap();
        let found = choose_chrome(
            "X_CHROME",
            Some(second.clone()),
            std::slice::from_ref(&first),
        )
        .unwrap();
        assert_eq!(found, second);
        // ...and is an error, naming the variable, when it does not; a
        // directory is not a binary either.
        for bad in [missing.clone(), dir.clone()] {
            let err =
                choose_chrome("X_CHROME", Some(bad), std::slice::from_ref(&first)).unwrap_err();
            assert!(err.to_string().contains("X_CHROME"), "{err}");
        }
        // Nothing found: the error says how to point at one.
        let err = choose_chrome("X_CHROME", None, &[missing]).unwrap_err();
        assert!(err.to_string().contains("set X_CHROME"), "{err}");
        assert!(matches!(err, BrowserError::ChromeNotFound(_)), "{err:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_active_port_file_is_read_only_when_complete() {
        let cases: &[(&str, Option<(u16, &str)>)] = &[
            (
                "9222\n/devtools/browser/0a1b-2c3d\n",
                Some((9222, "/devtools/browser/0a1b-2c3d")),
            ),
            (
                "51234\r\n/devtools/browser/abc\r\n",
                Some((51234, "/devtools/browser/abc")),
            ),
            (
                "9222\n/devtools/browser/abc",
                Some((9222, "/devtools/browser/abc")),
            ),
            // Half-written, empty, or malformed: keep waiting.
            ("", None),
            ("9222\n", None),
            ("9222", None),
            ("0\n/devtools/browser/abc\n", None),
            ("70000\n/devtools/browser/abc\n", None),
            ("port\n/devtools/browser/abc\n", None),
            ("9222\n/devtools/page/abc\n", None),
            ("9222\n/devtools/browser/\n", None),
            ("9222\n/devtools/browser/a?b\n", None),
            ("9222\n/devtools/browser/a/../b\n", None),
        ];
        for (text, want) in cases {
            let got = parse_active_port(text);
            let got = got.as_ref().map(|(p, s)| (*p, s.as_str()));
            assert_eq!(got, *want, "{text:?}");
        }
    }

    #[test]
    fn only_https_start_urls_are_handed_to_chrome() {
        assert!(start_url_ok("https://example.com"));
        assert!(start_url_ok("https://app.example.com/login?next=/"));
        for bad in [
            "http://example.com/",
            "--remote-allow-origins=*",
            "--user-data-dir=/tmp/x",
            "-v",
            "example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            "https://user@example.com/",
            "",
        ] {
            assert!(!start_url_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn chrome_gets_a_fresh_port_the_profile_and_the_pages_last() {
        let args = chrome_args(
            Path::new("/tmp/p"),
            &["https://a.example/".into(), "https://b.example/".into()],
        );
        assert_eq!(
            args,
            [
                "--remote-debugging-port=0",
                "--user-data-dir=/tmp/p",
                "--no-first-run",
                "--no-default-browser-check",
                "https://a.example/",
                "https://b.example/",
            ]
        );
        assert!(!args
            .iter()
            .any(|a| a.to_string_lossy().contains("automation")));
    }

    #[test]
    fn the_launch_refuses_bad_start_urls_before_touching_anything() {
        let err = Launch::new("t")
            .chrome("/nonexistent/chrome")
            .ctrl_c(false)
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("start URL"), "{err}");
        let err = Launch::new("t")
            .url("--remote-allow-origins=*")
            .chrome("/nonexistent/chrome")
            .ctrl_c(false)
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("not an https URL"), "{err}");
    }

    /// The give-up path: a profile that cannot be removed (here: its parent
    /// made read-only) ends the retries with the error, never a hang, and
    /// the warning names the path. Skipped where permissions do not bind
    /// (running as root).
    #[cfg(unix)]
    #[test]
    fn a_profile_that_cannot_be_removed_is_reported_not_hung_on() {
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!("pk-cli-browser-stuck-{}", unpredictable()));
        std::fs::create_dir(&parent).unwrap();
        let profile = ProfileDir::create_in(&parent, "t").unwrap();
        let path = profile.path().to_path_buf();
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&parent, mode(0o500)).unwrap();
        let started = Instant::now();
        let result = remove_dir_patiently(&path, 3, Duration::from_millis(1));
        std::fs::set_permissions(&parent, mode(0o700)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        if let Err(e) = result {
            assert!(path.exists());
            let warning = removal_warning(&path, &e);
            assert!(warning.starts_with("warning: could not remove the browser profile "));
            assert!(warning.contains(&path.display().to_string()), "{warning}");
            assert!(
                warning.contains("may hold the signed-in session"),
                "{warning}"
            );
        }
        drop(profile);
        assert!(!path.exists(), "removed once it can be");
        // A path already gone is not an error.
        assert!(remove_dir_patiently(&path, 1, Duration::ZERO).is_ok());
        std::fs::remove_dir(&parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_profile_is_private_exclusive_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!("pk-cli-browser-prof-{}", unpredictable()));
        std::fs::create_dir(&parent).unwrap();

        let profile = ProfileDir::create_in(&parent, "my cli/../x").unwrap();
        let path = profile.path().to_path_buf();
        assert_eq!(path.parent(), Some(parent.as_path()), "no traversal");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("my_cli____x-browser-login-"), "{name}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Two profiles never share a name.
        let other = ProfileDir::create_in(&parent, "my cli/../x").unwrap();
        assert_ne!(other.path(), path);

        // An existing path is refused, never adopted.
        let err = create_private_dir(&path).unwrap_err();
        assert!(err.to_string().contains("must not already exist"), "{err}");

        // Dropping removes the directory and whatever Chrome wrote into it.
        std::fs::create_dir(path.join("Default")).unwrap();
        std::fs::write(path.join("Default").join("Cookies"), "x").unwrap();
        drop(profile);
        assert!(!path.exists());
        drop(other);
        std::fs::remove_dir(&parent).unwrap();
    }

    /// A stand-in for Chrome (tests/fixtures/fake-chrome.sh) that writes
    /// `DevToolsActivePort` into the profile and sleeps, or exits early when
    /// its start page asks it to. Exercises launch, the port read and the
    /// teardown without a browser.
    #[cfg(unix)]
    const FAKE_CHROME: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-chrome.sh");

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn a_launched_browser_is_stopped_and_its_profile_removed_on_drop() {
        let mut browser = Launch::new("pk-cli-browser-test")
            .url("https://app.example.com/login")
            .chrome(FAKE_CHROME)
            .ctrl_c(false)
            .quiet(true)
            .spawn()
            .unwrap();
        assert_eq!(browser.port, 9222);
        assert_eq!(browser.ws_path, "/devtools/browser/fake-browser-id");
        let profile = browser.profile.path().to_path_buf();
        let args = std::fs::read_to_string(profile.join("args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "--remote-debugging-port=0",
                &format!("--user-data-dir={}", profile.display()),
                "--no-first-run",
                "--no-default-browser-check",
                "https://app.example.com/login",
            ]
        );
        assert!(!browser.exited());
        let pid = browser.child.id();
        assert!(alive(pid));
        drop(browser);
        assert!(!alive(pid), "Chrome is stopped");
        assert!(!profile.exists(), "the profile is removed");
    }

    #[cfg(unix)]
    #[test]
    fn a_browser_that_dies_on_launch_is_reported_and_cleaned_up() {
        let before = profiles_named("pk-cli-browser-early");
        let err = Launch::new("pk-cli-browser-early")
            .url("https://exit-early.example/")
            .chrome(FAKE_CHROME)
            .ctrl_c(false)
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("exited before"), "{err}");
        assert_eq!(profiles_named("pk-cli-browser-early"), before);

        let err = Launch::new("pk-cli-browser-early")
            .url("https://app.example.com/")
            .chrome("/nonexistent/chrome")
            .ctrl_c(false)
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("launching"), "{err}");
        assert_eq!(profiles_named("pk-cli-browser-early"), before);
    }

    #[cfg(unix)]
    fn profiles_named(prefix: &str) -> usize {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{prefix}-browser-login-{}-", std::process::id()))
            })
            .count()
    }
}
