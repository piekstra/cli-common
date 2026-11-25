//! `auth key`: read the vault's key once and say what the keychain did
//! (`auth-key/v1`, DESIGN.md §2.5).
//!
//! The first run of a newly built or upgraded binary is the one macOS asks
//! about, and no other command is sure to reach the keychain: one that
//! answers from a cache, or finds nothing stored, exits 0 having read
//! nothing. `auth key` reads the key and nothing else, so it is the first
//! call after an install:
//!
//! ```no_run
//! # use pk_cli_auth::{key::KeyArgs, vault::Vault};
//! # fn run(args: KeyArgs, json: bool) -> Result<(), pk_cli_core::CliError> {
//! // In the CLI's dispatch, before anything that opens the vault:
//! let code = Vault::for_binary("svc")?.check_key(&args)?.emit(json);
//! if code != 0 {
//!     std::process::exit(code);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! At most one read may raise a dialog, and it is never tried again. On
//! macOS a read that may not ask ([`KeyStore::get_quiet`]) comes before it,
//! which says whether a dialog is needed, and another follows a dialog that
//! was answered, which says whether the answer was Always Allow or Allow:
//! macOS does not report the button, and lets this binary read again without
//! asking only after Always Allow.
//!
//! A CLI that keeps its credential in a keychain item instead of a vault
//! calls [`check_item`] on that item.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use pk_cli_core::{output, CliError, Reason};
use pk_cli_secrets::{signing, AskedRead, QuietRead, Secret, Signing};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::vault::KeyStore;

/// The `auth key` flags.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct KeyArgs {
    /// Raise no keychain dialog: exit 3 if reading the key would need one.
    #[arg(long)]
    pub no_dialog: bool,
    /// Wait this long for the keychain dialog to be answered (default: until
    /// it is).
    #[arg(
        long,
        value_name = "SECONDS",
        conflicts_with = "no_dialog",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub wait: Option<u64>,
}

/// Where the key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeySource {
    /// The keychain, read by this command. `auth key` has no cache, so a key
    /// that was read is always this.
    Keychain,
    /// Nothing was read.
    None,
}

/// What the keychain did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyOutcome {
    /// It gave the item.
    Read,
    /// It holds no such item. No dialog is raised for one.
    Absent,
    /// The person denied it, or it could not ask.
    Refused,
    /// It would have had to ask, and `--no-dialog` said not to.
    NeedsDialog,
    /// Nobody answered its dialog within `--wait`.
    NoAnswer,
    /// It failed otherwise.
    Failed,
}

/// What macOS lets this binary do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyGrant {
    /// Read the item without asking: no dialog was needed, or it was answered
    /// Always Allow.
    Always,
    /// The dialog was answered Allow, which covered that one read: the next
    /// asks again.
    Once,
}

/// The `auth key --json` DTO (`auth-key/v1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeyCheck {
    pub schema: String,
    /// Whether the keychain gave the item to this binary.
    pub read: bool,
    pub source: KeySource,
    pub outcome: KeyOutcome,
    /// The keychain service read, which a test override renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub account: String,
    /// Whether reading the item needed a dialog. Absent when the platform
    /// does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog_needed: Option<bool>,
    /// Present when the item was read and the platform says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<KeyGrant>,
    /// From the start of the check to the keychain's answer.
    pub elapsed_ms: u64,
    /// The running binary's signature, on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<Signing>,
    /// Why the command fails, printed as `error` in JSON mode.
    #[serde(skip)]
    failure: Option<CliError>,
}

impl KeyCheck {
    /// Why the command fails: `None` when the key was read, or when there is
    /// nothing to read and nothing that needs it.
    pub fn failure(&self) -> Option<&CliError> {
        self.failure.as_ref()
    }

    /// The one JSON document: this DTO, with the error contract's `error`
    /// object in it on a failure (DESIGN.md §1.4).
    pub fn to_json(&self) -> Value {
        let mut dto = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(failure) = &self.failure {
            dto["error"] = failure.to_json()["error"].clone();
        }
        dto
    }

    /// Standard text rendering (`Key: value` block).
    pub fn render(&self) {
        print!("{}", self.text());
    }

    fn text(&self) -> String {
        let yes_no = |yes| if yes { "yes" } else { "no" };
        let item = match &self.service {
            Some(service) => format!("{service}, {}", self.account),
            None => self.account.clone(),
        };
        let mut text = format!("Read:          {}\n", yes_no(self.read));
        let source = match self.source {
            KeySource::Keychain => "keychain",
            KeySource::None => "none",
        };
        text.push_str(&format!("Source:        {source} ({item})\n"));
        if self.outcome == KeyOutcome::Absent && self.failure.is_none() {
            text.push_str("Nothing stored: no key, and nothing that needs one\n");
        }
        if let Some(needed) = self.dialog_needed {
            text.push_str(&format!("Dialog needed: {}\n", yes_no(needed)));
        }
        match self.grant {
            Some(KeyGrant::Always) => text.push_str("Grant:         always\n"),
            Some(KeyGrant::Once) => text.push_str(
                "Grant:         once (the next read asks again; choose Always Allow then)\n",
            ),
            None => {}
        }
        text.push_str(&format!("Elapsed:       {} ms\n", self.elapsed_ms));
        if let Some(signing) = &self.signing {
            text.push_str(&format!("Signed:        {signing}\n"));
        }
        text
    }

    /// Emit per the output contract (the DTO in JSON mode, the block
    /// otherwise, a failure's message on stderr) and give the exit code to
    /// end with. End with it directly: returning the failure as well would
    /// print a second document.
    pub fn emit(&self, json_mode: bool) -> i32 {
        if json_mode {
            output::json(&self.to_json());
        } else {
            self.render();
        }
        match &self.failure {
            Some(failure) => output::fail(failure, false),
            None => 0,
        }
    }
}

/// How a read ended, with what it gave.
enum End {
    Found(Secret),
    Absent,
    Refused(i32),
    NeedsDialog,
    Unanswered(u64),
    Failed(CliError),
}

/// Read the keychain item `account` once, as `auth key` does, for a CLI that
/// keeps its credential in an item instead of a vault
/// ([`Vault::check_key`](crate::vault::Vault::check_key) otherwise). `judge`
/// says what is wrong with what was found (`None`: no such item), if
/// anything.
///
/// An error is a usage error, before anything is read: `--no-dialog` on a
/// store that cannot be read without possibly asking. Every other failure is
/// in the [`KeyCheck`].
pub fn check_item(
    keys: &Arc<dyn KeyStore>,
    account: &str,
    args: &KeyArgs,
    judge: impl FnOnce(Option<&Secret>) -> Result<(), CliError>,
) -> Result<KeyCheck, CliError> {
    let started = Instant::now();
    let (answer, dialog_needed) =
        match keys.get_quiet(account) {
            Ok(QuietRead::Found(item)) => (End::Found(item), Some(false)),
            Ok(QuietRead::Absent) => (End::Absent, Some(false)),
            Ok(QuietRead::NeedsPerson) if args.no_dialog => (End::NeedsDialog, Some(true)),
            Ok(QuietRead::NeedsPerson) => (ask(keys, account, args.wait), Some(true)),
            Ok(QuietRead::Unsupported) if args.no_dialog => return Err(CliError::Usage(
                "--no-dialog needs a keychain that can be read without possibly asking; this one \
                 cannot"
                    .into(),
            )),
            Ok(QuietRead::Unsupported) => (ask(keys, account, args.wait), None),
            Err(e) => (End::Failed(e), None),
        };
    let elapsed = started.elapsed();
    let grant = match (&answer, dialog_needed) {
        (End::Found(_), Some(false)) => Some(KeyGrant::Always),
        (End::Found(_), Some(true)) => match keys.get_quiet(account) {
            Ok(QuietRead::Found(_)) => Some(KeyGrant::Always),
            Ok(QuietRead::NeedsPerson) => Some(KeyGrant::Once),
            _ => None,
        },
        _ => None,
    };
    let (outcome, failure) = match answer {
        End::Found(item) => (KeyOutcome::Read, judge(Some(&item)).err()),
        End::Absent => (KeyOutcome::Absent, judge(None).err()),
        End::Refused(status) => (KeyOutcome::Refused, Some(refused(status))),
        End::NeedsDialog => (KeyOutcome::NeedsDialog, Some(needs_dialog())),
        End::Unanswered(secs) => (KeyOutcome::NoAnswer, Some(no_answer(secs))),
        End::Failed(e) => (KeyOutcome::Failed, Some(e)),
    };
    let read = outcome == KeyOutcome::Read;
    Ok(KeyCheck {
        schema: "auth-key/v1".into(),
        read,
        source: if read {
            KeySource::Keychain
        } else {
            KeySource::None
        },
        outcome,
        service: keys.keychain_service().map(str::to_string),
        account: account.into(),
        dialog_needed,
        grant,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        signing: signing::running_binary(),
        failure,
    })
}

/// The one read that may raise a dialog, waiting for it at most `wait`
/// seconds. One left unanswered goes on in the background until the process
/// ends.
fn ask(keys: &Arc<dyn KeyStore>, account: &str, wait: Option<u64>) -> End {
    let answered = match wait {
        None => keys.get_asking(account),
        Some(secs) => {
            let (tx, rx) = mpsc::channel();
            let (keys, account) = (Arc::clone(keys), account.to_string());
            std::thread::spawn(move || {
                let _ = tx.send(keys.get_asking(&account));
            });
            match rx.recv_timeout(Duration::from_secs(secs)) {
                Ok(answered) => answered,
                Err(mpsc::RecvTimeoutError::Timeout) => return End::Unanswered(secs),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(CliError::Keychain(
                    "the keychain read ended without an answer".into(),
                )),
            }
        }
    };
    match answered {
        Ok(AskedRead::Found(item)) => End::Found(item),
        Ok(AskedRead::Absent) => End::Absent,
        Ok(AskedRead::Refused { status }) => End::Refused(status),
        Err(e) => End::Failed(e),
    }
}

fn unread(message: String, remedy: &str) -> CliError {
    CliError::Auth(message)
        .because(Reason::CredentialUnreadable)
        .with_remedy(remedy)
}

fn refused(status: i32) -> CliError {
    unread(
        format!(
            "the keychain refused the read (status {status}): its dialog was denied, or it could \
             not ask (no login session, or a locked keychain)"
        ),
        "run this again at the screen and choose Always Allow",
    )
}

fn needs_dialog() -> CliError {
    unread(
        "this build may not read the key until someone answers the keychain's dialog; none was \
         raised"
            .into(),
        "run `auth key` at the screen without --no-dialog and choose Always Allow",
    )
}

fn no_answer(secs: u64) -> CliError {
    unread(
        format!("nobody answered the keychain's dialog within {secs} s"),
        "run this again at the screen and choose Always Allow",
    )
}

#[cfg(test)]
mod tests;
