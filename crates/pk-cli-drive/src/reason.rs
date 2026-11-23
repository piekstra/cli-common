//! The causes this crate's errors name ([`CliError::reason`]). A caller
//! compares with these rather than with a slug's spelling or the message:
//! `err.reason() == Some(reason::OCCUPIED)`. Each slug is also what `--json`
//! prints as `error.reason`.
//!
//! [`CliError::reason`]: pk_cli_core::CliError::reason

use pk_cli_core::Reason;

/// A move that may not replace found its destination taken (every backend's
/// `move_no_clobber`). Exit 2; both files untouched.
pub const OCCUPIED: Reason = Reason::Provider("occupied");

/// `Drive::put_tree` with `Existing::Keep` found a file there holding other
/// bytes. Exit 2; nothing was uploaded.
pub const EXISTING_DIFFERS: Reason = Reason::Provider("existing_differs");

/// A copy would land on one of Google's own documents (Docs, Sheets), which
/// hold no bytes of their own. Exit 2; nothing was uploaded.
pub const GOOGLE_DOCUMENT: Reason = Reason::Provider("google_document");

/// `Drive::fetch_tree` found an item whose name would land outside the local
/// folder, or two items landing on one local path. Exit 1; nothing was
/// written.
pub const UNSAFE_NAME: Reason = Reason::Provider("unsafe_name");

/// Two items of one name in one folder, where a path must name one.
/// `Drive::duplicates` lists every such name.
pub const DUPLICATE_NAME: Reason = Reason::Provider("duplicate_name");

/// A write, move or delete onto or through a Google Drive shortcut. Exit 2.
pub const SHORTCUT: Reason = Reason::Provider("shortcut");

/// `Drive::trash_empty_folder` found the folder holding something outside
/// the trash, and trashed nothing. Exit 2.
pub const NOT_EMPTY: Reason = Reason::Provider("not_empty");

/// A whole listing met a shortcut whose target is gone or out of the
/// sign-in's sight.
pub const SHORTCUT_TARGET_MISSING: Reason = Reason::Provider("shortcut_target_missing");

/// A whole listing met a shortcut to a folder that holds it.
pub const SHORTCUT_LOOP: Reason = Reason::Provider("shortcut_loop");

/// The account's Drive storage is full. Exit 5.
pub const DRIVE_STORAGE_FULL: Reason = Reason::Provider("drive_storage_full");

/// A Drive profile has no OAuth client imported. Exit 2.
pub const NO_CLIENT: Reason = Reason::Provider("no_client");
