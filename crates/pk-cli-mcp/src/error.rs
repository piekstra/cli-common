//! Tool errors: the family's exit codes, worded as the next step for the
//! model.
//!
//! A CLI's provider code already fails with [`CliError`], whose variant is the
//! exit code (SPEC §1.5). A tool returns the same error (`?` converts it) and
//! [`ToolError::render`] turns it into the text of an `isError` result that
//! says what to do about it. The remedy depends only on the kind, so every
//! family server words an expired login, a bad argument or a provider outage
//! the same way.
//!
//! **A tool never signs in.** An MCP server has no terminal: its stdin and
//! stdout carry the protocol, and a prompt for a password or a keychain-item
//! creation would hang the call or corrupt the stream. So an auth failure is
//! reported, with the command the user runs in their own terminal, and the
//! model relays it.

use std::fmt;

use pk_cli_core::CliError;

use crate::toolset::Server;

/// What went wrong, by the family's exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Exit 2: the arguments are wrong.
    Usage,
    /// Exit 3: not signed in, or the credential is invalid or expired.
    Auth,
    /// Exit 4: no such record.
    NotFound,
    /// Exit 5: the provider failed, timed out or rate-limited.
    Upstream,
    /// Exit 6: a change that needs the user's confirmation.
    ConfirmationRequired,
    /// Exit 1: anything else.
    Other,
}

impl ErrorKind {
    /// The kind for a family exit code; anything unknown is [`Self::Other`].
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            2 => Self::Usage,
            3 => Self::Auth,
            4 => Self::NotFound,
            5 => Self::Upstream,
            6 => Self::ConfirmationRequired,
            _ => Self::Other,
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::Auth => 3,
            Self::NotFound => 4,
            Self::Upstream => 5,
            Self::ConfirmationRequired => 6,
            Self::Other => 1,
        }
    }

    /// The `--json` error slug for the same exit code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::Upstream => "upstream",
            Self::ConfirmationRequired => "confirmation_required",
            Self::Other => "other",
        }
    }
}

/// A failed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    kind: ErrorKind,
    message: String,
    fix: Option<String>,
}

impl ToolError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            fix: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Auth, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Upstream, message)
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }

    /// From a family exit code and the CLI's message.
    pub fn from_exit_code(code: i32, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::from_exit_code(code), message)
    }

    /// Replace the kind's standard remedy with a tool-specific one, e.g.
    /// "call coinbase_accounts to see the account names".
    pub fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The `isError` result text: the kind's slug, the message, and what to
    /// do next. `server` supplies the sign-in remedy (its host's) and the
    /// CLI's name.
    pub fn render(&self, server: &Server) -> String {
        let fix = self
            .fix
            .clone()
            .unwrap_or_else(|| self.standard_fix(server));
        format!(
            "{}: {} {}",
            self.kind.code(),
            sentence(&self.message),
            sentence(&fix)
        )
    }

    fn standard_fix(&self, server: &Server) -> String {
        let bin = &server.bin;
        match self.kind {
            ErrorKind::Usage => "Fix the arguments as described and call the tool again".into(),
            ErrorKind::Auth => format!(
                "{}, then call the tool again. Tools never sign in or create credentials \
                 themselves",
                capitalized(&server.sign_in())
            ),
            ErrorKind::NotFound => {
                "Check the name or id against what the list tool returns, then call again".into()
            }
            ErrorKind::Upstream => "The provider failed or is limiting requests; the arguments \
                 are not the problem. Try again in a minute"
                .into(),
            ErrorKind::ConfirmationRequired => {
                "Nothing was changed: this needs the user's confirmation".into()
            }
            ErrorKind::Other if server.is_local() => format!(
                "If it persists, ask the user to check `{bin} auth status` and `{bin} --version` \
                 in a terminal"
            ),
            ErrorKind::Other => "If it persists, tell the user what failed".into(),
        }
    }
}

fn capitalized(s: &str) -> String {
    let mut chars = s.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// End `s` with exactly one full stop (or its own `?`/`!`).
fn sentence(s: &str) -> String {
    let t = s.trim();
    if t.ends_with(['.', '?', '!']) {
        t.to_string()
    } else {
        format!("{t}.")
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<CliError> for ToolError {
    fn from(e: CliError) -> Self {
        // The inner message, not `Display`, which prefixes "not found:" and
        // "network/upstream error:" for a terminal; the slug says that here.
        match e {
            CliError::Usage(m) => Self::usage(m),
            CliError::Auth(m) => Self::auth(m),
            CliError::NotFound(m) => Self::not_found(m),
            CliError::Upstream(m) => Self::upstream(m),
            CliError::ConfirmationRequired(m) => Self::new(ErrorKind::ConfirmationRequired, m),
            CliError::Keychain(m) => Self::other(format!("the OS keychain could not be read: {m}")),
            CliError::Other(m) => Self::other(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(bin: &str) -> Server {
        Server::new(bin, "0", "Svc", "svc")
    }

    #[test]
    fn exit_codes_round_trip() {
        for code in 1..=6 {
            assert_eq!(ErrorKind::from_exit_code(code).exit_code(), code);
        }
        assert_eq!(ErrorKind::from_exit_code(0), ErrorKind::Other);
        assert_eq!(ErrorKind::from_exit_code(99), ErrorKind::Other);
    }

    #[test]
    fn cli_errors_keep_their_kind_and_inner_message() {
        let e: ToolError = CliError::NotFound("no account `x`".into()).into();
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert_eq!(e.message(), "no account `x`");
        let e: ToolError = CliError::Upstream("HTTP 503".into()).into();
        assert_eq!(e.kind(), ErrorKind::Upstream);
        let e: ToolError = CliError::Keychain("locked".into()).into();
        assert_eq!(e.kind(), ErrorKind::Other);
    }

    #[test]
    fn auth_errors_name_the_login_command_and_never_offer_to_sign_in() {
        let text = ToolError::auth("the key was rejected (HTTP 401)").render(&server("cbase"));
        assert!(text.starts_with("auth: the key was rejected (HTTP 401). "));
        assert!(text.contains("`cbase auth login` in a terminal"));
        assert!(text.contains("never sign in"));
    }

    /// A host where `auth login` fixes nothing (a Worker) supplies its own
    /// remedy, and no CLI command is suggested anywhere.
    #[test]
    fn another_host_s_remedy_replaces_the_local_one() {
        let worker = server("cbase").hosted(
            "through code running in the user's own Cloudflare account",
            "ask the user to update the key in their Worker's secrets",
        );
        let text = ToolError::auth("the key was rejected").render(&worker);
        assert!(
            text.contains("Ask the user to update the key in their Worker's secrets, then call"),
            "{text}"
        );
        assert!(!text.contains("auth login"), "{text}");
        let text = ToolError::other("boom").render(&worker);
        assert!(!text.contains("cbase"), "{text}");
    }

    #[test]
    fn a_tool_specific_fix_replaces_the_standard_one() {
        let text = ToolError::not_found("no account named `BTC`")
            .with_fix("Call coinbase_accounts to see the account names")
            .render(&server("cbase"));
        assert_eq!(
            text,
            "not_found: no account named `BTC`. Call coinbase_accounts to see the account names."
        );
    }

    #[test]
    fn messages_get_exactly_one_full_stop() {
        let text = ToolError::usage("limit must be 1 to 200.").render(&server("x"));
        assert!(text.starts_with("usage: limit must be 1 to 200. Fix"));
        assert!(!text.contains(".."));
    }
}
