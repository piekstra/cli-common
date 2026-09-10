//! The mutation gate (SPEC v1 §1.3): a mutation prompts for confirmation
//! unless `--force`, and when it *cannot* prompt — `--json`, or stdin is not a
//! terminal — it fails with exit 6 instead of hanging on a read nobody will
//! answer.
//!
//! # The ordering rule
//!
//! Decide whether a confirmation is even *possible* **before** any keychain or
//! network work, and prompt with the details **after** the cheap reads that
//! produce them. Split that way because the two halves want to run at
//! different times:
//!
//! - [`require_confirmable`] needs only `--force` and the interactivity flag,
//!   both known at parse time. Running it first means a driver that forgot
//!   `--force` gets its exit 6 in milliseconds — no keychain prompt on macOS,
//!   no session mint, no request to the provider — and `--help`-style
//!   mistakes never touch a credential.
//! - [`confirm`] wants the resolved names ("Move Office Lamp to Kitchen?"),
//!   which usually take a read to produce. By the time it runs the answer to
//!   "may we prompt at all?" is already yes, so its only outcomes are
//!   *proceed* or *cancelled*.
//!
//! [`gate`] is the one-call form for the common case where the prompt text is
//! known up front; it is the two steps back to back.
//!
//! Every refusal is [`CliError::ConfirmationRequired`] (exit 6), including a
//! user answering "n": to a script the two are the same condition — the
//! mutation did not run because nobody confirmed it.

use std::io::BufRead;

use crate::{CliError, CommonArgs};

/// Pass when the mutation may proceed to a prompt (or straight through with
/// `--force`); exit 6 when it could neither prompt nor was forced.
///
/// Call this **before** any keychain or network work — see the module docs
/// for why. `interactive` is [`CommonArgs::interactive`] (stdin is a TTY and
/// no `--json`); `what` names the mutation for the error message.
pub fn require_confirmable(force: bool, interactive: bool, what: &str) -> Result<(), CliError> {
    if force || interactive {
        Ok(())
    } else {
        Err(CliError::ConfirmationRequired(format!(
            "{what} — pass --force to run non-interactively"
        )))
    }
}

/// Interactive yes/no on stderr (`<prompt> [y/N]`); `--force` skips it.
///
/// Only reach this after [`require_confirmable`] passed, so stdin is known to
/// be answerable. Anything but `y`/`yes` (case-insensitive) is a refusal,
/// reported as [`CliError::ConfirmationRequired`] — see the module docs.
/// The prompt goes to stderr because stdout is data (§1.4).
pub fn confirm(force: bool, prompt: &str) -> Result<(), CliError> {
    if force {
        return Ok(());
    }
    eprint!("{prompt} [y/N] ");
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    if read_answer(&mut lock)? {
        Ok(())
    } else {
        Err(CliError::ConfirmationRequired("cancelled".into()))
    }
}

/// [`require_confirmable`] then [`confirm`] in one call, for mutations whose
/// prompt is knowable before any work is done ("Cancel order 42?").
///
/// Still call it **first** in the command — before the keychain or network —
/// or it loses the property the split exists for.
pub fn gate(common: &CommonArgs, force: bool, prompt: &str) -> Result<(), CliError> {
    require_confirmable(force, common.interactive(), prompt)?;
    confirm(force, prompt)
}

/// One line from `input`, interpreted as a yes/no answer. EOF is "no".
fn read_answer(input: &mut impl BufRead) -> Result<bool, CliError> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| CliError::Other(format!("reading confirmation: {e}")))?;
    Ok(accepts(&line))
}

fn accepts(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn require_confirmable_passes_on_force_or_interactive() {
        assert!(require_confirmable(true, false, "x").is_ok());
        assert!(require_confirmable(false, true, "x").is_ok());
        assert!(require_confirmable(true, true, "x").is_ok());
    }

    #[test]
    fn require_confirmable_is_exit_6_when_neither() {
        let err = require_confirmable(false, false, "deleting the room").unwrap_err();
        assert!(matches!(err, CliError::ConfirmationRequired(_)));
        assert_eq!(err.exit_code(), 6);
        let msg = err.to_string();
        assert!(msg.contains("deleting the room"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
    }

    #[test]
    fn force_skips_the_prompt_entirely() {
        // No stdin is read: were it, this test would block on the harness's
        // stdin or fail on a closed one.
        assert!(confirm(true, "Proceed?").is_ok());
    }

    #[test]
    fn answers_are_y_or_yes_case_insensitive_and_trimmed() {
        for yes in ["y\n", "Y\n", "yes\n", "YES\n", "  Yes  \n", "y"] {
            assert!(read_answer(&mut Cursor::new(yes)).unwrap(), "{yes:?}");
        }
        for no in ["n\n", "N\n", "no\n", "\n", "", "yeah\n", "ye\n", "y n\n"] {
            assert!(!read_answer(&mut Cursor::new(no)).unwrap(), "{no:?}");
        }
    }

    #[test]
    fn a_refusal_is_the_same_exit_as_not_being_able_to_ask() {
        // A script cannot tell "user said no" from "nobody could say yes",
        // and should not have to: both are exit 6.
        let refused = CliError::ConfirmationRequired("cancelled".into());
        let unaskable = require_confirmable(false, false, "x").unwrap_err();
        assert_eq!(refused.exit_code(), unaskable.exit_code());
        assert_eq!(refused.code(), unaskable.code());
    }
}
