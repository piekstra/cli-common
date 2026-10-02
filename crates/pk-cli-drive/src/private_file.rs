//! Owner-only file output: the one writer for files that hold the owner's
//! data outside the store — the read cache's copies, and the temp file an
//! rclone upload goes through. A CLI can use it for its own exports too.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// What [`write`] does when `path` already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replace {
    /// Fail with `ErrorKind::AlreadyExists`. The create is exclusive, so an
    /// existing path (a symlink included) is never written through.
    Never,
    /// Write a new file beside `path` and rename it over. A reader never
    /// sees a torn file, the mtime moves atomically, and a symlink at
    /// `path` is replaced rather than followed.
    Always,
}

/// Write `bytes` to `path`, readable by its owner only (0600 on Unix). A
/// failed write leaves nothing behind: not a partial file at `path` (which
/// this call created, so it is ours to remove), not a temp file.
pub fn write(path: &Path, bytes: &[u8], replace: Replace) -> io::Result<()> {
    if replace == Replace::Never {
        return create_filled(path, |f| f.write_all(bytes));
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{}` names no file", path.display()),
        )
    })?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}.{nanos}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    create_filled(&tmp, |f| f.write_all(bytes))?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Create `path` exclusively (0600) and fill it; if filling fails, remove
/// the file this call created.
fn create_filled(
    path: &Path,
    fill: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    fill(&mut file).inspect_err(|_| {
        let _ = fs::remove_file(path);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count()
    }

    #[test]
    fn the_file_is_owner_only_and_never_written_through_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.txt");
        write(&out, b"one", Replace::Never).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let err = write(&out, b"two", Replace::Never).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&out).unwrap(), "one");
        write(&out, b"two", Replace::Always).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "a replaced file is owner-only too");
        }

        // A symlink is replaced, never written through.
        #[cfg(unix)]
        {
            let target = dir.path().join("target");
            fs::write(&target, "keep").unwrap();
            let link = dir.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let err = write(&link, b"x", Replace::Never).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
            write(&link, b"x", Replace::Always).unwrap();
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
            assert_eq!(fs::read_to_string(&link).unwrap(), "x");
        }

        let missing = dir.path().join("no-such-dir").join("x.txt");
        assert!(write(&missing, b"x", Replace::Never).is_err());
        assert!(write(&missing, b"x", Replace::Always).is_err());
        assert_eq!(temp_files(dir.path()), 0);
    }

    /// A write that fails part way (disk full, I/O error) leaves no
    /// truncated file behind, so the next run is not refused by a file the
    /// tool itself half-wrote.
    #[test]
    fn a_failed_fill_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x.txt");
        let err = create_filled(&out, |f| {
            f.write_all(b"half")?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert!(!out.exists());
        assert_eq!(temp_files(dir.path()), 0);
    }

    #[test]
    fn a_path_with_no_file_name_is_invalid_input() {
        let err = write(Path::new("/"), b"x", Replace::Always).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
