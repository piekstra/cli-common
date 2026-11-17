//! Owner-only file output: the one writer for files that hold the owner's
//! data, used by `pk-cli-config`'s `config.json`, `pk-cli-auth`'s vault and
//! `pk-cli-drive`'s read cache and upload staging. A CLI can use it for its
//! own exports too.
//!
//! [`write`] replaces whatever is at the path, a symlink included. A caller
//! that keeps a link (a dotfiles-managed config) resolves it first with
//! [`resolve_links`] and writes the target.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// What [`write`] does when `path` already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// Fail with `ErrorKind::AlreadyExists`. The create is exclusive, so an
    /// existing path (a symlink included) is never written through.
    Never,
    /// Write a new file beside `path`, sync it to disk, rename it over
    /// `path`, then sync the directory. A reader never sees a torn file, not
    /// even after a crash or a power loss right after the rename; the mtime
    /// moves atomically; a file that was wider than `0600` is replaced by a
    /// `0600` one; and a symlink at `path` is replaced rather than followed.
    ///
    /// The directory sync is best effort: once the rename has succeeded the
    /// new file is in place, so a filesystem that refuses to sync a
    /// directory (some network and FUSE mounts) does not turn a completed
    /// write into an error.
    Always,
}

/// Write `bytes` to `path`, readable by its owner only (`0600` on Unix). A
/// failed write leaves nothing behind: not a partial file at `path` (which
/// this call created, so it is ours to remove), not a temporary file.
pub fn write(path: &Path, bytes: &[u8], replace: Replace) -> io::Result<()> {
    if replace == Replace::Never {
        return create_filled(path, bytes);
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{}` names no file", path.display()),
        )
    })?;
    let dir = parent_of(path).unwrap_or(Path::new("."));
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        unique_suffix()
    ));
    create_filled(&temp, bytes)?;
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    #[cfg(unix)]
    let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    Ok(())
}

/// Create `dir` and its missing parents, each one this call creates `0700`
/// on Unix. A directory that exists keeps its mode: it may be shared, or the
/// owner's choice.
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Most links [`resolve_links`] follows, as Linux's `MAXSYMLINKS`.
const MAX_LINKS: usize = 40;

/// The path a write to `path` reaches by following symlinks: each link's
/// target in turn, a relative one taken from the link's directory, until a
/// path that is not a link. That last path need not exist, so a link to a
/// file not yet created (a fresh dotfiles checkout) resolves to where the
/// file goes. A path that is not a link is returned as it is. More than 40
/// links in a chain is `InvalidInput`, which a loop reaches too.
pub fn resolve_links(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..=MAX_LINKS {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
                current = match parent_of(&current) {
                    Some(dir) if target.is_relative() => dir.join(target),
                    _ => target,
                };
            }
            Ok(_) => return Ok(current),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("`{}`: more than {MAX_LINKS} symbolic links", path.display()),
    ))
}

/// The directory holding `path`, `None` for a bare file name.
fn parent_of(path: &Path) -> Option<&Path> {
    path.parent().filter(|dir| !dir.as_os_str().is_empty())
}

/// Unique among this host's live processes and this process's calls, and
/// unlikely to meet a temporary file a crashed process left with the same
/// id: the process id, the clock in nanoseconds and a counter.
fn unique_suffix() -> String {
    static N: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}.{nanos}.{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Create `path` exclusively (`0600`), fill it and sync it to disk; if
/// either fails, remove the file this call created. The sync comes before
/// any rename, so the rename can never reach the disk ahead of the bytes it
/// publishes (`File::sync_all` is `F_FULLFSYNC` on macOS).
fn create_filled(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let filled = {
        let mut file = opts.open(path)?;
        file.write_all(bytes).and_then(|()| file.sync_all())
    };
    if filled.is_err() {
        let _ = fs::remove_file(path);
    }
    filled
}

#[cfg(test)]
mod tests;
