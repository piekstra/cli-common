//! Reading the code out of the mailbox it was sent to, so a login with an
//! email second factor can finish unattended.
//!
//! The family reads Gmail through `gro` (the read-only Google CLI);
//! [`GroMailbox`] drives it as a subprocess. Agents reach the same mailbox
//! through the `gmail_search` MCP tool, which a compiled CLI cannot call, so
//! the MCP route stays with the agent: it reads the code and passes it to
//! `auth login --code -`.
//!
//! # Rails
//!
//! - **Nothing secret goes on `gro`'s argv.** The argv carries the search
//!   query (a sender and subject the CLI hard-codes, plus a timestamp) and a
//!   message id. The code comes back on `gro`'s stdout.
//! - **Only mail newer than the request counts.** The query is narrowed with
//!   `after:<unix seconds>` taken *before* the code was requested, so an
//!   older code email can never be redeemed against the new session.
//! - **One failure stops the reading.** `gro` reads its OAuth token from the
//!   keychain, and a keychain read can raise a macOS approval dialog. A `gro`
//!   that fails or hangs is not called again in the same login: retrying
//!   would raise one dialog per attempt. See the polling loop in the parent
//!   module.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pk_cli_core::CliError;

use super::code::{extract_code, CodeShape, OtpCode};

/// Which emails carry the provider's code, in Gmail search syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailQuery {
    query: String,
    shape: CodeShape,
}

impl MailQuery {
    /// `query` is Gmail search syntax naming the provider's code email, e.g.
    /// `from:no-reply@example.com subject:"verification code"`. Control
    /// characters are refused: the query is one argv element and one line.
    pub fn new(query: impl Into<String>) -> Result<Self, CliError> {
        let query = query.into();
        if query.trim().is_empty() {
            return Err(CliError::Usage("the mailbox query is empty".into()));
        }
        if query.chars().any(char::is_control) {
            return Err(CliError::Usage(
                "the mailbox query contains a control character".into(),
            ));
        }
        Ok(MailQuery {
            query,
            shape: CodeShape::default(),
        })
    }

    /// What the provider's codes look like (default: six digits).
    pub fn code_shape(mut self, shape: CodeShape) -> Self {
        self.shape = shape;
        self
    }

    pub fn shape(&self) -> CodeShape {
        self.shape
    }

    /// The query narrowed to mail received after `since_unix`.
    pub fn after(&self, since_unix: u64) -> String {
        format!("{} after:{since_unix}", self.query.trim())
    }
}

/// A mailbox the code can be read from.
///
/// One call is one look: `Ok(None)` means no matching email has arrived yet,
/// and the caller decides whether to look again. An `Err` means the mailbox
/// could not be read at all, and the caller stops looking.
pub trait Mailbox {
    fn find_code(&self, query: &MailQuery, since_unix: u64) -> Result<Option<OtpCode>, CliError>;
}

/// How [`GroMailbox`] runs a program. A seam so the parsing and the argv can
/// be tested offline without a `gro` on the machine.
trait Runner {
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> Result<String, CliError>;
}

struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> Result<String, CliError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    CliError::Upstream(format!("`{program}` is not installed or not on PATH"))
                }
                _ => CliError::Upstream(format!("starting `{program}`: {e}")),
            })?;
        // Drain both pipes on threads so a chatty child can't block on a full
        // pipe while we wait for it to exit.
        let mut out = child.stdout.take().expect("stdout is piped");
        let mut err = child.stderr.take().expect("stderr is piped");
        let out_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CliError::Upstream(format!(
                        "`{program}` did not answer within {}s — it may be waiting on a \
                         keychain approval",
                        timeout.as_secs()
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(CliError::Upstream(format!("waiting on `{program}`: {e}"))),
            }
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        if !status.success() {
            let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let first: String = first.chars().take(200).collect();
            return Err(CliError::Upstream(format!(
                "`{program}` failed ({status}){}",
                if first.is_empty() {
                    String::new()
                } else {
                    format!(": {first}")
                }
            )));
        }
        Ok(stdout)
    }
}

/// The family's Gmail reader, `gro`, as a [`Mailbox`].
///
/// One look is `gro mail search --max 5 -- "<query> after:<t>"`; the code is
/// read from the subject and snippet of the newest hit that carries one. Only
/// when no hit's snippet does is the newest message itself read
/// (`gro mail read -- <id>`), once.
pub struct GroMailbox {
    program: String,
    credential_ref: Option<String>,
    timeout: Duration,
    runner: Box<dyn Runner>,
}

/// Search hits fetched per look. A code email is the newest match; a few more
/// cover a provider that sends a "new sign-in" notice alongside it.
const MAX_HITS: u32 = 5;

impl Default for GroMailbox {
    fn default() -> Self {
        GroMailbox::new()
    }
}

impl GroMailbox {
    /// `gro` from `PATH`, its active profile, a 30-second limit per call.
    pub fn new() -> Self {
        GroMailbox {
            program: "gro".into(),
            credential_ref: None,
            timeout: Duration::from_secs(30),
            runner: Box::new(ProcessRunner),
        }
    }

    /// Run a different binary (a full path, or a renamed install).
    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Read through a specific `gro` credential ref (`<service>/<profile>`),
    /// for the account the provider mails rather than `gro`'s active one.
    pub fn credential_ref(mut self, credential_ref: impl Into<String>) -> Result<Self, CliError> {
        let r = credential_ref.into();
        if r.trim().is_empty() || r.starts_with('-') || r.chars().any(char::is_control) {
            return Err(CliError::Usage(format!(
                "`{r}` is not a gro credential ref (expected <service>/<profile>)"
            )));
        }
        self.credential_ref = Some(r);
        Ok(self)
    }

    /// The limit on each `gro` call. A call that runs past it is killed and
    /// the mailbox is not read again in this login.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_runner(mut self, runner: impl Runner + 'static) -> Self {
        self.runner = Box::new(runner);
        self
    }

    fn args(&self, rest: &[&str]) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(r) = &self.credential_ref {
            args.push("--ref".to_string());
            args.push(r.clone());
        }
        args.extend(rest.iter().map(|s| s.to_string()));
        args
    }
}

impl Mailbox for GroMailbox {
    fn find_code(&self, query: &MailQuery, since_unix: u64) -> Result<Option<OtpCode>, CliError> {
        let max = MAX_HITS.to_string();
        let q = query.after(since_unix);
        // `--` ends gro's flags, so a query beginning with `-` (Gmail's
        // negation) is a query, never an option.
        let search = self.args(&["mail", "search", "--max", &max, "--", &q]);
        let out = self.runner.run(&self.program, &search, self.timeout)?;
        let hits = parse_search(&out);
        let Some(newest) = hits.first() else {
            return Ok(None);
        };
        for hit in &hits {
            let text = format!("{}\n{}", hit.subject, hit.snippet);
            if let Some(code) = extract_code(&text, query.shape()) {
                return Ok(Some(code));
            }
        }
        if newest.id.is_empty() {
            return Ok(None);
        }
        let read = self.args(&["mail", "read", "--", &newest.id]);
        let body = self.runner.run(&self.program, &read, self.timeout)?;
        Ok(extract_code(&message_text(&body), query.shape()))
    }
}

/// One `gro mail search` hit.
#[derive(Debug, Default, PartialEq, Eq)]
struct Hit {
    id: String,
    subject: String,
    snippet: String,
}

/// Parse `gro mail search` text output: `Key: value` blocks separated by
/// `---` lines, newest first. `No messages found.` (or anything without an
/// `ID:` line) is no hits.
fn parse_search(out: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut cur = Hit::default();
    let mut started = false;
    for line in out.lines() {
        if line.trim() == "---" {
            if started {
                hits.push(std::mem::take(&mut cur));
            }
            started = false;
            continue;
        }
        if let Some(v) = line.strip_prefix("ID: ") {
            cur.id = v.trim().to_string();
            started = true;
        } else if let Some(v) = line.strip_prefix("Subject: ") {
            cur.subject = v.to_string();
        } else if let Some(v) = line.strip_prefix("Snippet: ") {
            cur.snippet = v.to_string();
        }
    }
    if started {
        hits.push(cur);
    }
    hits
}

/// The subject and body of `gro mail read` output, without the `From:`/`To:`
/// headers (an address's digits are never the code).
fn message_text(out: &str) -> String {
    let subject = out
        .lines()
        .find_map(|l| l.strip_prefix("Subject: "))
        .unwrap_or("");
    let body = out.split_once("--- Body ---").map(|(_, b)| b).unwrap_or("");
    format!("{subject}\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const SEARCH: &str = include_str!("../../tests/fixtures/gro/search-code.txt");
    const SEARCH_NO_CODE: &str = include_str!("../../tests/fixtures/gro/search-no-code.txt");
    const READ: &str = include_str!("../../tests/fixtures/gro/read-code.txt");
    const NONE: &str = include_str!("../../tests/fixtures/gro/search-empty.txt");

    /// Answers each call with the next canned output and records the argv.
    #[derive(Clone)]
    struct Canned {
        outputs: Rc<RefCell<Vec<Result<String, CliError>>>>,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl Canned {
        fn new(outputs: Vec<Result<&str, CliError>>) -> Self {
            Canned {
                outputs: Rc::new(RefCell::new(
                    outputs.into_iter().map(|o| o.map(str::to_string)).collect(),
                )),
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl Runner for Canned {
        fn run(&self, program: &str, args: &[String], _t: Duration) -> Result<String, CliError> {
            assert_eq!(program, "gro");
            self.calls.borrow_mut().push(args.to_vec());
            self.outputs.borrow_mut().remove(0)
        }
    }

    fn query() -> MailQuery {
        MailQuery::new(r#"from:no-reply@example.com subject:"verification code""#).unwrap()
    }

    #[test]
    fn the_code_comes_from_the_newest_hits_snippet_in_one_call() {
        let runner = Canned::new(vec![Ok(SEARCH)]);
        let mb = GroMailbox::new().with_runner(runner.clone());
        let code = mb.find_code(&query(), 1_800_000_000).unwrap().unwrap();
        assert_eq!(code.expose(), "482917");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec![
                "mail",
                "search",
                "--max",
                "5",
                "--",
                r#"from:no-reply@example.com subject:"verification code" after:1800000000"#,
            ]
        );
    }

    #[test]
    fn a_snippet_without_a_code_reads_the_newest_message_once() {
        let runner = Canned::new(vec![Ok(SEARCH_NO_CODE), Ok(READ)]);
        let mb = GroMailbox::new().with_runner(runner.clone());
        let code = mb.find_code(&query(), 1).unwrap().unwrap();
        assert_eq!(code.expose(), "305116");
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], vec!["mail", "read", "--", "19a0c0ffee000002"]);
    }

    #[test]
    fn no_mail_yet_is_none_not_an_error() {
        let runner = Canned::new(vec![Ok(NONE)]);
        let mb = GroMailbox::new().with_runner(runner);
        assert_eq!(mb.find_code(&query(), 1).unwrap(), None);
    }

    #[test]
    fn a_gro_failure_is_an_error_for_the_caller_to_stop_on() {
        let runner = Canned::new(vec![Err(CliError::Upstream("`gro` failed".into()))]);
        let mb = GroMailbox::new().with_runner(runner);
        assert_eq!(mb.find_code(&query(), 1).unwrap_err().exit_code(), 5);
    }

    #[test]
    fn the_credential_ref_leads_the_argv_and_is_validated() {
        let runner = Canned::new(vec![Ok(NONE)]);
        let mb = GroMailbox::new()
            .credential_ref("gro/work")
            .unwrap()
            .with_runner(runner.clone());
        mb.find_code(&query(), 1).unwrap();
        assert_eq!(&runner.calls.borrow()[0][..2], ["--ref", "gro/work"]);
        for bad in ["", "--max", "a\nb"] {
            assert!(GroMailbox::new().credential_ref(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_query_is_validated_and_narrowed_by_time() {
        assert!(MailQuery::new("  ").is_err());
        assert!(MailQuery::new("from:a@example.com\nlabel:x").is_err());
        assert_eq!(
            MailQuery::new("from:a@example.com ").unwrap().after(42),
            "from:a@example.com after:42"
        );
    }

    #[test]
    fn search_output_parses_into_hits_newest_first() {
        let hits = parse_search(SEARCH);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "19a0c0ffee000001");
        assert!(hits[0].snippet.contains("482917"));
        assert!(parse_search(NONE).is_empty());
        assert!(parse_search("").is_empty());
    }

    #[test]
    fn a_missing_program_is_an_upstream_error() {
        let err = ProcessRunner
            .run("pk-cli-auth-no-such-program", &[], Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(err.to_string().contains("not installed"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_program_is_killed_at_the_timeout() {
        let started = Instant::now();
        let err = ProcessRunner
            .run("sleep", &["5".into()], Duration::from_millis(200))
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(err.to_string().contains("did not answer"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_program_reports_its_first_stderr_line() {
        let err = ProcessRunner
            .run(
                "sh",
                &["-c".into(), "echo 'token expired' >&2; exit 1".into()],
                Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(err.to_string().contains("token expired"), "{err}");
    }
}
