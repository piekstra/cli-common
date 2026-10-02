//! 1Password as a credential source, through the `op` CLI.
//!
//! A CLI declares where a secret lives in 1Password as a secret reference,
//! `op://<vault>/<item>/<field>` (or `op://<vault>/<item>/<section>/<field>`),
//! and [`OnePassword::read`] resolves it with `op read`. A one-time password
//! field resolves to the current code with `?attribute=otp`.
//!
//! # Rails
//!
//! - **The secret never touches argv, logs or disk.** The argv is
//!   `op read --no-newline [--account <ACCOUNT>] <REFERENCE>`: a reference
//!   names where the secret is, not what it is. The value comes back on
//!   `op`'s stdout, through a pipe, straight into a [`Secret`]. No
//!   `--out-file`, no temp file. Error messages carry `op`'s stderr, never
//!   its stdout.
//! - **One call, bounded, never retried.** `op` can raise a Touch ID or
//!   1Password-app approval, and with nobody at the keyboard it waits. Each
//!   read is one `op` process, killed at a timeout (60 s by default). A
//!   failed or timed-out read is reported, not repeated: a retry would raise
//!   a second approval. Nothing here polls `op` to see whether it is signed
//!   in.
//! - **`op` never reads the terminal.** Its stdin is closed, so a signed-out
//!   `op` that would ask for the account password fails instead of waiting
//!   on a prompt nobody sees.
//!
//! # Exit codes
//!
//! | `op` outcome | Error | Exit |
//! |---|---|---|
//! | not signed in, session expired, approval dismissed | [`CliError::Auth`] naming `op signin` | 3 |
//! | no answer within the timeout | [`CliError::Auth`] naming the approval and `op signin` | 3 |
//! | the vault, item or field does not exist, or the value is empty | [`CliError::NotFound`] naming the reference | 4 |
//! | any other `op` failure | [`CliError::Upstream`] with `op`'s first stderr line | 5 |
//! | `op` is not installed | [`CliError::Other`] | 1 |

use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use pk_cli_core::CliError;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::Secret;

/// A 1Password secret reference: `op://<vault>/<item>/[<section>/]<field>`,
/// optionally followed by a query such as `?attribute=otp`.
///
/// Not a secret itself, so it prints, serializes and can sit in a config
/// file. Parsing checks the shape only; whether the item exists is `op`'s
/// answer at read time.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OpRef(String);

impl OpRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for OpRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // The input is not echoed: a value that fails this check may be a
        // secret pasted where its reference belongs.
        let bad = |why: &str| {
            Err(format!(
                "not a 1Password secret reference ({why}); \
                 expected op://<vault>/<item>/<field>"
            ))
        };
        if s.chars().any(char::is_control) {
            return bad("it contains a control character");
        }
        let Some(rest) = s.strip_prefix("op://") else {
            return bad("it does not start with op://");
        };
        let (path, query) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        let segments: Vec<&str> = path.split('/').collect();
        if !(3..=4).contains(&segments.len()) {
            return bad("it needs a vault, an item and a field, with an optional section");
        }
        if segments.iter().any(|seg| seg.trim().is_empty()) {
            return bad("a path segment is empty");
        }
        if query.is_some_and(|q| q.is_empty() || !q.contains('=')) {
            return bad("the query after `?` must be key=value");
        }
        Ok(OpRef(s.to_string()))
    }
}

impl TryFrom<String> for OpRef {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<OpRef> for String {
    fn from(r: OpRef) -> String {
        r.0
    }
}

impl fmt::Display for OpRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `--op <REFERENCE>` flag, flattenable beside [`crate::SecretSourceArgs`]
/// on `auth login` / `auth set-credential`. Read both with
/// [`crate::SecretSourceArgs::read_with_op`].
#[derive(clap::Args, Debug, Default, Clone)]
pub struct OpArgs {
    /// Read the secret from 1Password with `op read` (a secret reference,
    /// e.g. op://Example/Login/password). Not the secret itself.
    #[arg(long = "op", value_name = "op://VAULT/ITEM/FIELD", value_parser = OpRefParser)]
    pub op: Option<OpRef>,
}

/// Parses `--op` without echoing a rejected value. clap's own message for a
/// failed `FromStr` quotes the input, and a value that is not a reference
/// may be the secret itself.
#[derive(Clone, Copy, Debug)]
struct OpRefParser;

impl clap::builder::TypedValueParser for OpRefParser {
    type Value = OpRef;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<OpRef, clap::Error> {
        let flag = arg
            .and_then(|a| a.get_long())
            .map(|l| format!("--{l}"))
            .unwrap_or_else(|| "--op".into());
        let why = match value.to_str() {
            Some(v) => match v.parse::<OpRef>() {
                Ok(r) => return Ok(r),
                Err(e) => e,
            },
            None => "not a 1Password secret reference (not UTF-8)".to_string(),
        };
        Err(clap::Error::raw(
            clap::error::ErrorKind::ValueValidation,
            format!("{flag}: {why} (the value given is not shown)\n"),
        ))
    }
}

/// The 1Password CLI, `op`, as a secret reader.
#[derive(Clone, Debug)]
pub struct OnePassword {
    program: String,
    account: Option<String>,
    timeout: Duration,
}

impl Default for OnePassword {
    fn default() -> Self {
        OnePassword::new()
    }
}

impl OnePassword {
    /// `op` from `PATH`, its default account, a 60-second limit per read.
    /// The limit leaves time to answer a Touch ID or app approval.
    pub fn new() -> Self {
        OnePassword {
            program: "op".into(),
            account: None,
            timeout: Duration::from_secs(60),
        }
    }

    /// Run a different binary (a full path, or a renamed install).
    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Read from a specific 1Password account (`op --account`: a sign-in
    /// address, an account ID or a user ID) instead of `op`'s default.
    pub fn account(mut self, account: impl Into<String>) -> Result<Self, CliError> {
        let a = account.into();
        if a.trim().is_empty() || a.starts_with('-') || a.chars().any(char::is_control) {
            return Err(CliError::Usage(format!("`{a}` is not a 1Password account")));
        }
        self.account = Some(a);
        Ok(self)
    }

    /// The limit on each `op` call. A call past it is killed and reported
    /// as exit 3; it is not retried.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The argv for one read: no secret, only where it lives.
    fn args(&self, reference: &OpRef) -> Vec<String> {
        let mut args = vec!["read".to_string(), "--no-newline".to_string()];
        if let Some(a) = &self.account {
            args.push("--account".into());
            args.push(a.clone());
        }
        args.push(reference.as_str().to_string());
        args
    }

    /// Resolve one reference with one `op read`. See the module docs for the
    /// rails and the exit-code mapping.
    pub fn read(&self, reference: &OpRef) -> Result<Secret, CliError> {
        let out = run_bounded(&self.program, &self.args(reference), self.timeout)?;
        match out {
            Outcome::TimedOut => Err(CliError::Auth(format!(
                "`{}` did not answer within {}s — it is probably waiting on a 1Password \
                 approval (Touch ID or the app's prompt). Approve it and rerun, or run \
                 `op signin` first; the read was not retried",
                self.program,
                self.timeout.as_secs()
            ))),
            Outcome::Exited {
                success: true,
                stdout,
                ..
            } => {
                let mut value = stdout;
                // `--no-newline` should leave none; an older `op` ignores it.
                if value.expose().ends_with('\n') {
                    let trimmed = value.expose().trim_end_matches(['\n', '\r']).to_string();
                    value = Secret::new(trimmed);
                }
                if value.is_empty() {
                    return Err(CliError::NotFound(format!(
                        "1Password returned an empty value for {reference}"
                    )));
                }
                Ok(value)
            }
            Outcome::Exited { stderr, .. } => Err(classify(&self.program, reference, &stderr)),
        }
    }
}

/// What one bounded `op` run produced.
enum Outcome {
    Exited {
        success: bool,
        stdout: Secret,
        stderr: String,
    },
    TimedOut,
}

fn run_bounded(program: &str, args: &[String], timeout: Duration) -> Result<Outcome, CliError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CliError::Other(format!(
                "`{program}` (the 1Password CLI) is not installed or not on PATH"
            )),
            _ => CliError::Other(format!("starting `{program}`: {e}")),
        })?;
    // Drain both pipes on threads so a chatty child can't block on a full
    // pipe while we wait for it to exit.
    let mut out = child.stdout.take().expect("stdout is piped");
    let mut err = child.stderr.take().expect("stderr is piped");
    // Zeroizing: on a timeout the reader is never joined, and whatever it
    // read is wiped when the thread's result is dropped.
    let out_reader = std::thread::spawn(move || {
        let mut buf = Zeroizing::new(Vec::new());
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Not joined: a grandchild `op` left behind can hold the
                // pipes open, and joining would wait on it past the limit.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Outcome::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(CliError::Other(format!("waiting on `{program}`: {e}"))),
        }
    };
    let mut stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    let secret = match String::from_utf8(std::mem::take(&mut *stdout)) {
        Ok(s) => Secret::new(s),
        Err(e) => {
            e.into_bytes().zeroize();
            return Err(CliError::Upstream(format!(
                "`{program}` returned a value that is not UTF-8"
            )));
        }
    };
    Ok(Outcome::Exited {
        success: status.success(),
        stdout: secret,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// Map a failed `op read` to the family exit codes from its stderr.
fn classify(program: &str, reference: &OpRef, stderr: &str) -> CliError {
    let said = first_line(stderr);
    let lower = stderr.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&["authorization prompt dismissed", "authorization was denied"]) {
        return CliError::Auth(format!(
            "the 1Password approval was dismissed ({said}); rerun and approve it, \
             or run `op signin` first"
        ));
    }
    if has(&[
        "not currently signed in",
        "not signed in",
        "sign in to create a new session",
        "session expired",
        "no accounts configured",
        "authorization timeout",
        "op signin",
    ]) {
        return CliError::Auth(format!(
            "1Password CLI is not signed in ({said}); run `op signin` \
             (or unlock the 1Password app with CLI integration on), then retry"
        ));
    }
    if has(&[
        "isn't an item",
        "isn't a vault",
        "isn't a field",
        "does not have a field",
        "could not find",
        "no item found",
    ]) {
        return CliError::NotFound(format!(
            "1Password has nothing at {reference} ({said}); check the vault, item and \
             field names"
        ));
    }
    CliError::Upstream(if said.is_empty() {
        format!("`{program} read` failed")
    } else {
        format!("`{program} read` failed: {said}")
    })
}

/// `op`'s first non-empty stderr line, without its `[ERROR] <date> <time>`
/// prefix, capped at 200 characters.
fn first_line(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = match line.strip_prefix("[ERROR]") {
        Some(rest) => {
            let rest = rest.trim_start();
            // `2026/10/02 09:00:00 message` → `message`
            let mut parts = rest.splitn(3, ' ');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(d), Some(t), Some(msg))
                    if d.contains('/') && t.contains(':') && !msg.is_empty() =>
                {
                    msg
                }
                _ => rest,
            }
        }
        None => line,
    };
    line.chars().take(200).collect()
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fake `op`: a shell script written per test into its own directory,
    //! which logs its argv and answers from a fixture. Tests never call a
    //! real `op`.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    pub struct FakeOp {
        pub dir: PathBuf,
    }

    impl FakeOp {
        /// `body` is the script after the argv logging line.
        pub fn new(body: &str) -> FakeOp {
            let dir = std::env::temp_dir().join(format!(
                "pk-cli-secrets-fake-op-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\necho call >> \"{}\"\n{body}\n",
                dir.join("argv").display(),
                dir.join("calls").display()
            );
            let path = dir.join("op");
            std::fs::write(&path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            FakeOp { dir }
        }

        /// Prints `value` on stdout and succeeds.
        pub fn answering(value: &str) -> FakeOp {
            FakeOp::new(&format!("printf '%s' '{value}'"))
        }

        /// Prints a fixture from `tests/fixtures/op/` on stderr and fails.
        pub fn failing_with(fixture: &str) -> FakeOp {
            FakeOp::new(&format!(
                "cat \"{}\" >&2\nexit 1",
                fixture_path(fixture).display()
            ))
        }

        pub fn program(&self) -> String {
            self.dir.join("op").display().to_string()
        }

        pub fn argv(&self) -> Vec<String> {
            read_lines(&self.dir.join("argv"))
        }

        pub fn calls(&self) -> usize {
            read_lines(&self.dir.join("calls")).len()
        }

        /// Every file in the fake's directory: the script and its own logs.
        pub fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for FakeOp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/op")
            .join(name)
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::fake::FakeOp;
    use super::*;

    const SECRET: &str = "example-secret-value";

    fn reference() -> OpRef {
        "op://Example/Login/password".parse().unwrap()
    }

    fn op(fake: &FakeOp) -> OnePassword {
        OnePassword::new()
            .program(fake.program())
            .timeout(Duration::from_secs(10))
    }

    #[test]
    fn a_read_returns_the_value_with_only_the_reference_on_argv() {
        let fake = FakeOp::answering(SECRET);
        let secret = op(&fake).read(&reference()).unwrap();
        assert_eq!(secret.expose(), SECRET);
        assert_eq!(
            fake.argv(),
            ["read", "--no-newline", "op://Example/Login/password"]
        );
        assert!(fake.argv().iter().all(|a| !a.contains(SECRET)));
        assert_eq!(fake.calls(), 1);
    }

    #[test]
    fn nothing_is_written_to_disk_by_a_read() {
        let fake = FakeOp::answering(SECRET);
        op(&fake).read(&reference()).unwrap();
        // Only the fake's own script and logs; no output file, no temp file.
        assert_eq!(fake.files(), ["argv", "calls", "op"]);
        for f in ["argv", "calls"] {
            let body = std::fs::read_to_string(fake.dir.join(f)).unwrap();
            assert!(!body.contains(SECRET));
        }
    }

    #[test]
    fn the_account_goes_before_the_reference() {
        let fake = FakeOp::answering(SECRET);
        op(&fake)
            .account("example.1password.com")
            .unwrap()
            .read(&reference())
            .unwrap();
        assert_eq!(
            fake.argv(),
            [
                "read",
                "--no-newline",
                "--account",
                "example.1password.com",
                "op://Example/Login/password"
            ]
        );
        for bad in ["", "--format", "a\nb"] {
            assert!(OnePassword::new().account(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_trailing_newline_from_an_older_op_is_dropped() {
        let fake = FakeOp::new(&format!("printf '%s\\n' '{SECRET}'"));
        assert_eq!(op(&fake).read(&reference()).unwrap().expose(), SECRET);
    }

    #[test]
    fn not_signed_in_is_exit_3_naming_op_signin() {
        for fixture in [
            "not-signed-in.txt",
            "session-expired.txt",
            "no-accounts.txt",
            "prompt-dismissed.txt",
        ] {
            let fake = FakeOp::failing_with(fixture);
            let err = op(&fake).read(&reference()).unwrap_err();
            assert_eq!(err.exit_code(), 3, "{fixture}: {err}");
            assert!(err.to_string().contains("`op signin`"), "{fixture}: {err}");
            assert!(!err.to_string().contains("[ERROR]"), "{fixture}: {err}");
            assert_eq!(fake.calls(), 1, "{fixture}: never retried");
        }
    }

    #[test]
    fn a_missing_vault_item_or_field_is_exit_4_naming_the_reference() {
        for fixture in ["no-item.txt", "no-vault.txt", "no-field.txt"] {
            let fake = FakeOp::failing_with(fixture);
            let err = op(&fake).read(&reference()).unwrap_err();
            assert_eq!(err.exit_code(), 4, "{fixture}: {err}");
            assert!(
                err.to_string().contains("op://Example/Login/password"),
                "{err}"
            );
        }
    }

    #[test]
    fn an_empty_value_is_exit_4() {
        let fake = FakeOp::answering("");
        assert_eq!(op(&fake).read(&reference()).unwrap_err().exit_code(), 4);
    }

    #[test]
    fn any_other_failure_is_exit_5_with_ops_first_line_and_never_its_stdout() {
        let fake = FakeOp::new(&format!(
            "printf '%s' '{SECRET}'\ncat \"{}\" >&2\nexit 1",
            super::fake::fixture_path("other-error.txt").display()
        ));
        let err = op(&fake).read(&reference()).unwrap_err();
        assert_eq!(err.exit_code(), 5);
        let msg = err.to_string();
        assert!(msg.contains("connecting to desktop app"), "{msg}");
        assert!(!msg.contains("second line"), "{msg}");
        assert!(!msg.contains(SECRET), "{msg}");
    }

    #[test]
    fn a_hung_op_is_killed_at_the_timeout_once_and_is_exit_3() {
        let fake = FakeOp::new("sleep 10");
        let started = Instant::now();
        let err = op(&fake)
            .timeout(Duration::from_millis(1500))
            .read(&reference())
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_eq!(err.exit_code(), 3);
        let msg = err.to_string();
        assert!(
            msg.contains("approval") && msg.contains("`op signin`"),
            "{msg}"
        );
        // At most one: a slow first exec may be killed before it logs.
        assert!(fake.calls() <= 1);
    }

    #[test]
    fn a_missing_op_is_exit_1_saying_so() {
        let err = OnePassword::new()
            .program("pk-cli-secrets-no-such-op")
            .read(&reference())
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    #[test]
    fn first_line_strips_the_error_prefix() {
        assert_eq!(
            first_line("\n[ERROR] 2026/10/02 09:00:00 you are not signed in\nmore"),
            "you are not signed in"
        );
        assert_eq!(first_line("plain message"), "plain message");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line(&"x".repeat(500)).len(), 200);
    }
}

#[cfg(test)]
mod ref_tests {
    use super::*;

    #[test]
    fn references_parse_by_shape() {
        for ok in [
            "op://Example/Login/password",
            "op://Example/Login/Section/password",
            "op://Example Vault/Bank Login/one-time password?attribute=otp",
        ] {
            let r: OpRef = ok.parse().unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert_eq!(r.to_string(), ok);
        }
        for bad in [
            "",
            "Example/Login/password",
            "op://Example/Login",
            "op://Example//password",
            "op://a/b/c/d/e",
            "op://Example/Login/password?",
            "op://Example/Login/password?otp",
            "op://Example/Login/pass\nword",
        ] {
            assert!(bad.parse::<OpRef>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_reference_round_trips_through_config_as_a_string() {
        #[derive(Serialize, Deserialize)]
        struct Cfg {
            password: OpRef,
        }
        let cfg: Cfg =
            serde_json::from_str(r#"{"password":"op://Example/Login/password"}"#).unwrap();
        assert_eq!(cfg.password.as_str(), "op://Example/Login/password");
        assert_eq!(
            serde_json::to_string(&cfg).unwrap(),
            r#"{"password":"op://Example/Login/password"}"#
        );
        assert!(serde_json::from_str::<Cfg>(r#"{"password":"not-a-ref"}"#).is_err());
    }

    #[test]
    fn the_flag_parses_a_reference() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            op: OpArgs,
        }
        let cli = Cli::try_parse_from(["x", "--op", "op://Example/Login/password"]).unwrap();
        assert_eq!(cli.op.op.unwrap().as_str(), "op://Example/Login/password");
        let err = Cli::try_parse_from(["x", "--op", "hunter2"])
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("--op"), "{err}");
        assert!(
            !err.contains("hunter2"),
            "a rejected value is never echoed: {err}"
        );
    }
}
