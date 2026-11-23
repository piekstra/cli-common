//! The Google Drive API backend and Drive profiles (feature `google`).
//!
//! rclone's shared Google sign-in client is being retired, and every Drive
//! writer needs an OAuth client of its own. This module is that writer for
//! the family: a [`Drive`] store behind the same interface as the rclone
//! [`crate::Remote`] (so [`crate::CachedRemote`] and [`crate::Backend`] work
//! over it unchanged), signed in through a Drive profile.
//!
//! - **A profile is one Google account**: the Desktop app OAuth client it
//!   uses (imported from Google's JSON, by file or stdin), the account's
//!   email verified at sign-in, the scopes granted, and the token set. The
//!   client and the token set are vault entries (`pk_cli_auth::vault`, one
//!   keychain item per CLI); the rest is `drive.json` beside the CLI's
//!   config ([`ProfileStore`]). There is no default profile.
//! - **Commands any CLI can mount** ([`DriveCommand`], run by
//!   [`DriveCli::run`]): `login --profile <NAME>` (PKCE with a loopback
//!   redirect, the browser opened by the command), `status [--json]`
//!   (`auth-status/v1`), `logout`, `about`, `client import`, and
//!   `profiles list|remove`.
//! - **The store** ([`DriveCli::drive`]): paths resolve to file ids through a
//!   map cached for the command; listings carry size, modified time and md5;
//!   writes replace an existing file's content in place (`files.update`),
//!   never a second file of the same name; files above 5 MiB go up in a
//!   resumable session; [`Drive::put_tree`] skips files whose md5 already
//!   matches; two children of one folder with the same name are an error,
//!   never a guess; requests retry with backoff on 429, rate-limit 403s and
//!   5xx; [`Drive::about`] gives the account's email and quota.
//! - **Tests** run against [`fake::FakeDrive`] (feature `fake-google`), a
//!   Drive API server on 127.0.0.1 holding synthetic files, which a CLI's
//!   black-box tests reach through `<PREFIX>_DRIVE_TEST_ENDPOINT`.
//!
//! The scope a CLI asks for, and the evidence behind the choice, are in the
//! crate's `docs/google-drive.md`.

mod account;
mod api;
mod client;
mod commands;
mod drive;
mod endpoints;
mod error;
mod files;
mod profiles;

#[cfg(any(test, feature = "fake-google"))]
pub mod fake;

pub use account::Account;
pub use api::Retry;
pub use client::{id_hint, DesktopClient};
pub use commands::{
    ClientCommand, DriveArgs, DriveCli, DriveCommand, FolderCommand, LoginArgs, LogoutArgs,
    ProfilesCommand, Scope, StatusArgs, LOGIN_WAIT_MINUTES,
};
pub use drive::{
    check_folder_id, Drive, DriveSpec, Duplicate, Existing, Hashed, Listing, TreeCopy, SPEC_PREFIX,
};
pub use endpoints::Endpoints;
pub use files::{md5_file, About, DriveFile, ShortcutDetails, RESUMABLE_ABOVE};
pub use profiles::{client_entry, token_entry, DriveProfile, ProfileStore};
