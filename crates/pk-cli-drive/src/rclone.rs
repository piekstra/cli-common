//! The rclone backend: `cat`/`copyto`/`moveto`/`lsf`/`mkdir` against a
//! remote spec (`<remote>:<path>`). Slower per call than a mount, but it
//! talks to the storage provider's API directly and is immune to mount
//! health.

use pk_cli_core::CliError;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::backend::{occupied, FileEntry};
use crate::private_file::{self, Replace};

/// The reads the cache needs from its source of truth. A trait so the
/// cache's serve/revalidate decisions are testable against an in-memory
/// remote that counts its calls, without spawning rclone.
pub trait RemoteRead {
    /// The remote spec (`remote:path`) with `rel` appended — display only.
    fn target(&self, rel: &str) -> String;
    fn cat(&self, rel: &str) -> Result<Option<String>, CliError>;
    fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError>;
}

/// The temp-name prefix a [`Remote`] uses unless told otherwise.
pub const DEFAULT_TEMP_PREFIX: &str = "pk-drive";

/// An rclone remote spec plus how to call rclone against it.
pub struct Remote {
    spec: String,
    /// When set, every rclone call is killed once this instant passes and
    /// fails as upstream. The background revalidator sets it so its lifetime
    /// is bounded; interactive reads leave it unset.
    deadline: Option<Instant>,
    program: String,
    pub(crate) temp_prefix: String,
}

impl RemoteRead for Remote {
    fn target(&self, rel: &str) -> String {
        Remote::target(self, rel)
    }
    fn cat(&self, rel: &str) -> Result<Option<String>, CliError> {
        Remote::cat(self, rel)
    }
    fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        Remote::list_entries(self, rel)
    }
}

impl Remote {
    /// A remote at `spec` (`<remote>:<path>`; a trailing `/` is dropped).
    pub fn new(spec: &str) -> Self {
        Self {
            spec: spec.trim_end_matches('/').to_string(),
            deadline: None,
            program: "rclone".into(),
            temp_prefix: DEFAULT_TEMP_PREFIX.into(),
        }
    }

    /// This remote with every rclone call bounded to `budget` from now.
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.deadline = Some(Instant::now() + budget);
        self
    }

    /// Run `program` instead of `rclone` from `PATH` (an absolute path, or a
    /// stub in tests).
    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Name this CLI's temp files `<prefix>-remote-…` (an upload's staging
    /// file) and `<prefix>-doc-…` (a download's private dir). Usually the
    /// binary name. Default [`DEFAULT_TEMP_PREFIX`].
    pub fn temp_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.temp_prefix = prefix.into();
        self
    }

    /// The spec, trailing `/` removed — what [`crate::cache::cache_dir_for`]
    /// keys on.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    pub fn target(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.spec.clone()
        } else {
            format!("{}/{}", self.spec, rel)
        }
    }

    fn run(&self, args: &[&str]) -> Result<Output, CliError> {
        match self.deadline {
            Some(deadline) => run_until(&self.program, args, deadline),
            None => run(&self.program, args),
        }
    }

    pub fn cat(&self, rel: &str) -> Result<Option<String>, CliError> {
        let out = self.run(&["cat", &self.target(rel)])?;
        if out.status.success() {
            Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
        } else if missing(&out) {
            Ok(None)
        } else {
            Err(upstream("cat", &self.target(rel), &out))
        }
    }

    /// Write a small UTF-8 file: staged in an owner-only temp file, then
    /// `copyto`. The temp file is created exclusively, so a predictable name
    /// in a shared temp dir never follows a planted symlink and is never
    /// readable by others; it is removed whatever the upload's outcome.
    pub fn write(&self, rel: &str, content: &str) -> Result<(), CliError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!(
            "{}-remote-{}-{nanos}.tmp",
            self.temp_prefix,
            std::process::id()
        ));
        private_file::write(&tmp, content.as_bytes(), Replace::Never)
            .map_err(|e| CliError::Other(format!("writing temp file: {e}")))?;
        let res = self.copy_in(&tmp, rel);
        let _ = fs::remove_file(&tmp);
        res
    }

    /// Upload a local file as-is (`rclone copyto`) — binary-safe, and rclone
    /// creates the destination folders.
    pub fn copy_in(&self, local: &Path, rel: &str) -> Result<(), CliError> {
        let out = self.run(&["copyto", &local.display().to_string(), &self.target(rel)])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(upstream("copyto", &self.target(rel), &out))
        }
    }

    /// Download a file to a local path (`rclone copyto`) — binary-safe.
    pub fn copy_out(&self, rel: &str, local: &Path) -> Result<(), CliError> {
        let target = self.target(rel);
        let out = self.run(&["copyto", &target, &local.display().to_string()])?;
        if out.status.success() {
            Ok(())
        } else if missing(&out) {
            Err(CliError::NotFound(format!("{target}: no such file")))
        } else {
            Err(upstream("copyto", &target, &out))
        }
    }

    /// Move within the remote (`rclone moveto`) — server-side, no download.
    pub fn move_to(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        let (from, to) = (self.target(rel_from), self.target(rel_to));
        let out = self.run(&["moveto", &from, &to])?;
        if out.status.success() {
            Ok(())
        } else if missing(&out) {
            Err(CliError::NotFound(format!("{from}: nothing to move")))
        } else {
            Err(upstream("moveto", &to, &out))
        }
    }

    /// `rclone moveto --ignore-existing`: rclone skips an occupied
    /// destination and leaves the source in place, so a source still there
    /// afterwards means the destination was taken — refused, nothing moved.
    /// Costs one extra `lsf` round trip per move.
    pub fn move_no_clobber(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        let (from, to) = (self.target(rel_from), self.target(rel_to));
        let out = self.run(&["moveto", "--ignore-existing", &from, &to])?;
        if !out.status.success() {
            return Err(if missing(&out) {
                CliError::NotFound(format!("{from}: nothing to move"))
            } else {
                upstream("moveto", &to, &out)
            });
        }
        if self.exists(rel_from)? {
            return Err(occupied(&to));
        }
        Ok(())
    }

    pub fn exists(&self, rel: &str) -> Result<bool, CliError> {
        let out = self.run(&["lsf", &self.target(rel)])?;
        if out.status.success() {
            Ok(true)
        } else if missing(&out) {
            Ok(false)
        } else {
            Err(upstream("lsf", &self.target(rel), &out))
        }
    }

    pub fn mkdir(&self, rel: &str) -> Result<(), CliError> {
        let out = self.run(&["mkdir", &self.target(rel)])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(upstream("mkdir", &self.target(rel), &out))
        }
    }

    /// `--format=tsp --time-format=unix` yields `<mtime>;<size>;<path>` per
    /// line, so one listing carries sizes and modification times. Both lead,
    /// so splitting on the first two `;` is safe even for a filename that
    /// contains one. An rclone too old for `--time-format` (added in v1.63)
    /// rejects the flag; the listing then falls back to sizes only rather
    /// than failing every read.
    pub fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        let target = self.target(rel);
        let out = self.run(&[
            "lsf",
            "-R",
            "--files-only",
            "--format=tsp",
            "--time-format=unix",
            &target,
        ])?;
        let (out, with_time) = if !out.status.success() && !missing(&out) && unknown_flag(&out) {
            let sizes_only = ["lsf", "-R", "--files-only", "--format=sp", &target];
            (self.run(&sizes_only)?, false)
        } else {
            (out, true)
        };
        if out.status.success() {
            let mut entries: Vec<FileEntry> = lines(&out)
                .into_iter()
                .map(|line| parse_entry(&line, with_time))
                .collect();
            entries.sort_by(|a, b| a.rel.cmp(&b.rel));
            Ok(entries)
        } else if missing(&out) {
            Ok(Vec::new())
        } else {
            Err(upstream("lsf -R", &target, &out))
        }
    }

    /// The immediate subdirectories of `rel`, sorted, without trailing `/`.
    pub fn list_dirs(&self, rel: &str) -> Result<Vec<String>, CliError> {
        let out = self.run(&["lsf", "--dirs-only", &self.target(rel)])?;
        if out.status.success() {
            Ok(lines(&out)
                .into_iter()
                .map(|d| d.trim_end_matches('/').to_string())
                .collect())
        } else if missing(&out) {
            Ok(Vec::new())
        } else {
            Err(upstream("lsf --dirs-only", &self.target(rel), &out))
        }
    }
}

/// One `lsf` line: `<mtime>;<size>;<path>` (`with_time`) or `<size>;<path>`.
/// A leading field that does not parse is `None`, never an error.
fn parse_entry(line: &str, with_time: bool) -> FileEntry {
    let (modified, rest) = match line.split_once(';') {
        Some((t, rest)) if with_time => (t.trim().parse().ok(), rest),
        _ => (None, line),
    };
    match rest.split_once(';') {
        Some((size, path)) => FileEntry {
            rel: path.to_string(),
            size: size.trim().parse().ok(),
            modified,
        },
        None => FileEntry {
            rel: rest.to_string(),
            size: None,
            modified,
        },
    }
}

/// rclone rejected a flag it does not know (an older release).
fn unknown_flag(out: &Output) -> bool {
    String::from_utf8_lossy(&out.stderr).contains("unknown flag")
}

/// rclone's documented exit codes: 3 = directory not found, 4 = file not
/// found. Everything else nonzero is a real failure.
fn missing(out: &Output) -> bool {
    matches!(out.status.code(), Some(3) | Some(4))
}

fn lines(out: &Output) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    v
}

fn upstream(verb: &str, target: &str, out: &Output) -> CliError {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or("unknown error");
    CliError::Upstream(format!("rclone {verb} {target}: {first}"))
}

/// Run `rclone <args> --log-level=ERROR` to completion, unbounded.
pub fn run_rclone(args: &[&str]) -> Result<Output, CliError> {
    run("rclone", args)
}

fn run(program: &str, args: &[&str]) -> Result<Output, CliError> {
    Command::new(program)
        .args(args)
        .arg("--log-level=ERROR")
        .output()
        .map_err(spawn_error)
}

fn spawn_error(e: std::io::Error) -> CliError {
    if e.kind() == std::io::ErrorKind::NotFound {
        CliError::Usage(
            "rclone not found on PATH — install rclone, or unset the `remote` config \
             to use a local mount only"
                .into(),
        )
    } else {
        CliError::Other(format!("running rclone: {e}"))
    }
}

/// [`run`] that kills the child once `deadline` passes, so a hung call
/// cannot keep the calling process alive past its bound. On timeout the pipe
/// readers are not joined: they end when the killed child's pipes close, and
/// the caller is about to exit anyway.
fn run_until(program: &str, args: &[&str], deadline: Instant) -> Result<Output, CliError> {
    let mut child = Command::new(program)
        .args(args)
        .arg("--log-level=ERROR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CliError::Upstream(format!(
                    "rclone {} timed out",
                    args.first().copied().unwrap_or("")
                )));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CliError::Other(format!("waiting on rclone: {e}")));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// The first line of `rclone version`, for a health check.
pub fn rclone_version() -> Result<String, CliError> {
    let out = Command::new("rclone")
        .args(["version", "--log-level=ERROR"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::Usage("rclone not found on PATH".into())
            } else {
                CliError::Other(format!("running rclone: {e}"))
            }
        })?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("rclone (version unknown)")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsf_lines_parse_with_and_without_times() {
        let e = parse_entry("1767225600;42;Example Bank/a;b.pdf", true);
        assert_eq!(e.rel, "Example Bank/a;b.pdf");
        assert_eq!(e.size, Some(42));
        assert_eq!(e.modified, Some(1_767_225_600));
        let e = parse_entry("42;scan0001.pdf", false);
        assert_eq!(
            (e.rel.as_str(), e.size, e.modified),
            ("scan0001.pdf", Some(42), None)
        );
        let e = parse_entry("-;7;x.pdf", true);
        assert_eq!(
            (e.rel.as_str(), e.size, e.modified),
            ("x.pdf", Some(7), None)
        );
    }

    #[test]
    fn a_trailing_slash_is_dropped_from_the_spec() {
        let r = Remote::new("example:State/");
        assert_eq!(r.spec(), "example:State");
        assert_eq!(r.target(""), "example:State");
        assert_eq!(r.target("a/b.json"), "example:State/a/b.json");
    }

    #[cfg(unix)]
    use crate::stub::Stub;

    #[cfg(unix)]
    #[test]
    fn rclone_exit_3_and_4_read_as_missing_and_other_failures_are_upstream() {
        let gone = Stub::new("exit 3");
        let r = gone.remote();
        assert_eq!(r.cat("a.json").unwrap(), None);
        assert!(!r.exists("a.json").unwrap());
        assert_eq!(r.list_entries("2026").unwrap(), vec![]);
        assert_eq!(r.list_dirs("").unwrap(), Vec::<String>::new());
        assert_eq!(r.move_to("a", "b").unwrap_err().exit_code(), 4);
        assert_eq!(
            gone.calls()[0],
            "cat example:State/a.json --log-level=ERROR"
        );

        let down = Stub::new("echo 'Failed to cat: quota exceeded' >&2\necho second >&2\nexit 1");
        let err = down.remote().cat("a.json").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(
            err.to_string()
                .ends_with("rclone cat example:State/a.json: Failed to cat: quota exceeded"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_listing_carries_times_and_falls_back_on_an_old_rclone() {
        let new = Stub::new("printf '1767225600;3;b.pdf\\n1767225601;2;a/c.pdf\\n'");
        let entries = new.remote().list_entries("2026").unwrap();
        assert_eq!(
            entries,
            vec![
                FileEntry {
                    rel: "a/c.pdf".into(),
                    size: Some(2),
                    modified: Some(1_767_225_601)
                },
                FileEntry {
                    rel: "b.pdf".into(),
                    size: Some(3),
                    modified: Some(1_767_225_600)
                },
            ]
        );

        let old = Stub::new(
            "case \"$*\" in *time-format*) echo 'Error: unknown flag: --time-format' >&2; exit 1;; esac\nprintf '3;b.pdf\\n'",
        );
        let entries = old.remote().list_entries("2026").unwrap();
        assert_eq!(
            entries,
            vec![FileEntry {
                rel: "b.pdf".into(),
                size: Some(3),
                modified: None
            }]
        );
        assert_eq!(old.calls().len(), 2);
        assert!(old.calls()[1].starts_with("lsf -R --files-only --format=sp example:State/2026"));
    }

    #[cfg(unix)]
    #[test]
    fn a_write_stages_an_owner_only_temp_file_and_removes_it() {
        // The stub copies the staged file aside so the test can inspect it.
        let stub = Stub::new("cp \"$2\" \"$(dirname \"$0\")/staged\"");
        let r = stub.remote().temp_prefix("pk-drive-test");
        r.write("notes.md", "hello").unwrap();
        let staged = stub.dir.path().join("staged");
        assert_eq!(fs::read_to_string(&staged).unwrap(), "hello");
        let call = &stub.calls()[0];
        let tmp = call.split(' ').nth(1).unwrap();
        assert!(
            Path::new(tmp)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("pk-drive-test-remote-{}-", std::process::id())),
            "{call}"
        );
        assert!(!Path::new(tmp).exists(), "the staged file is removed");
    }

    #[cfg(unix)]
    #[test]
    fn a_no_clobber_move_whose_source_remains_is_refused() {
        // `moveto --ignore-existing` succeeds, then `lsf` finds the source.
        let stub = Stub::new("exit 0");
        let err = stub.remote().move_no_clobber("a.pdf", "b.pdf").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("example:State/b.pdf"), "{err}");
        assert_eq!(
            stub.calls(),
            vec![
                "moveto --ignore-existing example:State/a.pdf example:State/b.pdf --log-level=ERROR",
                "lsf example:State/a.pdf --log-level=ERROR",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_budgeted_call_is_killed_at_its_deadline() {
        let stub = Stub::new("sleep 5");
        let r = stub.remote().with_budget(Duration::from_millis(200));
        let started = Instant::now();
        let err = r.cat("a.json").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(err.exit_code(), 5);
        assert!(err.to_string().ends_with("rclone cat timed out"), "{err}");
    }

    #[test]
    fn a_missing_program_is_a_usage_error_naming_rclone() {
        let r = Remote::new("example:State").program("/nonexistent/pk-cli-drive/rclone");
        let err = r.cat("a.json").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains("rclone not found on PATH"),
            "{err}"
        );
    }
}
