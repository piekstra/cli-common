//! The on-disk cache for one remote spec: mirrored file contents, listings,
//! the write generation and its lock, and the revalidation markers.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::{digest, Kind};
use crate::backend::FileEntry;
use crate::private_file::{self, Replace};

/// Cached directory listings live under this sibling of the mirrored content.
/// The `-v2` suffix is the listing format: v2 entries carry `modified`, so a
/// listing cached before the field existed is never read back as "no mtime".
const LISTINGS_SUBDIR: &str = ".listings-v2";
/// Listing dirs of earlier formats, dropped whenever listings are invalidated.
const LEGACY_LISTINGS_SUBDIRS: &[&str] = &[".listings"];
/// The lock, the write generation and the revalidation markers.
const CONTROL_SUBDIR: &str = ".swr";

/// The on-disk cache for one remote spec. All plumbing is best-effort: a disk
/// error makes a read miss (and fall through) and a store a no-op.
pub(super) struct CacheDir {
    pub(super) dir: PathBuf,
}

impl CacheDir {
    /// A cached file and its age (`None`: unreadable or future mtime).
    pub(super) fn file(&self, rel: &str) -> Option<(String, Option<Duration>)> {
        let path = self.content_path(rel)?;
        let content = fs::read_to_string(&path).ok()?;
        Some((content, age(&path)))
    }

    pub(super) fn listing(&self, rel: &str) -> Option<(Vec<FileEntry>, Option<Duration>)> {
        let path = self.listing_path(rel)?;
        let entries = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
        Some((entries, age(&path)))
    }

    /// Store (or, for `None`, drop) a fetched file — only if no write bumped
    /// the generation since `generation` was read before the fetch. Returns
    /// whether the cache now reflects the fetch.
    pub(super) fn commit_file(&self, rel: &str, content: Option<&str>, generation: u64) -> bool {
        let Some(_lock) = self.lock() else {
            return false;
        };
        if self.generation() != generation {
            return false;
        }
        match content {
            Some(content) => self.store_file(rel, content),
            None => self.remove_file(rel),
        }
        true
    }

    pub(super) fn commit_listing(&self, rel: &str, entries: &[FileEntry], generation: u64) -> bool {
        let Some(_lock) = self.lock() else {
            return false;
        };
        if self.generation() != generation {
            return false;
        }
        if let (Some(path), Ok(json)) = (self.listing_path(rel), serde_json::to_vec(entries)) {
            let _ = self.write_atomic(&path, &json);
        }
        true
    }

    /// A completed remote write: bump the generation (so any fetch in flight
    /// discards its possibly pre-write result), store or drop each written
    /// path's copy, and drop every cached listing — any write can add, move
    /// or delete a file, and writes are rare next to reads. Without the lock
    /// (a disk error) nothing can be ordered, so every touched copy is
    /// dropped instead of stored.
    pub(super) fn record_write<'a>(
        &self,
        paths: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) {
        let lock = self.lock();
        if lock.is_some() {
            let _ = self.write_atomic(
                &self.control().join("generation"),
                (self.generation().wrapping_add(1)).to_string().as_bytes(),
            );
        }
        for (rel, content) in paths {
            match content {
                Some(content) if lock.is_some() => self.store_file(rel, content),
                _ => self.remove_file(rel),
            }
        }
        for sub in std::iter::once(&LISTINGS_SUBDIR).chain(LEGACY_LISTINGS_SUBDIRS) {
            let _ = fs::remove_dir_all(self.dir.join(sub));
        }
    }

    /// The write generation: bumped by every write through this cache dir.
    pub(super) fn generation(&self) -> u64 {
        fs::read_to_string(self.control().join("generation"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The cache-wide lock that orders a write's generation bump and stores
    /// against a fetch's check-and-store. Held only around local file
    /// operations, never across a network call. Released on drop.
    pub(super) fn lock(&self) -> Option<fs::File> {
        self.secure_base().ok()?;
        fs::create_dir_all(self.control()).ok()?;
        let mut opts = fs::OpenOptions::new();
        opts.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(self.control().join("lock")).ok()?;
        file.lock().ok()?;
        Some(file)
    }

    /// The in-flight marker for one entry's background refresh.
    pub(super) fn marker(&self, kind: Kind, rel: &str) -> Option<PathBuf> {
        Some(self.control().join("revalidating").join(format!(
            "{}-{:016x}",
            kind.as_str(),
            digest(rel.as_bytes())
        )))
    }

    pub(super) fn control(&self) -> PathBuf {
        self.dir.join(CONTROL_SUBDIR)
    }

    pub(super) fn store_file(&self, rel: &str, content: &str) {
        if let Some(path) = self.content_path(rel) {
            let _ = self.write_atomic(&path, content.as_bytes());
        }
    }

    pub(super) fn remove_file(&self, rel: &str) {
        if let Some(path) = self.content_path(rel) {
            let _ = fs::remove_file(path);
        }
    }

    /// Content mirrors the data-root relative path directly under the cache dir.
    pub(super) fn content_path(&self, rel: &str) -> Option<PathBuf> {
        safe_join(&self.dir, rel)
    }

    /// Listings live under `.listings-v2/<rel>.json` (root listing → `_root_`).
    pub(super) fn listing_path(&self, rel: &str) -> Option<PathBuf> {
        let key = if rel.is_empty() { "_root_" } else { rel };
        safe_join(&self.dir.join(LISTINGS_SUBDIR), &format!("{key}.json"))
    }

    pub(super) fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.secure_base()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Temp file + rename: a reader never sees a torn write and the mtime
        // (the TTL clock) advances atomically.
        private_file::write(path, bytes, Replace::Always)
    }

    /// Create the cache dir if needed and lock it to `0700` (per-machine,
    /// sensitive-adjacent — mirrors the data root's own contents).
    pub(super) fn secure_base(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Drops an in-flight marker when the refresh that holds it ends, so the
/// next stale read may start another.
pub(super) struct MarkerGuard(pub(super) Option<PathBuf>);

impl Drop for MarkerGuard {
    fn drop(&mut self) {
        if let Some(marker) = &self.0 {
            let _ = fs::remove_file(marker);
        }
    }
}

/// Claim an entry's revalidation: create its marker exclusively. A marker
/// older than `bound` belongs to a revalidator that died without releasing
/// it, and is taken over.
pub(super) fn claim_marker(marker: &Path, bound: Duration) -> bool {
    let Some(parent) = marker.parent() else {
        return false;
    };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    let create = || {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    };
    if create() {
        return true;
    }
    match age(marker) {
        Some(age) if age <= bound => false,
        _ => {
            let _ = fs::remove_file(marker);
            create()
        }
    }
}

/// Join `rel` under `base`, rejecting anything but plain forward path
/// components — no `..`, absolute paths, or prefixes can escape the cache dir.
pub(super) fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let mut path = base.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => path.push(c),
            _ => return None,
        }
    }
    Some(path)
}

/// How long ago `path` was last written. `None` for a missing file, an
/// unreadable mtime, or a future mtime (clock skew) — all of which the
/// caller treats as too old to serve.
pub(super) fn age(path: &Path) -> Option<Duration> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
}
