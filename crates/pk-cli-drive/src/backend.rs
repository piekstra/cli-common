//! The store a CLI keeps its state in, behind one interface: a mounted
//! folder, rclone straight through, or rclone behind the read cache.

use pk_cli_core::CliError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cache::CachedRemote;
use crate::rclone::Remote;

/// Where the store lives. Every method takes a path relative to the store
/// root (`rel`, forward slashes) and behaves the same on every backend.
pub enum Backend {
    /// A local directory: a Drive-for-desktop mount, or any folder (tests
    /// use temp dirs). Fast, but a mount can go stale without saying so;
    /// see the crate docs. Never cached.
    Mount(PathBuf),
    /// rclone straight through, no cache: for a CLI that cannot resolve a
    /// cache dir, or that has not configured a remote yet.
    Remote(Remote),
    /// rclone behind the read cache ([`CachedRemote`]). Reads hit the
    /// cache; writes go to the remote first, then update the cache. The
    /// remote stays the single source of truth.
    Cached(CachedRemote),
}

/// A store file available on local disk: the file itself on a mount, or a
/// download whose private temp dir is removed when this is dropped.
pub struct LocalCopy {
    pub path: PathBuf,
    _dir: Option<TempDir>,
}

/// An owner-only temp directory, removed (with its contents) on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A new owner-only (`0700`) directory under the system temp dir, named
/// `<prefix>-<pid>-<nanos>-<seq>`. Created with `create`, not `create_all`:
/// an existing path (planted or stale) is refused, never reused.
pub fn private_temp_dir(prefix: &str) -> Result<TempDir, CliError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&path)
        .map_err(|e| CliError::Other(format!("creating temp dir {}: {e}", path.display())))?;
    Ok(TempDir { path })
}

/// A file in the store: path relative to the directory that was listed, plus
/// its size and last-modified time when the backend reports them without an
/// extra round trip. Serde so the cache can persist a listing as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub rel: String,
    pub size: Option<u64>,
    /// Last-modified time, Unix seconds. `None` when the backend did not
    /// report one (an rclone too old for `--time-format`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

impl Backend {
    /// Where `rel` lives, for messages: the mount path, or the remote spec
    /// with `rel` appended (never the cache dir).
    pub fn display_path(&self, rel: &str) -> String {
        match self {
            Backend::Mount(root) => {
                if rel.is_empty() {
                    root.display().to_string()
                } else {
                    root.join(rel).display().to_string()
                }
            }
            Backend::Remote(r) => r.target(rel),
            Backend::Cached(c) => c.target(rel),
        }
    }

    /// A small UTF-8 state file's contents; `None` when it does not exist.
    pub fn read_file(&self, rel: &str) -> Result<Option<String>, CliError> {
        match self {
            Backend::Mount(root) => match fs::read_to_string(root.join(rel)) {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(CliError::Other(format!("reading {rel}: {e}"))),
            },
            Backend::Remote(r) => r.cat(rel),
            Backend::Cached(c) => c.cat(rel),
        }
    }

    /// Write a small UTF-8 state file. Binary files (documents, scans) go
    /// through [`Backend::put_file`], never this.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<(), CliError> {
        match self {
            Backend::Mount(root) => {
                let path = root.join(rel);
                create_parent(&path)?;
                fs::write(&path, content)
                    .map_err(|e| CliError::Other(format!("writing {}: {e}", path.display())))
            }
            Backend::Remote(r) => r.write(rel, content),
            Backend::Cached(c) => c.write(rel, content),
        }
    }

    /// Copy a local file INTO the store at `rel`, byte for byte. Separate
    /// from `write_file` on purpose: PDFs, images and scans would be
    /// corrupted by a round trip through a `String`. Parent folders are
    /// created as needed; an existing destination is overwritten, so callers
    /// that must not clobber check `exists` first.
    pub fn put_file(&self, local: &Path, rel: &str) -> Result<(), CliError> {
        match self {
            Backend::Mount(root) => {
                let dest = root.join(rel);
                create_parent(&dest)?;
                fs::copy(local, &dest).map(|_| ()).map_err(|e| {
                    CliError::Other(format!(
                        "copying {} to {}: {e}",
                        local.display(),
                        dest.display()
                    ))
                })
            }
            Backend::Remote(r) => r.copy_in(local, rel),
            Backend::Cached(c) => c.copy_in(local, rel),
        }
    }

    /// A local copy of the store file at `rel`, for a reader that needs the
    /// bytes. A mount hands back the file itself; a remote downloads it into
    /// a private temp dir that lives as long as the returned [`LocalCopy`].
    /// Downloads are never cached.
    pub fn local_copy(&self, rel: &str) -> Result<LocalCopy, CliError> {
        let remote = match self {
            Backend::Mount(root) => {
                let path = root.join(rel);
                if !path.is_file() {
                    return Err(CliError::NotFound(format!(
                        "{}: no such file",
                        path.display()
                    )));
                }
                return Ok(LocalCopy { path, _dir: None });
            }
            Backend::Remote(r) => r,
            Backend::Cached(c) => c.remote(),
        };
        let dir = private_temp_dir(&format!("{}-doc", remote.temp_prefix))?;
        let name = rel.rsplit('/').next().filter(|n| !n.is_empty());
        let path = dir.path.join(name.unwrap_or("document"));
        remote.copy_out(rel, &path)?;
        Ok(LocalCopy {
            path,
            _dir: Some(dir),
        })
    }

    /// Move a file from one store-relative path to another. Overwrites an
    /// existing destination; see [`Backend::move_no_clobber`].
    pub fn move_within(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        match self {
            Backend::Mount(root) => {
                let (from, to) = (root.join(rel_from), root.join(rel_to));
                create_parent(&to)?;
                let Err(rename_err) = fs::rename(&from, &to) else {
                    return Ok(());
                };
                // A rename across filesystems (EXDEV) fails even when both
                // paths are inside the store — a Drive mount can sit on a
                // different volume than the rest of the tree. Fall back to
                // copy+remove rather than branching on an OS error code, and
                // if that fails too, report both causes.
                fs::copy(&from, &to).map_err(|e| {
                    CliError::Other(format!(
                        "moving {} to {}: rename failed ({rename_err}), copy fallback failed ({e})",
                        from.display(),
                        to.display()
                    ))
                })?;
                fs::remove_file(&from).map_err(|e| {
                    CliError::Other(format!(
                        "copied to {} but could not remove {}: {e}",
                        to.display(),
                        from.display()
                    ))
                })
            }
            Backend::Remote(r) => r.move_to(rel_from, rel_to),
            Backend::Cached(c) => c.move_to(rel_from, rel_to),
        }
    }

    /// [`Backend::move_within`] that never replaces an existing destination,
    /// for a destination name the caller computed. A plain move overwrites
    /// (`rename(2)` and `rclone moveto` both do), so a destination that
    /// appeared after the caller's `exists` check would be destroyed. An
    /// occupied destination exits 2 with both files untouched.
    pub fn move_no_clobber(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        match self {
            Backend::Mount(root) => {
                let to = root.join(rel_to);
                if to.exists() {
                    return Err(occupied(&to.display().to_string()));
                }
                self.move_within(rel_from, rel_to)
            }
            Backend::Remote(r) => r.move_no_clobber(rel_from, rel_to),
            Backend::Cached(c) => c.move_no_clobber(rel_from, rel_to),
        }
    }

    pub fn exists(&self, rel: &str) -> Result<bool, CliError> {
        match self {
            Backend::Mount(root) => Ok(root.join(rel).exists()),
            Backend::Remote(r) => r.exists(rel),
            Backend::Cached(c) => c.exists(rel),
        }
    }

    /// Returns true if the directory was created, false if it existed.
    pub fn ensure_dir(&self, rel: &str) -> Result<bool, CliError> {
        match self {
            Backend::Mount(root) => {
                let dir = if rel.is_empty() {
                    root.clone()
                } else {
                    root.join(rel)
                };
                if dir.is_dir() {
                    return Ok(false);
                }
                fs::create_dir_all(&dir)
                    .map_err(|e| CliError::Other(format!("creating {}: {e}", dir.display())))?;
                Ok(true)
            }
            Backend::Remote(r) => {
                if r.exists(rel)? {
                    return Ok(false);
                }
                r.mkdir(rel)?;
                Ok(true)
            }
            Backend::Cached(c) => {
                if c.exists(rel)? {
                    return Ok(false);
                }
                c.mkdir(rel)?;
                Ok(true)
            }
        }
    }

    /// All files under `rel`, as paths relative to `rel`, recursive. A
    /// missing directory reads as empty; other failures are real errors
    /// (an unreachable remote must never masquerade as "no files").
    pub fn list_files_recursive(&self, rel: &str) -> Result<Vec<String>, CliError> {
        Ok(self.list_entries(rel)?.into_iter().map(|e| e.rel).collect())
    }

    /// Same listing, with sizes and modification times — one call, no extra
    /// round trips. Sorted by path; dot-files are skipped on a mount.
    pub fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        match self {
            Backend::Mount(root) => {
                let dir = root.join(rel);
                let mut out = Vec::new();
                walk(&dir, &dir, &mut out);
                out.sort_by(|a, b| a.rel.cmp(&b.rel));
                Ok(out)
            }
            Backend::Remote(r) => r.list_entries(rel),
            Backend::Cached(c) => c.list_entries(rel),
        }
    }
}

/// The refusal every backend gives for an occupied rename destination.
pub(crate) fn occupied(target: &str) -> CliError {
    CliError::Usage(format!(
        "refusing to overwrite {target} — a document is already there; nothing was moved"
    ))
}

fn create_parent(path: &Path) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| CliError::Other(format!("creating {}: {e}", parent.display())))?;
    }
    Ok(())
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<FileEntry>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk(base, &path, out);
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                let meta = entry.metadata().ok();
                out.push(FileEntry {
                    rel: rel.to_string_lossy().into_owned(),
                    size: meta.as_ref().map(|m| m.len()),
                    modified: meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .and_then(|d| i64::try_from(d.as_secs()).ok()),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_listing_carries_mtime_and_a_no_clobber_move_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir_all(root.join("2026/A")).unwrap();
        fs::write(root.join("2026/A/one.pdf"), b"1").unwrap();
        fs::write(root.join("2026/A/two.pdf"), b"2").unwrap();
        let b = Backend::Mount(root.clone());
        let entries = b.list_entries("2026").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.modified.is_some()));

        let e = b
            .move_no_clobber("2026/A/one.pdf", "2026/A/two.pdf")
            .unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert_eq!(fs::read(root.join("2026/A/one.pdf")).unwrap(), b"1");
        assert_eq!(fs::read(root.join("2026/A/two.pdf")).unwrap(), b"2");

        b.move_no_clobber("2026/A/one.pdf", "2026/B/one.pdf")
            .unwrap();
        assert!(!root.join("2026/A/one.pdf").exists());
        assert_eq!(fs::read(root.join("2026/B/one.pdf")).unwrap(), b"1");
    }

    #[test]
    fn mount_state_files_round_trip_and_a_missing_one_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let b = Backend::Mount(tmp.path().to_path_buf());
        assert_eq!(b.read_file("state/a.json").unwrap(), None);
        assert!(!b.exists("state/a.json").unwrap());
        b.write_file("state/a.json", "{}").unwrap();
        assert_eq!(b.read_file("state/a.json").unwrap().as_deref(), Some("{}"));
        assert!(b.exists("state/a.json").unwrap());
        assert!(b.ensure_dir("inbox").unwrap());
        assert!(!b.ensure_dir("inbox").unwrap());
        fs::write(tmp.path().join("state/.hidden"), "x").unwrap();
        assert_eq!(b.list_files_recursive("state").unwrap(), vec!["a.json"]);
        assert_eq!(b.list_entries("missing").unwrap(), vec![]);
        let copy = b.local_copy("state/a.json").unwrap();
        assert_eq!(copy.path, tmp.path().join("state/a.json"));
        assert_eq!(b.local_copy("state/b.json").err().unwrap().exit_code(), 4);
    }

    #[test]
    fn a_private_temp_dir_is_owner_only_and_removed_on_drop() {
        let dir = private_temp_dir("pk-cli-drive-test").unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pk-cli-drive-test-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        drop(dir);
        assert!(!path.exists());
    }
}
