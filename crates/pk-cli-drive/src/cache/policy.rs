//! How a cache may use its copies, and where it keeps them: the
//! `<BIN>_CACHE_*` knobs and the cache directory's location and keying.

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

/// Default read-cache TTL. Bounds how long an *external* change (an edit made
/// directly in the provider's UI or on another machine) can be served without
/// a note — local writes are write-through, so they are never stale. 15
/// minutes keeps a working session fast.
pub const DEFAULT_TTL_SECS: u64 = 900;
/// Default staleness bound: the oldest copy served immediately (with a stderr
/// note and a background refresh) instead of blocking on rclone. A day covers
/// an overnight idle; anything older is fetched synchronously.
pub const DEFAULT_MAX_STALE_SECS: u64 = 24 * 60 * 60;
/// Default bound on one background revalidation's rclone calls.
pub const DEFAULT_REVALIDATE_TIMEOUT_SECS: u64 = 120;

/// The environment-variable prefix for `bin`: uppercased, `-` → `_`
/// (`example-cli` → `EXAMPLE_CLI`), per the family's flag > env > config rule.
pub fn env_prefix(bin: &str) -> String {
    bin.chars()
        .map(|c| match c {
            '-' | '.' | ' ' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

pub(crate) fn var(bin: &str, suffix: &str) -> String {
    format!("{}_{suffix}", env_prefix(bin))
}

fn secs_env(key: &str, default: u64) -> Duration {
    Duration::from_secs(parse_secs(env::var(key).ok().as_deref()).unwrap_or(default))
}

pub(crate) fn parse_secs(value: Option<&str>) -> Option<u64> {
    value?.trim().parse::<u64>().ok()
}

fn truthy(key: &str) -> bool {
    is_truthy(env::var(key).ok().as_deref())
}

/// The flag parser behind [`truthy`], over the variable's value (`None`:
/// unset) so the semantics are testable without touching the process env.
pub(crate) fn is_truthy(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim();
            !v.is_empty()
                && v != "0"
                && !v.eq_ignore_ascii_case("false")
                && !v.eq_ignore_ascii_case("no")
        }
        None => false,
    }
}

/// The cache directory for `bin`'s cache over a remote spec, or `None` when
/// no home/cache base can be resolved (caching then simply stays off —
/// fail-open). Keyed on the spec trimmed like [`crate::Remote::new`] trims
/// it, so `remote:State` and `remote:State/` (one remote) share a dir.
pub fn cache_dir_for(bin: &str, spec: &str) -> Option<PathBuf> {
    Some(cache_base(bin)?.join(dir_name(spec.trim_end_matches('/'))))
}

/// The per-machine cache root for `bin`: `$XDG_CACHE_HOME/<bin>`, else
/// `~/.cache/<bin>`; `None` when neither resolves. A CLI can keep its other
/// caches under it too.
pub fn cache_base(bin: &str) -> Option<PathBuf> {
    let base = if let Some(xdg) = env::var("XDG_CACHE_HOME").ok().filter(|s| !s.is_empty()) {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(env::var("HOME").ok().filter(|s| !s.is_empty())?).join(".cache")
    };
    Some(base.join(bin))
}

/// A unique, human-recognizable directory name for a remote spec. The slug is
/// readable; the hash suffix guarantees two distinct specs never collide onto
/// one cache dir (which would serve one remote's data for another).
pub(crate) fn dir_name(spec: &str) -> String {
    let mut h = DefaultHasher::new();
    spec.hash(&mut h);
    format!("{}-{:016x}", slug(spec), h.finish())
}

/// Lowercase alphanumerics; every other run collapses to a single `-`.
pub(crate) fn slug(spec: &str) -> String {
    let mut out = String::with_capacity(spec.len());
    let mut dash = false;
    for c in spec.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    // Keep the readable prefix bounded; uniqueness comes from the hash suffix.
    trimmed.chars().take(40).collect()
}

/// How a [`super::CachedRemote`] may use its copies, and whether it refreshes
/// them in the background. Resolved once, where the backend is built, and
/// passed to everything that needs it — the parent's cache, its
/// [`super::background_revalidator`] and the child's
/// [`super::run_revalidation`] — so all three agree.
/// [`CachePolicy::from_env`] reads the documented variables; a CLI that keeps
/// these in its config builds the struct itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Copies younger than this are fresh.
    pub ttl: Duration,
    /// Copies past the TTL but younger than this are served stale.
    pub max_stale: Duration,
    /// The bound on one background refresh's rclone calls. A refresh's
    /// in-flight marker older than this plus 30s belongs to a refresh that
    /// died, and is taken over.
    pub revalidate_timeout: Duration,
    /// `--no-cache`: every read goes to the remote and nothing read is
    /// stored, but writes still update the cache, so a later cached read
    /// never serves the pre-write copy. No background refresh starts.
    pub bypass_reads: bool,
    /// Refresh a copy served stale in the background. Off: stale copies are
    /// still served and reported, and nothing is spawned.
    pub background_refresh: bool,
}

impl Default for CachePolicy {
    /// The documented defaults, cache and background refresh on.
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(DEFAULT_TTL_SECS),
            max_stale: Duration::from_secs(DEFAULT_MAX_STALE_SECS),
            revalidate_timeout: Duration::from_secs(DEFAULT_REVALIDATE_TIMEOUT_SECS),
            bypass_reads: false,
            background_refresh: true,
        }
    }
}

impl CachePolicy {
    /// The policy `bin`'s variables describe: `<BIN>_CACHE_TTL`,
    /// `<BIN>_CACHE_MAX_STALE`, `<BIN>_CACHE_REVALIDATE_TIMEOUT` (whole
    /// seconds; unparseable values fall back to the default),
    /// `<BIN>_NO_CACHE` and `<BIN>_CACHE_NO_REVALIDATE` (any value but empty,
    /// `0`, `false` or `no`).
    pub fn from_env(bin: &str) -> Self {
        Self {
            ttl: secs_env(&var(bin, "CACHE_TTL"), DEFAULT_TTL_SECS),
            max_stale: secs_env(&var(bin, "CACHE_MAX_STALE"), DEFAULT_MAX_STALE_SECS),
            revalidate_timeout: secs_env(
                &var(bin, "CACHE_REVALIDATE_TIMEOUT"),
                DEFAULT_REVALIDATE_TIMEOUT_SECS,
            ),
            bypass_reads: truthy(&var(bin, "NO_CACHE")),
            background_refresh: !truthy(&var(bin, "CACHE_NO_REVALIDATE")),
        }
    }
}
