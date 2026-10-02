//! A transparent, read-through cache in front of the rclone [`Remote`]
//! backend. Only the remote path is slow — every read shells out to rclone
//! and the provider's API (seconds, sometimes minutes), and an app may fire
//! several reads per view — so this layer accelerates *reads* without ever
//! becoming a second source of truth. A mount is already a local filesystem
//! and needs no cache.
//!
//! Invariants (correctness first — a wrong read is worse than a slow one):
//!
//! - **Remote is authoritative.** The cache only accelerates reads; it can
//!   never contradict a write this process completed.
//! - **Write-through.** Every write hits the REMOTE first ([`Remote::write`],
//!   `copy_in`, `move_to`, `mkdir`). Only on remote success do we touch the
//!   cache — write the new content in, and invalidate cached listings the write
//!   changes. On remote error we propagate and leave the cache untouched.
//!   A `--no-cache` run bypasses reads only ([`CachePolicy::bypass_reads`]);
//!   its writes still update the cache, so a later cached read never serves
//!   the pre-write copy.
//! - **Self-healing / fail-open.** A cache miss, a parse/disk error, or an
//!   entry past the staleness bound falls through to the remote (and refreshes
//!   the cache). A cache problem must never turn a working read into an error.
//! - **Stale-while-revalidate.** An entry within the TTL (default 900s) is
//!   fresh and served as is. An entry past the TTL but within the staleness
//!   bound (default 24h, `<BIN>_CACHE_MAX_STALE`) is served at once, reported
//!   on stderr, and refreshed in the background through a [`Revalidator`] —
//!   so a cold open never blocks on rclone when there is something to show.
//!   Past the bound, or with no copy at all, the read fetches synchronously.
//! - **Writes never derive from an unconfirmed stale read.** Before a write,
//!   every copy this process served stale is re-read from the remote. If one
//!   changed, the write is refused (nothing written, the cache now current),
//!   so a read-modify-write cannot overwrite a newer remote file.
//! - **A write is never undone by a refresh.** Every write bumps a cache-wide
//!   generation under a lock; a fetch (foreground or background) stores its
//!   result only if the generation is unchanged since before it fetched, so a
//!   refresh that read the pre-write remote cannot land after the write.
//! - **Bounded background work.** The revalidator's rclone calls are killed
//!   at [`CachePolicy::revalidate_timeout`] (default 120s), so the child
//!   outlives the command that spawned it by at most that long; a per-entry
//!   marker keeps concurrent reads from spawning duplicates.
//!
//! Location: `$XDG_CACHE_HOME/<bin>/<slug-of-remote-spec>/`, else
//! `~/.cache/<bin>/…` (per-machine, dir `0700`, files `0600`). Distinct
//! remote specs get distinct cache dirs. File contents mirror the store's
//! relative paths; directory listings live under a `.listings-v2/` sibling
//! with their own mtime; the lock, generation and revalidation markers live
//! under `.swr/`.
//!
//! Knobs live in one [`CachePolicy`], resolved once and handed to the cache,
//! its [`background_revalidator`] and the child's [`run_revalidation`].
//! [`CachePolicy::from_env`] reads them from the environment with the
//! binary's prefix ([`env_prefix`]: `example-cli` → `EXAMPLE_CLI`):
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `<BIN>_NO_CACHE` | bypass reads (a `--no-cache` flag sets [`CachePolicy::bypass_reads`] directly) | off |
//! | `<BIN>_CACHE_TTL` | seconds a copy is fresh; `0` makes every read fetch | 900 |
//! | `<BIN>_CACHE_MAX_STALE` | seconds a copy may be served stale; `0` turns stale serving off | 86400 |
//! | `<BIN>_CACHE_REVALIDATE_TIMEOUT` | bound on one background refresh's rclone calls | 120 |
//! | `<BIN>_CACHE_NO_REVALIDATE` | serve stale copies without starting a refresh | off |

mod policy;
mod revalidate;
mod store;

pub use policy::{
    cache_base, cache_dir_for, env_prefix, CachePolicy, DEFAULT_MAX_STALE_SECS,
    DEFAULT_REVALIDATE_TIMEOUT_SECS, DEFAULT_TTL_SECS,
};
pub use revalidate::{
    background_revalidator, run_revalidation, Revalidator, SpawnRevalidator, REMOTE_SPEC_ARG,
    REVALIDATE_FILE_ARG, REVALIDATE_LISTING_ARG,
};

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use pk_cli_core::CliError;

use crate::backend::FileEntry;
use crate::rclone::{Remote, RemoteRead};
use store::{claim_marker, CacheDir, MarkerGuard};

/// How a stale serve's refresh went, for the stderr note.
#[derive(Debug, PartialEq, Eq)]
enum Refresh {
    Started,
    AlreadyRunning,
    CouldNotStart,
    Off,
}

/// What a cache entry holds: one file's text, or one recursive listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Listing,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Listing => "listing",
        }
    }
}

/// How a cached copy of a given age may be used.
#[derive(Debug, PartialEq, Eq)]
enum Freshness {
    /// Within the TTL: serve it.
    Fresh,
    /// Past the TTL, within the staleness bound: serve it, report it, refresh
    /// it in the background.
    Stale,
    /// Too old (or an unreadable/future mtime): fetch synchronously.
    Expired,
}

fn classify(age: Option<Duration>, ttl: Duration, max_stale: Duration) -> Freshness {
    match age {
        _ if ttl.is_zero() => Freshness::Expired,
        Some(age) if age <= ttl => Freshness::Fresh,
        Some(age) if age <= max_stale => Freshness::Stale,
        _ => Freshness::Expired,
    }
}

/// What a forced refresh did to its cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The remote copy is now cached.
    Refreshed,
    /// The file is gone from the remote; its cached copy was dropped.
    Gone,
    /// A write landed while the remote was being read, so the fetched copy
    /// may predate it and was discarded. The write's own copy stands.
    Superseded,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Refreshed => "refreshed",
            Outcome::Gone => "gone",
            Outcome::Superseded => "superseded",
        }
    }
}

/// A copy this process served past its TTL, kept until a write confirms it
/// still matches the remote.
struct StaleRead {
    kind: Kind,
    rel: String,
    digest: u64,
}

/// A read-through cache mirroring the [`Remote`] method surface. A CLI builds
/// one for every remote whose cache dir resolves (with `--no-cache` it only
/// bypasses reads, see [`CachePolicy::bypass_reads`]), and for a cache-warming
/// command. Reads work over any [`RemoteRead`] (unit tests use an in-memory one); writes
/// need the real [`Remote`]. A read may start a background refresh, but only
/// through a [`Revalidator`] handed to [`with_revalidator`](Self::with_revalidator).
///
/// Build one per command. It remembers every copy it served stale until a
/// write confirms them, and a refused write keeps that record, so every
/// later write through the same handle is refused too: the documented
/// remedy is to rerun the command, which builds a new handle over the
/// now-current cache.
pub struct CachedRemote<R: RemoteRead = Remote> {
    remote: R,
    cache: CacheDir,
    policy: CachePolicy,
    revalidator: Option<Box<dyn Revalidator>>,
    stale_reads: Mutex<Vec<StaleRead>>,
}

impl<R: RemoteRead> CachedRemote<R> {
    pub fn new(remote: R, dir: PathBuf, policy: CachePolicy) -> Self {
        Self {
            remote,
            cache: CacheDir { dir },
            policy,
            revalidator: None,
            stale_reads: Mutex::new(Vec::new()),
        }
    }

    /// Refresh copies served stale in the background through `revalidator`.
    /// Without one, stale copies are still served and reported.
    pub fn with_revalidator(mut self, revalidator: Box<dyn Revalidator>) -> Self {
        self.revalidator = Some(revalidator);
        self
    }

    /// The remote target — used for display/error messages, always the
    /// authoritative remote path (never the cache dir).
    pub fn target(&self, rel: &str) -> String {
        self.remote.target(rel)
    }

    // ── reads ────────────────────────────────────────────────────────────

    /// Read file contents. A fresh copy is served locally; a stale one within
    /// the bound is served locally, reported, and refreshed in the
    /// background; otherwise fetch from the remote and refresh the cache. On a
    /// remote *error* with any cached copy present, serve that copy rather
    /// than fail. With `bypass_reads`, straight to the remote.
    pub fn cat(&self, rel: &str) -> Result<Option<String>, CliError> {
        if self.policy.bypass_reads {
            return self.remote.cat(rel);
        }
        if let Some((content, age)) = self.cache.file(rel) {
            match self.freshness(age) {
                Freshness::Fresh => return Ok(Some(content)),
                Freshness::Stale => {
                    self.served_stale(Kind::File, rel, digest(content.as_bytes()), age);
                    return Ok(Some(content));
                }
                Freshness::Expired => {}
            }
        }
        let generation = self.cache.generation();
        match self.remote.cat(rel) {
            Ok(content) => {
                self.cache.commit_file(rel, content.as_deref(), generation);
                Ok(content)
            }
            Err(e) => match self.cache.file(rel) {
                Some((stale, _)) => {
                    warn_unreachable(&self.target(rel), &e);
                    Ok(Some(stale))
                }
                None => Err(e),
            },
        }
    }

    /// Recursive file listing, cached as JSON with its own mtime. Same
    /// freshness tiers and self-healing fallback as [`cat`](Self::cat).
    pub fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
        if self.policy.bypass_reads {
            return self.remote.list_entries(rel);
        }
        if let Some((entries, age)) = self.cache.listing(rel) {
            match self.freshness(age) {
                Freshness::Fresh => return Ok(entries),
                Freshness::Stale => {
                    self.served_stale(Kind::Listing, rel, listing_digest(&entries), age);
                    return Ok(entries);
                }
                Freshness::Expired => {}
            }
        }
        let generation = self.cache.generation();
        match self.remote.list_entries(rel) {
            Ok(entries) => {
                self.cache.commit_listing(rel, &entries, generation);
                Ok(entries)
            }
            Err(e) => match self.cache.listing(rel) {
                Some((stale, _)) => {
                    warn_unreachable(&self.target(rel), &e);
                    Ok(stale)
                }
                None => Err(e),
            },
        }
    }

    // ── forced refresh (a warming command, and the background revalidator) ─

    /// Re-pull one entry from the remote into the cache regardless of age.
    /// The fetched copy is stored only if no write landed while it was being
    /// read ([`Outcome::Superseded`] otherwise).
    pub fn revalidate(&self, kind: Kind, rel: &str) -> Result<Outcome, CliError> {
        let generation = self.cache.generation();
        Ok(match kind {
            Kind::File => {
                let content = self.remote.cat(rel)?;
                match (
                    self.cache.commit_file(rel, content.as_deref(), generation),
                    content,
                ) {
                    (false, _) => Outcome::Superseded,
                    (true, Some(_)) => Outcome::Refreshed,
                    (true, None) => Outcome::Gone,
                }
            }
            Kind::Listing => {
                let entries = self.remote.list_entries(rel)?;
                if self.cache.commit_listing(rel, &entries, generation) {
                    Outcome::Refreshed
                } else {
                    Outcome::Superseded
                }
            }
        })
    }

    /// [`revalidate`](Self::revalidate) a file for a warming command, which
    /// counts the files that exist. Returns whether the remote has the file —
    /// what the remote said, not whether the copy was stored: a
    /// [`Outcome::Superseded`] fetch still found the file.
    pub fn refresh_file(&self, rel: &str) -> Result<bool, CliError> {
        let generation = self.cache.generation();
        let content = self.remote.cat(rel)?;
        self.cache.commit_file(rel, content.as_deref(), generation);
        Ok(content.is_some())
    }

    /// The background refresh's body: [`revalidate`](Self::revalidate) the
    /// entry, then release the in-flight marker its spawner claimed in this
    /// same cache dir, however the refresh ended.
    pub fn revalidate_claimed(&self, kind: Kind, rel: &str) -> Result<Outcome, CliError> {
        let _release = MarkerGuard(self.cache.marker(kind, rel));
        self.revalidate(kind, rel)
    }

    fn freshness(&self, age: Option<Duration>) -> Freshness {
        classify(age, self.policy.ttl, self.policy.max_stale)
    }

    // ── stale serving ────────────────────────────────────────────────────

    /// Record a copy served past its TTL, report it once, and start its
    /// background refresh.
    fn served_stale(&self, kind: Kind, rel: &str, digest: u64, age: Option<Duration>) {
        let mut reads = self.stale_reads.lock().unwrap_or_else(|p| p.into_inner());
        if reads.iter().any(|r| r.kind == kind && r.rel == rel) {
            return;
        }
        reads.push(StaleRead {
            kind,
            rel: rel.to_string(),
            digest,
        });
        drop(reads);
        let refresh = self.start_refresh(kind, rel);
        eprintln!(
            "note: served a cached copy of {} that is {} old (past the {} freshness \
             window); {}",
            self.target(rel),
            human(age.unwrap_or_default()),
            human(self.policy.ttl),
            match refresh {
                Refresh::Started => "refreshing it in the background",
                Refresh::AlreadyRunning => "a background refresh is already running",
                Refresh::CouldNotStart => "the background refresh could not start",
                Refresh::Off => "background refresh is off",
            }
        );
    }

    /// Claim the entry's in-flight marker and start its refresh, unless one
    /// is already running. A failed start releases the marker.
    fn start_refresh(&self, kind: Kind, rel: &str) -> Refresh {
        let Some(revalidator) = &self.revalidator else {
            return Refresh::Off;
        };
        let Some(marker) = self.cache.marker(kind, rel) else {
            return Refresh::CouldNotStart;
        };
        if !claim_marker(
            &marker,
            self.policy.revalidate_timeout + Duration::from_secs(30),
        ) {
            return Refresh::AlreadyRunning;
        }
        if revalidator.start(kind, rel) {
            Refresh::Started
        } else {
            let _ = fs::remove_file(&marker);
            Refresh::CouldNotStart
        }
    }

    /// Before a write: re-read every copy this process served stale. If any
    /// changed on the remote, refuse — the write may derive from the old copy
    /// (a read-modify-write would overwrite the newer file). The re-read
    /// lands in the cache either way, so a rerun sees the current copy.
    ///
    /// The refusal exits 1, not 5: the remote answered, and retrying the
    /// same command later is not the remedy; rerunning it now is.
    fn confirm_stale_reads(&self) -> Result<(), CliError> {
        let mut reads = self.stale_reads.lock().unwrap_or_else(|p| p.into_inner());
        for read in reads.iter() {
            let generation = self.cache.generation();
            let current = match read.kind {
                Kind::File => {
                    let content = self.remote.cat(&read.rel)?;
                    self.cache
                        .commit_file(&read.rel, content.as_deref(), generation);
                    content.map(|c| digest(c.as_bytes()))
                }
                Kind::Listing => {
                    let entries = self.remote.list_entries(&read.rel)?;
                    self.cache.commit_listing(&read.rel, &entries, generation);
                    Some(listing_digest(&entries))
                }
            };
            if current != Some(read.digest) {
                return Err(CliError::Other(format!(
                    "{} {STALE_WRITE_REFUSED} The cache now holds the current copy — rerun \
                     the command.",
                    self.target(&read.rel)
                )));
            }
        }
        reads.clear();
        Ok(())
    }

    /// Record a completed remote write in the cache. With `bypass_reads` and
    /// no cache dir yet there is nothing a later read could serve, so none
    /// is created.
    fn written<'a>(&self, paths: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) {
        if self.policy.bypass_reads && !self.cache.dir.is_dir() {
            return;
        }
        self.cache.record_write(paths);
    }
}

impl CachedRemote<Remote> {
    /// The remote behind the cache — for a document download, which the
    /// cache never holds (it caches small state files, not documents).
    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    /// A fresh cached copy proves existence; otherwise ask the remote directly
    /// (always correct — never cached as a negative).
    pub fn exists(&self, rel: &str) -> Result<bool, CliError> {
        if !self.policy.bypass_reads {
            if let Some((_, age)) = self.cache.file(rel) {
                if self.freshness(age) == Freshness::Fresh {
                    return Ok(true);
                }
            }
        }
        self.remote.exists(rel)
    }

    // ── writes (write-through) ──────────────────────────────────────────

    pub fn write(&self, rel: &str, content: &str) -> Result<(), CliError> {
        self.confirm_stale_reads()?;
        self.remote.write(rel, content)?;
        self.written([(rel, Some(content))]);
        Ok(())
    }

    /// A binary upload: we never read documents back through [`cat`](Self::cat),
    /// so don't slurp the bytes — just drop any stale text cache for the target
    /// and invalidate listings so the new file shows up next `list`.
    pub fn copy_in(&self, local: &Path, rel: &str) -> Result<(), CliError> {
        self.confirm_stale_reads()?;
        self.remote.copy_in(local, rel)?;
        self.written([(rel, None)]);
        Ok(())
    }

    pub fn move_to(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        self.confirm_stale_reads()?;
        self.remote.move_to(rel_from, rel_to)?;
        self.written([(rel_from, None), (rel_to, None)]);
        Ok(())
    }

    /// The rename path's move ([`Remote::move_no_clobber`]). A refusal moved
    /// nothing, so the cache is touched only on success.
    pub fn move_no_clobber(&self, rel_from: &str, rel_to: &str) -> Result<(), CliError> {
        self.confirm_stale_reads()?;
        self.remote.move_no_clobber(rel_from, rel_to)?;
        self.written([(rel_from, None), (rel_to, None)]);
        Ok(())
    }

    pub fn mkdir(&self, rel: &str) -> Result<(), CliError> {
        self.remote.mkdir(rel)?;
        self.written([]);
        Ok(())
    }
}

pub(crate) fn digest(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn listing_digest(entries: &[FileEntry]) -> u64 {
    digest(&serde_json::to_vec(entries).unwrap_or_default())
}

/// A whole-unit age for the stale note: `45s`, `12m`, `3h12m`, `2d4h`.
fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => match (s / 3600, s % 3600 / 60) {
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h{m}m"),
        },
        _ => match (s / 86400, s % 86400 / 3600) {
            (d, 0) => format!("{d}d"),
            (d, h) => format!("{d}d{h}h"),
        },
    }
}

/// The fixed part of the stale-write refusal, which [`is_stale_write_refusal`]
/// recognizes.
const STALE_WRITE_REFUSED: &str = "changed on the remote after this command read an older \
                                   cached copy of it, so the write was refused.";

/// Is `err` the refusal of a write that would have derived from a copy served
/// stale that has since changed on the remote? It is exit 1, like any other
/// generic error, so a CLI that renders it differently, or reruns the command
/// on it, asks here instead of matching the text. Nothing was written, and
/// the cache now holds the current copy.
pub fn is_stale_write_refusal(err: &CliError) -> bool {
    matches!(err, CliError::Other(message) if message.contains(STALE_WRITE_REFUSED))
}

/// The source of truth was unreachable but a cached copy exists — surface that
/// on stderr (never stdout, so `--json` stays clean) rather than fail the read.
fn warn_unreachable(target: &str, err: &CliError) {
    eprintln!(
        "warning: remote read failed ({err}); serving cached copy of {target} (may be stale)"
    );
}

#[cfg(test)]
mod tests {
    use super::policy::{dir_name, is_truthy, parse_secs, slug, var};
    use super::store::safe_join;
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn distinct_specs_get_distinct_dirs() {
        let a = dir_name("example:State Files");
        let b = dir_name("other:State Files");
        assert_ne!(a, b);
        // Deterministic across calls (stable keying is what makes the cache
        // survive process restarts).
        assert_eq!(a, dir_name("example:State Files"));
    }

    /// The dir name is part of the on-disk contract: a CLI that moves its
    /// cache onto this crate keeps finding the copies it already has only
    /// while the slug and the hash stay exactly this.
    #[test]
    fn the_cache_dir_name_is_pinned() {
        assert_eq!(
            dir_name("example:State Files"),
            "example-state-files-a34a6f91a040ea68"
        );
    }

    #[test]
    fn the_env_prefix_and_cache_root_follow_the_binary_name() {
        assert_eq!(env_prefix("example-cli"), "EXAMPLE_CLI");
        assert_eq!(env_prefix("tool"), "TOOL");
        assert_eq!(var("example-cli", "CACHE_TTL"), "EXAMPLE_CLI_CACHE_TTL");
    }

    #[test]
    fn slug_is_readable_and_specs_that_slug_alike_still_differ() {
        assert_eq!(slug("example:State Files"), "example-state-files");
        // `a:b` and `a-b` slug identically but must not share a cache dir.
        assert_eq!(slug("a:b"), slug("a-b"));
        assert_ne!(dir_name("a:b"), dir_name("a-b"));
    }

    #[test]
    fn safe_join_blocks_traversal() {
        let base = Path::new("/cache/x");
        assert_eq!(
            safe_join(base, "a/b.json"),
            Some(PathBuf::from("/cache/x/a/b.json"))
        );
        assert_eq!(safe_join(base, "../escape"), None);
        assert_eq!(safe_join(base, "/abs"), None);
        assert_eq!(safe_join(base, "a/../../escape"), None);
        assert_eq!(safe_join(base, ""), None);
    }

    #[test]
    fn freshness_tiers() {
        let (ttl, bound) = (Duration::from_secs(900), Duration::from_secs(86_400));
        let at = |s| classify(Some(Duration::from_secs(s)), ttl, bound);
        assert_eq!(at(0), Freshness::Fresh);
        assert_eq!(at(900), Freshness::Fresh);
        assert_eq!(at(901), Freshness::Stale);
        assert_eq!(at(86_400), Freshness::Stale);
        assert_eq!(at(86_401), Freshness::Expired);
        // An unreadable or future mtime is never served.
        assert_eq!(classify(None, ttl, bound), Freshness::Expired);
        // A zero TTL is "never fresh" AND never served stale.
        assert_eq!(
            classify(Some(Duration::ZERO), Duration::ZERO, bound),
            Freshness::Expired
        );
        // A zero bound turns stale serving off.
        assert_eq!(
            classify(Some(Duration::from_secs(901)), ttl, Duration::ZERO),
            Freshness::Expired
        );
    }

    #[test]
    fn env_seconds_parse_and_fall_back() {
        assert_eq!(parse_secs(Some(" 60 ")), Some(60));
        assert_eq!(parse_secs(Some("0")), Some(0));
        assert_eq!(parse_secs(Some("soon")), None);
        assert_eq!(parse_secs(None), None);
        assert_eq!(DEFAULT_TTL_SECS, 900);
        assert_eq!(DEFAULT_MAX_STALE_SECS, 86_400);
    }

    #[test]
    fn human_ages() {
        let h = |s| human(Duration::from_secs(s));
        assert_eq!(h(45), "45s");
        assert_eq!(h(12 * 60 + 5), "12m");
        assert_eq!(h(3 * 3600), "3h");
        assert_eq!(h(3 * 3600 + 12 * 60), "3h12m");
        assert_eq!(h(2 * 86_400 + 4 * 3600), "2d4h");
    }

    /// An in-memory remote that counts its calls.
    #[derive(Default)]
    struct FakeRemote {
        files: RefCell<HashMap<String, String>>,
        cats: Cell<usize>,
        lists: Cell<usize>,
    }

    impl FakeRemote {
        fn put(&self, rel: &str, content: &str) {
            self.files
                .borrow_mut()
                .insert(rel.to_string(), content.to_string());
        }
    }

    impl RemoteRead for FakeRemote {
        fn target(&self, rel: &str) -> String {
            format!("fake:{rel}")
        }
        fn cat(&self, rel: &str) -> Result<Option<String>, CliError> {
            self.cats.set(self.cats.get() + 1);
            Ok(self.files.borrow().get(rel).cloned())
        }
        fn list_entries(&self, rel: &str) -> Result<Vec<FileEntry>, CliError> {
            self.lists.set(self.lists.get() + 1);
            let prefix = format!("{rel}/");
            let mut out: Vec<FileEntry> = self
                .files
                .borrow()
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix(&prefix).map(|r| FileEntry {
                        rel: r.to_string(),
                        size: Some(v.len() as u64),
                        modified: None,
                    })
                })
                .collect();
            out.sort_by(|a, b| a.rel.cmp(&b.rel));
            Ok(out)
        }
    }

    const POLICY: CachePolicy = CachePolicy {
        ttl: Duration::from_secs(900),
        max_stale: Duration::from_secs(86_400),
        revalidate_timeout: Duration::from_secs(120),
        bypass_reads: false,
        background_refresh: true,
    };

    #[test]
    fn the_default_policy_is_the_documented_defaults() {
        assert_eq!(CachePolicy::default(), POLICY);
    }

    /// The policy the parent's cache uses is the one that decides whether a
    /// refresh is spawned at all.
    #[test]
    fn no_background_refresh_when_the_policy_says_so() {
        let off = CachePolicy {
            background_refresh: false,
            ..POLICY
        };
        assert!(background_revalidator(&off, &["sync"], "ex:S").is_none());
        let bypass = CachePolicy {
            bypass_reads: true,
            ..POLICY
        };
        assert!(background_revalidator(&bypass, &["sync"], "ex:S").is_none());
        assert!(background_revalidator(&POLICY, &["sync"], "ex:S").is_some());
    }

    #[test]
    fn a_spawned_refresh_names_the_entry_and_the_spec() {
        let r = SpawnRevalidator::new(PathBuf::from("/bin/example-cli"), &["sync"], "ex:S");
        assert_eq!(
            r.args(Kind::File, "a/b.json"),
            vec!["sync", "--revalidate-file=a/b.json", "--remote-spec=ex:S"]
        );
        assert_eq!(
            r.args(Kind::Listing, "2026"),
            vec!["sync", "--revalidate-listing=2026", "--remote-spec=ex:S"]
        );
    }

    fn cached(dir: &Path) -> CachedRemote<FakeRemote> {
        CachedRemote::new(FakeRemote::default(), dir.to_path_buf(), POLICY)
    }

    /// The refreshes a [`Recorder`] was asked to start.
    type Started = Arc<Mutex<Vec<(Kind, String)>>>;

    /// Records each refresh requested; starts them only when `ok`.
    struct Recorder {
        started: Started,
        ok: bool,
    }

    impl Revalidator for Recorder {
        fn start(&self, kind: Kind, rel: &str) -> bool {
            self.started.lock().unwrap().push((kind, rel.to_string()));
            self.ok
        }
    }

    fn recording(dir: &Path, ok: bool) -> (CachedRemote<FakeRemote>, Started) {
        let started = Arc::new(Mutex::new(Vec::new()));
        let c = cached(dir).with_revalidator(Box::new(Recorder {
            started: started.clone(),
            ok,
        }));
        (c, started)
    }

    /// A stale serve claims the entry's marker and requests exactly one
    /// refresh; while the marker is held no second refresh starts; the
    /// refresh's end releases the marker.
    #[test]
    fn a_stale_serve_starts_one_refresh_and_its_end_releases_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let (c, started) = recording(tmp.path(), true);
        c.remote.put("a.json", "v1");
        c.cat("a.json").unwrap();
        backdate(&tmp.path().join("a.json"), 3600);

        c.cat("a.json").unwrap();
        assert_eq!(
            *started.lock().unwrap(),
            vec![(Kind::File, "a.json".to_string())]
        );
        let marker = c.cache.marker(Kind::File, "a.json").unwrap();
        assert!(marker.is_file(), "the spawner claims the marker");

        // Another process reads the same stale copy: the refresh is running.
        let (other, other_started) = recording(tmp.path(), true);
        other.remote.put("a.json", "v1");
        assert_eq!(
            other.start_refresh(Kind::File, "a.json"),
            Refresh::AlreadyRunning
        );
        assert!(other_started.lock().unwrap().is_empty());

        // The refresh itself (the `--revalidate-file` child's body).
        c.remote.put("a.json", "v2");
        assert_eq!(
            c.revalidate_claimed(Kind::File, "a.json").unwrap(),
            Outcome::Refreshed
        );
        assert!(!marker.exists(), "the refresh releases the marker");
        let cats = c.remote.cats.get();
        assert_eq!(c.cat("a.json").unwrap().as_deref(), Some("v2"));
        assert_eq!(c.remote.cats.get(), cats);
    }

    #[test]
    fn a_refresh_that_cannot_start_releases_its_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let (c, started) = recording(tmp.path(), false);
        assert_eq!(
            c.start_refresh(Kind::Listing, "2026"),
            Refresh::CouldNotStart
        );
        assert_eq!(started.lock().unwrap().len(), 1);
        assert!(!c.cache.marker(Kind::Listing, "2026").unwrap().exists());
        // No revalidator: stale copies are still served, nothing starts.
        assert_eq!(
            cached(tmp.path()).start_refresh(Kind::File, "x"),
            Refresh::Off
        );
    }

    /// `--no-cache`: reads go to the remote and store nothing.
    #[test]
    fn bypassed_reads_hit_the_remote_and_store_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CachedRemote::new(
            FakeRemote::default(),
            tmp.path().to_path_buf(),
            CachePolicy {
                bypass_reads: true,
                ..POLICY
            },
        );
        c.remote.put("a.json", "v1");
        c.cat("a.json").unwrap();
        c.cat("a.json").unwrap();
        assert_eq!(c.remote.cats.get(), 2);
        assert!(!tmp.path().join("a.json").exists());
    }

    /// Set a file's mtime `secs` into the past.
    fn backdate(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() - Duration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn a_stale_copy_is_served_without_touching_the_remote_and_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cached(tmp.path());
        c.remote.put("a.json", "v1");
        assert_eq!(c.cat("a.json").unwrap().as_deref(), Some("v1"));
        assert_eq!(c.remote.cats.get(), 1, "a cold read fetches");

        // Fresh: served locally.
        assert_eq!(c.cat("a.json").unwrap().as_deref(), Some("v1"));
        assert_eq!(c.remote.cats.get(), 1);

        // Past the TTL, within the bound: the old copy, still no remote call,
        // and the serve is recorded for the write guard.
        c.remote.put("a.json", "v2");
        backdate(&tmp.path().join("a.json"), 2 * 3600);
        assert_eq!(c.cat("a.json").unwrap().as_deref(), Some("v1"));
        assert_eq!(
            c.remote.cats.get(),
            1,
            "a stale serve must not block on rclone"
        );
        assert_eq!(c.stale_reads.lock().unwrap().len(), 1);

        // Past the bound: fetched synchronously.
        backdate(&tmp.path().join("a.json"), 2 * 86_400);
        assert_eq!(c.cat("a.json").unwrap().as_deref(), Some("v2"));
        assert_eq!(c.remote.cats.get(), 2);
    }

    #[test]
    fn a_stale_listing_is_served_and_a_revalidation_refreshes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cached(tmp.path());
        c.remote.put("2026/a.pdf", "x");
        assert_eq!(c.list_entries("2026").unwrap().len(), 1);
        c.remote.put("2026/b.pdf", "y");
        backdate(&tmp.path().join(".listings-v2/2026.json"), 3600);
        assert_eq!(c.list_entries("2026").unwrap().len(), 1);
        assert_eq!(c.remote.lists.get(), 1);

        assert_eq!(
            c.revalidate(Kind::Listing, "2026").unwrap(),
            Outcome::Refreshed
        );
        assert_eq!(c.list_entries("2026").unwrap().len(), 2);
        assert_eq!(c.remote.lists.get(), 2, "the refreshed listing is fresh");
    }

    /// The in-flight race: a background refresh reads the remote, a write
    /// lands, then the refresh tries to store what it read. The write bumped
    /// the generation, so the pre-write copy is discarded and the next read
    /// serves the written copy.
    #[test]
    fn a_refresh_that_read_before_a_write_cannot_overwrite_it() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cached(tmp.path());
        c.remote.put("state.json", "as_of 2026-09-30");
        c.cat("state.json").unwrap();

        let generation = c.cache.generation();
        let fetched = c.remote.cat("state.json").unwrap();
        // The write: remote first, then the cache.
        c.remote.put("state.json", "as_of 2026-10-01");
        c.cache
            .record_write([("state.json", Some("as_of 2026-10-01"))]);
        assert!(!c
            .cache
            .commit_file("state.json", fetched.as_deref(), generation));

        let cats = c.remote.cats.get();
        assert_eq!(
            c.cat("state.json").unwrap().as_deref(),
            Some("as_of 2026-10-01")
        );
        assert_eq!(c.remote.cats.get(), cats, "served from the written copy");
    }

    #[test]
    fn a_write_after_a_stale_read_is_refused_only_when_the_remote_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cached(tmp.path());
        c.remote.put("notes.md", "one");
        c.cat("notes.md").unwrap();
        backdate(&tmp.path().join("notes.md"), 3600);

        // Unchanged remote: the stale read is confirmed, the write may go.
        assert_eq!(c.cat("notes.md").unwrap().as_deref(), Some("one"));
        c.confirm_stale_reads().unwrap();
        assert!(c.stale_reads.lock().unwrap().is_empty());

        // Changed remote: refused, and the cache now holds the current copy.
        backdate(&tmp.path().join("notes.md"), 3600);
        c.remote.put("notes.md", "one\ntwo");
        assert_eq!(c.cat("notes.md").unwrap().as_deref(), Some("one"));
        let err = c.confirm_stale_reads().unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(is_stale_write_refusal(&err), "{err}");
        // The exact text is a contract: adopters document it.
        assert!(
            matches!(&err, CliError::Other(m) if m == "fake:notes.md changed on the remote after \
                this command read an older cached copy of it, so the write was refused. The cache \
                now holds the current copy — rerun the command."),
            "{err:?}"
        );
        assert!(!is_stale_write_refusal(&CliError::Other(
            "disk full".into()
        )));
        let cats = c.remote.cats.get();
        assert_eq!(c.cat("notes.md").unwrap().as_deref(), Some("one\ntwo"));
        assert_eq!(c.remote.cats.get(), cats);
    }

    #[test]
    fn a_marker_is_claimed_once_and_a_dead_one_is_taken_over() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("m/file-1");
        let bound = Duration::from_secs(60);
        assert!(claim_marker(&marker, bound));
        assert!(!claim_marker(&marker, bound), "one refresh per entry");
        backdate(&marker, 120);
        assert!(claim_marker(&marker, bound), "a dead revalidator's marker");
    }

    #[test]
    fn truthy_semantics() {
        assert!(is_truthy(Some("1")));
        assert!(is_truthy(Some(" yes ")));
        assert!(!is_truthy(Some("0")));
        assert!(!is_truthy(Some("false")));
        assert!(!is_truthy(Some("NO")));
        assert!(!is_truthy(Some("")));
        assert!(!is_truthy(None));
    }

    /// Write-through over a real [`Remote`] (a stub rclone): the write lands
    /// on the remote, then in the cache, and the next read is local.
    #[cfg(unix)]
    #[test]
    fn a_write_goes_to_the_remote_first_then_the_cache() {
        let stub = crate::stub::Stub::backed();
        let cache = tempfile::tempdir().unwrap();
        let c = CachedRemote::new(stub.remote(), cache.path().to_path_buf(), POLICY);
        c.write("notes.md", "one").unwrap();
        assert_eq!(fs::read_to_string(stub.file("notes.md")).unwrap(), "one");
        let calls = stub.calls().len();
        assert_eq!(c.cat("notes.md").unwrap().as_deref(), Some("one"));
        assert_eq!(stub.calls().len(), calls, "served from the written copy");
        assert!(c.exists("notes.md").unwrap());
        assert_eq!(stub.calls().len(), calls, "a fresh copy proves existence");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_remote_write_leaves_the_cache_untouched() {
        let stub = crate::stub::Stub::new("echo 'quota exceeded' >&2\nexit 1");
        let cache = tempfile::tempdir().unwrap();
        let c = CachedRemote::new(stub.remote(), cache.path().to_path_buf(), POLICY);
        assert_eq!(c.write("notes.md", "one").unwrap_err().exit_code(), 5);
        assert!(!cache.path().join("notes.md").exists());
        assert!(!cache.path().join(".swr/generation").exists());
    }

    /// The read-modify-write guard end to end: a copy served stale, another
    /// writer changes the remote, and this command's write is refused with
    /// the remote's newer file intact.
    #[cfg(unix)]
    #[test]
    fn a_write_derived_from_a_stale_copy_that_changed_is_refused() {
        let stub = crate::stub::Stub::backed();
        let cache = tempfile::tempdir().unwrap();
        let c = CachedRemote::new(stub.remote(), cache.path().to_path_buf(), POLICY);
        c.write("notes.md", "one").unwrap();
        backdate(&cache.path().join("notes.md"), 3600);
        assert_eq!(c.cat("notes.md").unwrap().as_deref(), Some("one"));

        fs::write(stub.file("notes.md"), "one\ntwo").unwrap();
        let err = c.write("notes.md", "one\nthree").unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("example:State/notes.md"), "{err}");
        assert_eq!(
            fs::read_to_string(stub.file("notes.md")).unwrap(),
            "one\ntwo"
        );
        // This command stays refused: the remedy is a rerun, which is a new
        // command with a new cache handle. Its read is current and fresh.
        assert_eq!(c.write("notes.md", "x").unwrap_err().exit_code(), 1);
        let rerun = CachedRemote::new(stub.remote(), cache.path().to_path_buf(), POLICY);
        let calls = stub.calls().len();
        assert_eq!(rerun.cat("notes.md").unwrap().as_deref(), Some("one\ntwo"));
        assert_eq!(
            stub.calls().len(),
            calls,
            "the refusal left the cache current"
        );
        rerun.write("notes.md", "one\ntwo\nthree").unwrap();
        assert_eq!(
            fs::read_to_string(stub.file("notes.md")).unwrap(),
            "one\ntwo\nthree"
        );
    }

    #[test]
    fn a_cache_dir_is_owner_only_and_its_files_too() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("c");
        let c = cached(&dir);
        c.remote.put("a/b.json", "{}");
        c.cat("a/b.json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("a/b.json")), 0o600);
        }
    }
}
