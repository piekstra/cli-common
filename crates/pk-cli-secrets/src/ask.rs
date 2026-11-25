//! Keychain reads that say whether the platform asked the person: one that
//! may not ask ([`QuietRead`]) and one that may, told apart from every other
//! failure ([`AskedRead`]).
//!
//! On macOS a binary that is not yet in a keychain item's lists raises a
//! dialog when it reads the item, and [`CredentialStore::get`] waits on it
//! for as long as it takes. A quiet read turns that off
//! (`SecKeychainSetUserInteractionAllowed`): macOS answers "refused" in
//! milliseconds instead, whether it is the access list, the partition list
//! (a newly built binary) or a locked keychain that would have needed the
//! person.

use pk_cli_core::CliError;

use crate::{CredentialStore, Secret};

/// What a read that may not ask the person anything found.
#[derive(Debug)]
pub enum QuietRead {
    /// The item, without a dialog.
    Found(Secret),
    /// No such item.
    Absent,
    /// The item is there, and reading it needs a person: on macOS a dialog,
    /// which was not raised.
    NeedsPerson,
    /// This store cannot be read without possibly asking. Nothing was read.
    Unsupported,
}

/// What a read that may ask the person found.
#[derive(Debug)]
pub enum AskedRead {
    /// The item.
    Found(Secret),
    /// No such item.
    Absent,
    /// The person said no, or the platform could not ask (no login session,
    /// a locked keychain). `status` is the platform's own code, which does
    /// not tell those apart reliably.
    Refused { status: i32 },
}

impl CredentialStore {
    /// Read `account` without asking the person anything. It never raises a
    /// dialog and never waits on one.
    ///
    /// On macOS the switch behind it covers the whole process while the
    /// read lasts (milliseconds), so a read on another thread that would
    /// have raised a dialog is refused instead: call it before such reads
    /// start. On Windows the store never asks, so this is
    /// [`get`](Self::get). Elsewhere (the Linux Secret Service may ask to
    /// unlock) it is [`QuietRead::Unsupported`].
    pub fn get_quiet(&self, account: &str) -> Result<QuietRead, CliError> {
        #[cfg(target_os = "macos")]
        {
            macos::quiet(self.service(), account)
        }
        #[cfg(windows)]
        {
            Ok(match self.get(account)? {
                Some(secret) => QuietRead::Found(secret),
                None => QuietRead::Absent,
            })
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = account;
            Ok(QuietRead::Unsupported)
        }
    }

    /// Read `account` once, as [`get`](Self::get) does, saying when it was
    /// the person or the platform that refused. On macOS it may raise one
    /// dialog and waits for its answer; elsewhere a refusal is an error as
    /// it is from `get`.
    pub fn get_asking(&self, account: &str) -> Result<AskedRead, CliError> {
        #[cfg(target_os = "macos")]
        {
            macos::asking(self.service(), account)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(match self.get(account)? {
                Some(secret) => AskedRead::Found(secret),
                None => AskedRead::Absent,
            })
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::{Mutex, PoisonError};

    use pk_cli_core::CliError;
    use security_framework::os::macos::keychain::{SecKeychain, SecPreferencesDomain};
    use security_framework::os::macos::passwords::find_generic_password;

    use super::{AskedRead, QuietRead};
    use crate::Secret;

    const ITEM_NOT_FOUND: i32 = -25300;

    /// `errSecAuthFailed`, `errSecUserCanceled`,
    /// `errSecInteractionNotAllowed` and `errSecInteractionRequired`.
    pub(super) fn is_refusal(status: i32) -> bool {
        matches!(status, -25293 | -128 | -25308 | -25315)
    }

    /// Two quiet reads at once would turn the dialogs back on under each
    /// other: the switch is the process's, not the call's.
    static QUIET: Mutex<()> = Mutex::new(());

    pub(super) fn quiet(service: &str, account: &str) -> Result<QuietRead, CliError> {
        let _one = QUIET.lock().unwrap_or_else(PoisonError::into_inner);
        let _off = match SecKeychain::user_interaction_allowed() {
            // Off already, and not this function's to turn back on.
            Ok(false) => None,
            _ => match SecKeychain::disable_user_interaction() {
                Ok(off) => Some(off),
                Err(_) => return Ok(QuietRead::Unsupported),
            },
        };
        Ok(match asking(service, account)? {
            AskedRead::Found(secret) => QuietRead::Found(secret),
            AskedRead::Absent => QuietRead::Absent,
            AskedRead::Refused { .. } => QuietRead::NeedsPerson,
        })
    }

    /// The read `keyring` makes, with macOS's own status kept.
    pub(super) fn asking(service: &str, account: &str) -> Result<AskedRead, CliError> {
        let keychain = SecKeychain::default_for_domain(SecPreferencesDomain::User)
            .map_err(|e| CliError::Keychain(format!("opening the login keychain: {e}")))?;
        match find_generic_password(Some(&[keychain]), service, account) {
            Ok((bytes, _item)) => String::from_utf8(bytes.to_owned())
                .map(|text| AskedRead::Found(Secret::new(text)))
                .map_err(|_| CliError::Keychain("reading credential: not UTF-8".into())),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(AskedRead::Absent),
            Err(e) if is_refusal(e.code()) => Ok(AskedRead::Refused { status: e.code() }),
            Err(e) => Err(CliError::Keychain(format!("reading credential: {e}"))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// No keychain here: a test that read one would be a dialog.
        #[test]
        fn a_refusal_is_told_from_an_absent_item_and_from_a_failure() {
            for status in [-25293, -128, -25308, -25315] {
                assert!(is_refusal(status), "{status}");
            }
            for status in [0, ITEM_NOT_FOUND, -25291, -25294, -50] {
                assert!(!is_refusal(status), "{status}");
            }
        }
    }
}
