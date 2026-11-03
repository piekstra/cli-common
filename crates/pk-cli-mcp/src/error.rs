//! Tool errors: the family's exit codes, worded as the next step for the
//! model.
//!
//! A CLI's provider code already fails with [`CliError`], whose variant is the
//! exit code (SPEC §1.5). A tool returns the same error (`?` converts it) and
//! [`ToolError::render`] turns it into the text of an `isError` result that
//! says what to do about it. The remedy depends only on the kind, so every
//! family server words an expired login, a bad argument or a provider outage
//! the same way. A remedy the error kept apart for a terminal
//! ([`CliError::with_remedy`]) stays out of the tool's text.
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
            CliError::Auth(m) => Self::auth(remedy_cut_from_text(&m)),
            CliError::NotFound(m) => Self::not_found(m),
            CliError::Upstream(m) => Self::upstream(m),
            CliError::ConfirmationRequired(m) => Self::new(ErrorKind::ConfirmationRequired, m),
            // Not "read": a tool that renews a session writes the keychain too.
            CliError::Keychain(m) => Self::other(format!("the OS keychain could not be used: {m}")),
            CliError::Other(m) => Self::other(m),
            // A message with its remedy kept apart holds only what went wrong, so no
            // text is searched: an auth message may say `Run` in its own right.
            detailed @ CliError::Detailed(_) => match detailed.into_base() {
                CliError::Auth(m) => Self::auth(m),
                plain => plain.into(),
            },
        }
    }
}

/// A plain `CliError::Auth` from a message not yet moved to
/// [`CliError::with_remedy`]: why a read was refused, without the remedy it closes
/// on for a terminal (the family words it several ways). The tool error's fix names
/// the one command to ask for; a second, often different, command in the same error
/// would leave the user to pick. This reads the text, which rewording breaks silently,
/// so it goes once every family CLI's auth messages keep their remedies apart.
fn remedy_cut_from_text(message: &str) -> &str {
    [" \u{2014} run `", "; run `", " Run `", " Re-run `"]
        .iter()
        .filter_map(|remedy| message.find(remedy))
        .min()
        .map_or(message, |cut| &message[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool that renews a session writes the keychain too, so "read" would be wrong.
    #[test]
    fn a_keychain_failure_says_it_could_not_be_used() {
        let e = ToolError::from(CliError::Keychain("denied".into()));
        assert_eq!(e.kind(), ErrorKind::Other);
        assert_eq!(e.message(), "the OS keychain could not be used: denied");
    }

    #[test]
    fn a_cli_auth_message_loses_its_own_remedy_whichever_way_it_is_worded() {
        for message in [
            "no stored password \u{2014} run `old init` or `old set-credential --stdin`",
            "no stored password; run `old init`",
            "no stored password. Run `old init` again.",
            "no stored password. Re-run `old init` with the credentials you use.",
        ] {
            let text = ToolError::from(CliError::Auth(message.into())).render(&server("svc"));
            assert!(text.starts_with("auth: no stored password"), "{text}");
            assert!(text.contains("`svc auth login`"), "{text}");
            assert!(!text.contains("old"), "{text}");
        }
    }

    /// A typed remedy is not searched for: this message says "Run" as part of what went
    /// wrong, and the text cut would have eaten it.
    #[test]
    fn a_remedy_kept_apart_leaves_the_message_whole_and_out_of_the_tool_text() {
        let e = CliError::Auth("the token from a `; run ` of the old script expired".into())
            .with_remedy("run `old init`");
        let tool = ToolError::from(e);
        assert_eq!(tool.kind(), ErrorKind::Auth);
        assert_eq!(
            tool.message(),
            "the token from a `; run ` of the old script expired"
        );
        let text = tool.render(&server("svc"));
        assert!(text.contains("`svc auth login`"), "{text}");
        assert!(!text.contains("old init"), "{text}");
    }

    #[test]
    fn a_typed_error_keeps_its_kind_and_inner_message() {
        use pk_cli_core::Reason;
        for (error, kind, message) in [
            (
                CliError::Upstream("HTTP 400".into()).because(Reason::BadRequest),
                ErrorKind::Upstream,
                "HTTP 400",
            ),
            (
                CliError::NotFound("no such".into()).with_remedy("list them"),
                ErrorKind::NotFound,
                "no such",
            ),
            (
                CliError::Keychain("denied".into()).because(Reason::CredentialUnreadable),
                ErrorKind::Other,
                "the OS keychain could not be used: denied",
            ),
            (
                CliError::Other("odd".into()).because(Reason::Provider("x")),
                ErrorKind::Other,
                "odd",
            ),
        ] {
            let tool = ToolError::from(error);
            assert_eq!(tool.kind(), kind);
            assert_eq!(tool.message(), message);
        }
    }

    #[test]
    fn the_sign_in_command_reaches_errors_and_instructions_and_a_host_replaces_it() {
        struct Empty(Server);
        impl crate::toolset::Toolset for Empty {
            fn server(&self) -> Server {
                self.0.clone()
            }
            fn tools(&self) -> Vec<crate::tool::ToolDef> {
                Vec::new()
            }
            async fn call(
                &self,
                _: &str,
                _: serde_json::Value,
            ) -> Result<crate::result::ToolOutput, ToolError> {
                Err(ToolError::other("unused"))
            }
        }
        let local = server("svc").sign_in_command("svc auth login --stdin");
        let err = ToolError::auth("expired").render(&local);
        assert!(err.contains("`svc auth login --stdin`"), "{err}");
        assert!(!err.contains("`svc auth login`"), "{err}");
        let says = crate::toolset::instructions(&Empty(local.clone()));
        assert!(says.contains("`svc auth login --stdin`"), "{says}");
        assert!(!says.contains("`svc auth login`"), "{says}");

        let hosted = local.hosted("in the user's Worker", "ask the user to update the secret");
        let err = ToolError::auth("expired").render(&hosted);
        assert!(err.contains("update the secret"), "{err}");
        assert!(!err.contains("svc auth login"), "{err}");
    }

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
