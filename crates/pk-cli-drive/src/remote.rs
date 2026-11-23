//! What a remote store offers, whichever service holds it: rclone
//! ([`crate::Remote`]) or the Google Drive API (`google::Drive`, feature
//! `google`). The read cache ([`crate::CachedRemote`]) works over either.

use std::path::Path;
use std::time::Duration;

use pk_cli_core::CliError;

use crate::backend::FileEntry;

/// The reads the cache needs from its source of truth. A trait so the
/// cache's serve/revalidate decisions are testable against an in-memory
/// remote that counts its calls, without spawning rclone.
pub trait RemoteRead {
    /// Where `rel` lives on the remote, for messages only.
    fn target(&self, rel: &str) -> String;
    fn cat(&self, rel: &str) -> Result<Option<String>, CliError>;
    fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError>;
}

/// The rest of a remote's surface: what [`crate::Backend`] and the cache's
/// writes call. Every method takes a path relative to the store root
/// (`rel`, forward slashes) and means the same on every remote.
///
/// Sealed: the remotes are this crate's ([`crate::Remote`], `google::Drive`),
/// so a method can be added without breaking an implementor elsewhere.
pub trait RemoteStore: RemoteRead + sealed::Sealed {
    /// Write a small UTF-8 file, replacing one already there.
    fn write(&self, rel: &str, content: &str) -> Result<(), CliError>;
    /// Upload a local file byte for byte, replacing one already there.
    /// Parent folders are made as needed.
    fn copy_in(&self, local: &Path, rel: &str) -> Result<(), CliError>;
    /// Download a file to `local`; a missing one is exit 4.
    fn copy_out(&self, rel: &str, local: &Path) -> Result<(), CliError>;
    /// Move within the remote, replacing an existing destination.
    fn move_to(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError>;
    /// Move within the remote; an occupied destination is exit 2 with both
    /// files untouched.
    fn move_no_clobber(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError>;
    /// Delete one file: `true` when it was deleted, `false` when no file
    /// was there. Never a folder.
    fn delete_file(&self, rel: &str) -> Result<bool, CliError>;
    /// Whether a file or folder is at `rel` ("" is the store's root).
    /// `false` for any missing path, the store's own included, never an
    /// error: [`crate::Backend::ensure_dir`] asks this, then calls `mkdir`.
    fn exists(&self, rel: &str) -> Result<bool, CliError>;
    /// Make the folder `rel` and any missing parents, the store's own path
    /// included; nothing when it is there.
    fn mkdir(&self, rel: &str) -> Result<(), CliError>;
    /// The immediate subfolders of `rel`, sorted, without a trailing `/`.
    fn list_dirs(&self, rel: &str) -> Result<Vec<String>, CliError>;
    /// Write many files byte for byte, each replacing whatever file is at
    /// its `rel`, parent folders made as needed. Every `rel` must name a file
    /// under the store root, and none may be given twice; otherwise nothing
    /// is written (exit 2). The first failure, in the order given, stops the
    /// writes and is returned; the files written before it stay. `Ok(n)`:
    /// the files whose bytes were sent.
    ///
    /// This default writes one file after another, each through an
    /// owner-only temp file and [`RemoteStore::copy_in`], and sends every
    /// file. The Drive API's remote sends several at once and skips a file
    /// that already holds the same bytes.
    fn write_many(&self, files: &[(&str, &[u8])]) -> Result<usize, CliError> {
        crate::backend::check_batch(files)?;
        let dir = crate::backend::private_temp_dir(&format!("{}-put", self.temp_name_prefix()))?;
        for (i, (rel, bytes)) in files.iter().enumerate() {
            let tmp = dir.path().join(i.to_string());
            crate::private_file::write(&tmp, bytes, crate::private_file::Replace::Never)
                .map_err(|e| CliError::Other(format!("staging {rel} for upload: {e}")))?;
            let sent = self.copy_in(&tmp, rel);
            let _ = std::fs::remove_file(&tmp);
            sent?;
        }
        Ok(files.len())
    }
    /// The prefix of this CLI's temp files (usually the binary name).
    fn temp_name_prefix(&self) -> &str;
    /// This remote with every call it makes bounded to `budget` from now,
    /// failing as upstream (exit 5) once spent. The background refresh
    /// bounds its lifetime this way.
    fn with_budget(self, budget: Duration) -> Self
    where
        Self: Sized;
}

pub(crate) mod sealed {
    /// Implemented by this crate's remotes only.
    pub trait Sealed {}
}
