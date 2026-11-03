//! Finding and launching Chrome, and tearing it down.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::cdp::{Cdp, CdpError, WsTransport};
use crate::error::BrowserError;
use crate::host::parse_url;
use crate::interrupt;
use crate::profile::{
    default_root, invalid_name, profile_choice, profile_name_ok, ProfileChoice, ProfileDir,
    KEPT_SUPPORTED, UNSUPPORTED_NOTE,
};

/// How long Chrome gets to write `DevToolsActivePort` after launch.
const PORT_DEADLINE: Duration = Duration::from_secs(20);
/// How long Chrome gets to shut down on SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// How long a kept profile's Chrome gets to quit when asked over its debug
/// port, connecting included, before the signals take over. Its exit ends the
/// wait, so this costs time only when Chrome is stuck. It is about twice the
/// slowest exit measured on an overloaded machine (DESIGN.md §2.2).
const CLOSE_GRACE: Duration = Duration::from_secs(5);

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
/// (no fixed port to collide with or to guess), the profile, and first-run
/// suppression, then the start pages. Nothing else on purpose: a
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
///
/// Chrome runs in the CLI's kept profile,
/// `<data dir>/piekstra.<bin>/browser-profile` (on macOS
/// `~/Library/Application Support/piekstra.<bin>/browser-profile`, elsewhere
/// under `$XDG_DATA_HOME` or `~/.local/share`), so the provider's cookies
/// (its remembered device, its single-sign-on session, a bot check's
/// clearance) carry over to the next sign-in, which often finishes with no
/// password or code. [`Launch::fresh_profile`] opts out for one run;
/// [`forget_profile`](crate::forget_profile) removes the kept profile.
#[derive(Debug, Clone)]
pub struct Launch {
    pub(crate) bin: String,
    urls: Vec<String>,
    chrome: Option<PathBuf>,
    pub(crate) quiet: bool,
    ctrl_c: bool,
    pub(crate) login_hint: Option<String>,
    fresh_profile: bool,
    profile_name: Option<String>,
    // The data dir the kept profile lives under; `None` is the real one.
    profile_root: Option<PathBuf>,
}

impl Launch {
    /// A launch for the CLI `bin` (its binary name: the `<PREFIX>_CHROME`
    /// override, the profile's directory and the messages use it).
    pub fn new(bin: impl Into<String>) -> Self {
        Launch {
            bin: bin.into(),
            urls: Vec::new(),
            chrome: None,
            quiet: false,
            ctrl_c: true,
            login_hint: None,
            fresh_profile: false,
            profile_name: None,
            profile_root: None,
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

    /// Run Chrome in a fresh scratch profile instead of the kept one: an
    /// owner-only directory in the temp dir, removed when the window
    /// closes, so the provider sees an unknown device and nothing of this
    /// sign-in stays on disk. The kept profile is neither read nor changed.
    /// The CLI's `--fresh-profile` ([`BrowserArgs`](crate::BrowserArgs)).
    /// Default off.
    pub fn fresh_profile(mut self, fresh: bool) -> Self {
        self.fresh_profile = fresh;
        self
    }

    /// Keep a separate profile under `name` beside the CLI's default one,
    /// for a CLI that signs in more than one account at the provider
    /// (`rhd --profile <NAME>`): each account gets its own remembered
    /// device and session, and one account's sign-in never opens already
    /// signed in as another. The profile is
    /// `<data dir>/piekstra.<bin>/browser-profile-<name>`, with each
    /// uppercase letter of `name` written as `+` and its lowercase
    /// (`Alice` is `browser-profile-+alice`, `alice` is
    /// `browser-profile-alice`), so names that differ only in case keep
    /// their own profiles where the filesystem folds case (APFS). It is
    /// removed by [`forget_named_profile`](crate::forget_named_profile).
    /// `name` is 1 to 64 ASCII letters, digits, `-` or `_`; the account name
    /// is checked, never rewritten into another name, and anything else
    /// fails [`spawn`](Launch::spawn) as [`BrowserError::Invalid`]. No
    /// effect with [`fresh_profile(true)`](Launch::fresh_profile).
    pub fn profile_name(mut self, name: impl Into<String>) -> Self {
        self.profile_name = Some(name.into());
        self
    }

    /// Keep the profile under `root` instead of the per-user data dir
    /// (`<root>/piekstra.<bin>/browser-profile`). For a CLI's tests only
    /// (the `test-util` feature), so a test that launches never reads or
    /// changes the owner's real profile; `forget_profile_in` removes it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn profile_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.profile_root = Some(root.into());
        self
    }

    /// Launch Chrome and wait (up to 20 s) for its debug port. The returned
    /// [`Browser`] kills Chrome when dropped, on every path including errors
    /// after this point, and removes a scratch profile; the kept profile
    /// stays for the next sign-in.
    ///
    /// Before Chrome starts, the kept profile is created `0700` if missing,
    /// and refused as [`BrowserError::Profile`] when it is a symlink, is not
    /// the current user's or is not `0700`, and as
    /// [`BrowserError::ProfileInUse`] when another sign-in is using it or a
    /// Chrome is still running in it. What a Chrome that is no longer
    /// running left in it (its lock, the old debug port) is removed. Off
    /// unix there is no kept profile: the sign-in uses a scratch one and
    /// says so in a note on stderr (unless quiet).
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
        if let Some(name) = self.profile_name.as_deref().filter(|n| !profile_name_ok(n)) {
            return Err(invalid_name(name));
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

        let profile = match profile_choice(self.fresh_profile, KEPT_SUPPORTED) {
            ProfileChoice::Scratch => ProfileDir::create(&self.bin)?,
            ProfileChoice::ScratchUnsupported => {
                if !self.quiet {
                    eprintln!("{UNSUPPORTED_NOTE}");
                }
                ProfileDir::create(&self.bin)?
            }
            ProfileChoice::Kept => {
                let root = match &self.profile_root {
                    Some(root) => root.clone(),
                    None => default_root().map_err(|e| {
                        BrowserError::Profile(format!(
                            "{e}; pass --fresh-profile to sign in without it"
                        ))
                    })?,
                };
                ProfileDir::open_kept(&root, &self.bin, self.profile_name.as_deref(), &chrome)?
            }
        };
        let child = Command::new(&chrome)
            .args(chrome_args(profile.path(), &self.urls))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| BrowserError::Launch(format!("launching {}: {e}", chrome.display())))?;
        let live = interrupt::register(child.id(), profile.scratch_path());
        let mut browser = Browser {
            child,
            live,
            port: 0,
            ws_path: String::new(),
            launch: self,
            close_grace: CLOSE_GRACE,
            profile,
        };
        let (port, path) = browser.read_active_port()?;
        browser.port = port;
        browser.ws_path = path;
        browser.profile.note_running_chrome();
        Ok(browser)
    }
}

/// A running sign-in Chrome. Dropping it stops Chrome, then removes a scratch
/// profile or, for the kept profile, releases it for the next sign-in. Chrome
/// in the kept profile is first asked to quit over its debug port, so that it
/// writes a cookie it just received to disk. If that fails or takes too long,
/// and for a scratch profile, it gets SIGTERM, then SIGKILL after a grace
/// period.
pub struct Browser {
    child: Child,
    live: u64,
    pub(crate) port: u16,
    pub(crate) ws_path: String,
    pub(crate) launch: Launch,
    close_grace: Duration,
    // Last: removed or released after the child has been stopped by
    // `Drop::drop`.
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

    /// Ask Chrome to quit over its debug port and wait for it to exit. Chrome
    /// holds a cookie it just received in memory and writes it on a normal
    /// exit, which SIGTERM skips. `false` means it is still running.
    fn close_over_debug_port(&mut self) -> bool {
        // Zero is a launch that ended before the port was read.
        if self.port == 0 {
            return false;
        }
        let deadline = Instant::now() + self.close_grace;
        let Ok(transport) = WsTransport::connect(self.port, &self.ws_path) else {
            return false;
        };
        let answer_within = deadline.saturating_duration_since(Instant::now());
        // Never canceled: a first Ctrl-C is one way to get here, and the
        // cookie is still worth keeping.
        let mut cdp = Cdp::new(transport, answer_within, || false);
        match cdp.call("Browser.close", Value::Null, None) {
            // Chrome can close the socket before it answers.
            Ok(_) | Err(CdpError::Transport(_)) => {}
            Err(_) => return false,
        }
        while Instant::now() < deadline {
            if self.try_reap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn stop(&mut self) {
        if self.try_reap().is_some() {
            return;
        }
        // A scratch profile is about to be removed, so its cookies do not matter.
        if self.profile.scratch_path().is_none() && self.close_over_debug_port() {
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
        // `profile` drops after this: a scratch directory is removed, the
        // kept one's lock released.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::unpredictable;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};

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
        // A profile name that would need rewriting is refused, not mapped.
        for bad in ["", "a b", "../x", "a.b", &"x".repeat(65)] {
            let err = Launch::new("t")
                .url("https://app.example.com/")
                .chrome("/nonexistent/chrome")
                .ctrl_c(false)
                .profile_name(bad)
                .spawn()
                .unwrap_err();
            assert!(matches!(err, BrowserError::Invalid(_)), "{bad:?}: {err:?}");
            assert!(err.to_string().contains("browser profile name"), "{err}");
        }
    }

    /// A stand-in for Chrome (tests/fixtures/fake-chrome.sh) that writes
    /// `DevToolsActivePort` into the profile and sleeps, or exits early when
    /// its start page asks it to, or plays along with a [`FakeDebugPort`].
    /// Exercises launch, the port read and the teardown without a browser.
    #[cfg(unix)]
    const FAKE_CHROME: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-chrome.sh");

    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// A data dir of the test's own, standing in for the per-user one.
    #[cfg(unix)]
    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pk-cli-browser-{tag}-{}", unpredictable()));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[cfg(unix)]
    fn fake_launch(bin: &str, root: &Path) -> Launch {
        Launch::new(bin)
            .url("https://app.example.com/login")
            .chrome(FAKE_CHROME)
            .ctrl_c(false)
            .quiet(true)
            .profile_root(root)
    }

    #[cfg(unix)]
    fn launch_args(profile: &Path) -> Vec<String> {
        std::fs::read_to_string(profile.join("args"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_launched_browser_is_stopped_and_its_profile_removed_on_drop() {
        let root = temp_root("fresh");
        let mut browser = fake_launch("pk-cli-browser-test", &root)
            .fresh_profile(true)
            .spawn()
            .unwrap();
        assert_eq!(browser.port, 9222);
        assert_eq!(browser.ws_path, "/devtools/browser/fake-browser-id");
        let profile = browser.profile.path().to_path_buf();
        assert!(
            profile.starts_with(std::env::temp_dir()),
            "a scratch profile"
        );
        assert_eq!(
            launch_args(&profile),
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
        // A fresh profile never touches the kept one.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(&root).unwrap();
    }

    /// The default: Chrome runs in the kept profile, which outlives it, one
    /// sign-in at a time, and the previous run's debug port is never read.
    #[cfg(unix)]
    #[test]
    fn the_kept_profile_outlives_the_browser_one_sign_in_at_a_time() {
        let root = temp_root("kept");
        let kept = crate::profile::kept_profile_path(&root, "pk-cli-browser-kept", None);
        assert!(!kept.exists());

        let mut browser = fake_launch("pk-cli-browser-kept", &root).spawn().unwrap();
        let pid = browser.child.id();
        assert_eq!(browser.profile.path(), kept);
        assert_eq!(
            launch_args(&kept)[1],
            format!("--user-data-dir={}", kept.display())
        );
        assert!(!browser.exited());
        // What Chrome keeps (its cookie store) is written while it runs.
        std::fs::create_dir(kept.join("Default")).unwrap();
        std::fs::write(kept.join("Default").join("Cookies"), "x").unwrap();

        // A second sign-in is refused while this one holds the profile, and
        // launches nothing.
        let err = fake_launch("pk-cli-browser-kept", &root)
            .spawn()
            .unwrap_err();
        assert!(matches!(err, BrowserError::ProfileInUse(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("another pk-cli-browser-kept browser sign-in"),
            "{msg}"
        );
        assert!(msg.contains("--fresh-profile"), "{msg}");
        assert!(!browser.exited(), "the first window is untouched");

        drop(browser);
        assert!(!alive(pid), "Chrome is stopped");
        assert!(
            kept.join("Default").join("Cookies").exists(),
            "the profile stays"
        );

        // The next sign-in reuses it. A debug-port file left by the last
        // Chrome is removed before launch, so the new port is the one read.
        std::fs::write(
            kept.join("DevToolsActivePort"),
            "1111\n/devtools/browser/stale\n",
        )
        .unwrap();
        let browser = fake_launch("pk-cli-browser-kept", &root).spawn().unwrap();
        assert_eq!(browser.port, 9222);
        assert_eq!(browser.ws_path, "/devtools/browser/fake-browser-id");
        assert!(kept.join("Default").join("Cookies").exists());
        drop(browser);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn named_profiles_are_kept_apart() {
        let root = temp_root("named");
        let a = fake_launch("pk-cli-browser-named", &root)
            .profile_name("member-a")
            .spawn()
            .unwrap();
        // Another account's sign-in runs alongside, in its own profile.
        let b = fake_launch("pk-cli-browser-named", &root)
            .profile_name("member_b")
            .spawn()
            .unwrap();
        // Names that differ only in case are two accounts, even where the
        // filesystem folds case (APFS): both sign-ins run side by side.
        let upper = fake_launch("pk-cli-browser-named", &root)
            .profile_name("Member-A")
            .spawn()
            .unwrap();
        let dir = root.join("piekstra.pk-cli-browser-named");
        assert_eq!(a.profile.path(), dir.join("browser-profile-member-a"));
        assert_eq!(b.profile.path(), dir.join("browser-profile-member_b"));
        assert_eq!(upper.profile.path(), dir.join("browser-profile-+member-+a"));
        drop((a, b, upper));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_browser_that_dies_on_launch_is_reported_and_cleaned_up() {
        let root = temp_root("early");
        let before = profiles_named("pk-cli-browser-early");
        let err = fake_launch("pk-cli-browser-early", &root)
            .url("https://exit-early.example/")
            .fresh_profile(true)
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("exited before"), "{err}");
        assert_eq!(profiles_named("pk-cli-browser-early"), before);

        let err = fake_launch("pk-cli-browser-early", &root)
            .chrome("/nonexistent/chrome")
            .fresh_profile(true)
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("launching"), "{err}");
        assert_eq!(profiles_named("pk-cli-browser-early"), before);

        // In the kept profile the failure leaves the profile and frees it
        // for the next sign-in.
        let err = fake_launch("pk-cli-browser-early", &root)
            .url("https://exit-early.example/")
            .spawn()
            .unwrap_err();
        assert!(err.to_string().contains("exited before"), "{err}");
        let kept = crate::profile::kept_profile_path(&root, "pk-cli-browser-early", None);
        assert!(kept.is_dir());
        drop(fake_launch("pk-cli-browser-early", &root).spawn().unwrap());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// What the stand-in debug port does when asked for `Browser.close`.
    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum OnClose {
        AnswerAndExit,
        /// Closes the socket unanswered, and Chrome exits.
        HangUpAndExit,
        /// Chrome answers and stays up.
        AnswerAndStay,
        NeverAnswer,
        Refuse,
    }

    /// A loopback websocket standing in for Chrome's debug port, for a fake
    /// Chrome started with [`close_url`] in the kept profile `profile`.
    #[cfg(unix)]
    struct FakeDebugPort {
        port: u16,
        connections: Arc<AtomicUsize>,
        methods: Arc<Mutex<Vec<String>>>,
    }

    #[cfg(unix)]
    impl FakeDebugPort {
        fn start(profile: PathBuf, on_close: OnClose) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let connections = Arc::default();
            let methods = Arc::default();
            let debug_port = FakeDebugPort {
                port,
                connections: Arc::clone(&connections),
                methods: Arc::clone(&methods),
            };
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    connections.fetch_add(1, Ordering::SeqCst);
                    let mut ws = tungstenite::accept(stream.unwrap()).unwrap();
                    let Ok(tungstenite::Message::Text(text)) = ws.read() else {
                        continue;
                    };
                    let request: Value = serde_json::from_str(text.as_str()).unwrap();
                    methods
                        .lock()
                        .unwrap()
                        .push(request["method"].as_str().unwrap().to_owned());
                    std::fs::write(profile.join("close-seen"), "").unwrap();
                    let reply = match on_close {
                        OnClose::AnswerAndExit | OnClose::AnswerAndStay => {
                            serde_json::json!({"id": request["id"], "result": {}})
                        }
                        OnClose::Refuse => serde_json::json!({
                            "id": request["id"],
                            "error": {"code": -32601, "message": "not here"},
                        }),
                        OnClose::HangUpAndExit => {
                            drop(ws);
                            std::fs::write(profile.join("close-now"), "").unwrap();
                            continue;
                        }
                        OnClose::NeverAnswer => {
                            while ws.read().is_ok() {}
                            continue;
                        }
                    };
                    ws.send(tungstenite::Message::text(reply.to_string()))
                        .unwrap();
                    if matches!(on_close, OnClose::AnswerAndExit) {
                        std::fs::write(profile.join("close-now"), "").unwrap();
                    }
                    while ws.read().is_ok() {}
                }
            });
            debug_port
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        fn methods(&self) -> Vec<String> {
            self.methods.lock().unwrap().clone()
        }
    }

    /// A start page that makes the fake Chrome advertise `port`, wait for
    /// `close-now` in its profile and leave a marker saying how it was stopped.
    #[cfg(unix)]
    fn close_url(port: u16) -> String {
        format!("https://close-port-{port}.example/")
    }

    #[cfg(unix)]
    fn closed_over_the_port(profile: &Path) -> bool {
        profile.join("closed-over-port").exists()
    }

    #[cfg(unix)]
    fn stopped_by_signal(profile: &Path) -> bool {
        profile.join("stopped-by-signal").exists()
    }

    #[cfg(unix)]
    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let bound = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < bound, "waited too long for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The default path: Chrome quits when asked, nothing signals it, and the
    /// kept profile is free for the next sign-in.
    #[cfg(unix)]
    #[test]
    fn chrome_in_a_kept_profile_is_closed_over_its_debug_port() {
        let root = temp_root("close");
        let bin = "pk-cli-browser-close";
        let kept = crate::profile::kept_profile_path(&root, bin, None);
        let debug = FakeDebugPort::start(kept.clone(), OnClose::AnswerAndExit);
        let browser = fake_launch(bin, &root)
            .url(close_url(debug.port))
            .spawn()
            .unwrap();
        assert_eq!(browser.port, debug.port);
        let pid = browser.child.id();

        drop(browser);
        assert_eq!(debug.methods(), ["Browser.close"]);
        assert!(closed_over_the_port(&kept));
        assert!(!stopped_by_signal(&kept));
        assert!(!alive(pid), "Chrome is stopped");
        drop(fake_launch(bin, &root).spawn().unwrap());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Chrome can go before its answer does: the socket closing is not a
    /// failure while Chrome is on its way out.
    #[cfg(unix)]
    #[test]
    fn a_socket_closed_before_the_answer_still_waits_for_the_exit() {
        let root = temp_root("close-hangup");
        let bin = "pk-cli-browser-close-hangup";
        let kept = crate::profile::kept_profile_path(&root, bin, None);
        let debug = FakeDebugPort::start(kept.clone(), OnClose::HangUpAndExit);
        let browser = fake_launch(bin, &root)
            .url(close_url(debug.port))
            .spawn()
            .unwrap();

        drop(browser);
        assert_eq!(debug.methods(), ["Browser.close"]);
        assert!(closed_over_the_port(&kept));
        assert!(!stopped_by_signal(&kept));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Nothing on the port: the signals take over at once, without the wait.
    #[cfg(unix)]
    #[test]
    fn a_dead_debug_port_falls_back_to_the_signals_at_once() {
        let root = temp_root("close-dead");
        let bin = "pk-cli-browser-close-dead";
        let kept = crate::profile::kept_profile_path(&root, bin, None);
        let closed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut browser = fake_launch(bin, &root)
            .url(close_url(closed_port))
            .spawn()
            .unwrap();
        browser.close_grace = Duration::from_secs(30);
        let pid = browser.child.id();

        let started = Instant::now();
        drop(browser);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(stopped_by_signal(&kept));
        assert!(!closed_over_the_port(&kept));
        assert!(!alive(pid), "Chrome is stopped");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A port that takes the connection and never speaks costs the bound on
    /// the handshake, then the signals take over.
    #[cfg(unix)]
    #[test]
    fn a_port_that_never_finishes_the_handshake_falls_back_to_the_signals() {
        let root = temp_root("close-mute");
        let bin = "pk-cli-browser-close-mute";
        let kept = crate::profile::kept_profile_path(&root, bin, None);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut browser = fake_launch(bin, &root)
            .url(close_url(port))
            .spawn()
            .unwrap();
        browser.close_grace = Duration::from_secs(30);

        let started = Instant::now();
        drop(browser);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(stopped_by_signal(&kept));
        drop(listener);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Chrome answers, or nothing does, and it does not exit: the wait ends at
    /// the bound, and the signals stop it.
    #[cfg(unix)]
    #[test]
    fn a_chrome_that_does_not_quit_falls_back_to_the_signals_at_the_bound() {
        for (tag, on_close) in [
            ("stay", OnClose::AnswerAndStay),
            ("silent", OnClose::NeverAnswer),
        ] {
            let root = temp_root(&format!("close-{tag}"));
            let bin = format!("pk-cli-browser-close-{tag}");
            let kept = crate::profile::kept_profile_path(&root, &bin, None);
            let debug = FakeDebugPort::start(kept.clone(), on_close);
            let mut browser = fake_launch(&bin, &root)
                .url(close_url(debug.port))
                .spawn()
                .unwrap();
            browser.close_grace = Duration::from_millis(400);
            let pid = browser.child.id();

            let started = Instant::now();
            drop(browser);
            let took = started.elapsed();
            assert!(took >= Duration::from_millis(400), "{tag}: {took:?}");
            assert!(took < CLOSE_GRACE + STOP_GRACE, "{tag}: {took:?}");
            assert_eq!(debug.methods(), ["Browser.close"], "{tag}");
            assert!(stopped_by_signal(&kept), "{tag}");
            assert!(!closed_over_the_port(&kept), "{tag}");
            assert!(!alive(pid), "{tag}: Chrome is stopped");
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    /// A refusal is an answer: there is no point waiting out the bound.
    #[cfg(unix)]
    #[test]
    fn a_refused_close_falls_back_to_the_signals_at_once() {
        let root = temp_root("close-refused");
        let bin = "pk-cli-browser-close-refused";
        let kept = crate::profile::kept_profile_path(&root, bin, None);
        let debug = FakeDebugPort::start(kept.clone(), OnClose::Refuse);
        let mut browser = fake_launch(bin, &root)
            .url(close_url(debug.port))
            .spawn()
            .unwrap();
        browser.close_grace = Duration::from_secs(30);

        let started = Instant::now();
        drop(browser);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(debug.methods(), ["Browser.close"]);
        assert!(stopped_by_signal(&kept));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A scratch profile's cookies go with it, so nothing connects, and a
    /// Chrome that has already exited has nothing left to ask.
    #[cfg(unix)]
    #[test]
    fn only_a_running_chrome_in_a_kept_profile_is_asked_to_close() {
        let root = temp_root("close-not");
        let bin = "pk-cli-browser-close-not";
        let kept = crate::profile::kept_profile_path(&root, bin, None);
        let debug = FakeDebugPort::start(kept.clone(), OnClose::AnswerAndExit);

        let scratch = fake_launch(bin, &root)
            .url(close_url(debug.port))
            .fresh_profile(true)
            .spawn()
            .unwrap();
        let pid = scratch.child.id();
        drop(scratch);
        assert!(!alive(pid), "Chrome is stopped");
        assert_eq!(debug.connections(), 0, "scratch");

        let mut gone = fake_launch(bin, &root)
            .url(close_url(debug.port))
            .spawn()
            .unwrap();
        std::fs::write(kept.join("close-now"), "").unwrap();
        wait_for("the fake Chrome to exit", || gone.exited());
        drop(gone);
        assert_eq!(debug.connections(), 0, "already exited");

        // A launch that ended before the port was read has no port to ask.
        std::fs::remove_file(kept.join("close-now")).unwrap();
        let mut unread = fake_launch(bin, &root)
            .url(close_url(debug.port))
            .spawn()
            .unwrap();
        unread.port = 0;
        drop(unread);
        assert_eq!(debug.connections(), 0, "port not read");
        assert!(stopped_by_signal(&kept));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    const CTRL_C_CHILD_ROOT: &str = "PK_CLI_BROWSER_TEST_CTRL_C_ROOT";

    /// The child half of the Ctrl-C tests, which need a process of their own:
    /// the handler and the cancel flag are process-wide, and a second Ctrl-C
    /// exits the process. A CLI waiting on a Chrome that drops its guard on
    /// the first Ctrl-C and exits 0 once the guard has dropped.
    #[cfg(unix)]
    fn ctrl_c_child(root: &Path, on_close: OnClose) -> ! {
        let bin = "pk-cli-browser-ctrl-c";
        let kept = crate::profile::kept_profile_path(root, bin, None);
        let debug = FakeDebugPort::start(kept, on_close);
        let browser = fake_launch(bin, root)
            .url(close_url(debug.port))
            .ctrl_c(true)
            .spawn()
            .unwrap();
        std::fs::write(root.join("chrome-pid"), browser.child.id().to_string()).unwrap();
        while !interrupt::canceled() {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(browser);
        std::process::exit(0);
    }

    /// Start this test binary again, running just `test` as the child.
    #[cfg(unix)]
    fn start_ctrl_c_child(test: &str, root: &Path) -> (std::process::Child, u32) {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1"])
            .env(CTRL_C_CHILD_ROOT, root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut chrome = None;
        wait_for("the child's Chrome", || {
            chrome = std::fs::read_to_string(root.join("chrome-pid"))
                .ok()
                .and_then(|pid| pid.parse().ok());
            chrome.is_some()
        });
        (child, chrome.unwrap())
    }

    #[cfg(unix)]
    fn interrupt_process(child: &std::process::Child) {
        // SAFETY: kill(2) on a child this test spawned and has not reaped.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGINT);
        }
    }

    #[cfg(unix)]
    fn exit_code_of(child: &mut std::process::Child) -> Option<i32> {
        let mut status = None;
        wait_for("the child to exit", || {
            status = child.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap().code()
    }

    /// The close runs after a first Ctrl-C, which is what a canceled wait
    /// looks like to the guard.
    #[cfg(unix)]
    #[test]
    fn a_first_ctrl_c_still_closes_over_the_debug_port() {
        if let Ok(root) = std::env::var(CTRL_C_CHILD_ROOT) {
            ctrl_c_child(Path::new(&root), OnClose::AnswerAndExit);
        }
        let root = temp_root("ctrl-c-once");
        let kept = crate::profile::kept_profile_path(&root, "pk-cli-browser-ctrl-c", None);
        let (mut child, _chrome) = start_ctrl_c_child(
            "chrome::tests::a_first_ctrl_c_still_closes_over_the_debug_port",
            &root,
        );

        interrupt_process(&child);
        assert_eq!(exit_code_of(&mut child), Some(0));
        assert!(closed_over_the_port(&kept));
        assert!(!stopped_by_signal(&kept));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A second Ctrl-C does not wait for the close: it kills Chrome and exits
    /// 130, well inside the bound, and the kept profile stays.
    #[cfg(unix)]
    #[test]
    fn a_second_ctrl_c_during_the_close_exits_at_once() {
        if let Ok(root) = std::env::var(CTRL_C_CHILD_ROOT) {
            ctrl_c_child(Path::new(&root), OnClose::NeverAnswer);
        }
        let root = temp_root("ctrl-c-twice");
        let kept = crate::profile::kept_profile_path(&root, "pk-cli-browser-ctrl-c", None);
        let (mut child, chrome) = start_ctrl_c_child(
            "chrome::tests::a_second_ctrl_c_during_the_close_exits_at_once",
            &root,
        );

        interrupt_process(&child);
        wait_for("the close to start", || kept.join("close-seen").exists());
        let started = Instant::now();
        interrupt_process(&child);
        assert_eq!(exit_code_of(&mut child), Some(130));
        assert!(started.elapsed() < CLOSE_GRACE);
        wait_for("Chrome to be gone", || !alive(chrome));
        assert!(!stopped_by_signal(&kept), "killed, not signaled to stop");
        assert!(kept.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
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
