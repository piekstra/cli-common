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
//! Knobs, all read from the environment with the binary's prefix
//! ([`env_prefix`]: `example-cli` → `EXAMPLE_CLI`):
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `<BIN>_NO_CACHE` | bypass reads (what a `--no-cache` flag exports) | off |
//! | `<BIN>_CACHE_TTL` | seconds a copy is fresh; `0` makes every read fetch | 900 |
//! | `<BIN>_CACHE_MAX_STALE` | seconds a copy may be served stale; `0` turns stale serving off | 86400 |
//! | `<BIN>_CACHE_REVALIDATE_TIMEOUT` | bound on one background refresh's rclone calls | 120 |
//! | `<BIN>_CACHE_NO_REVALIDATE` | serve stale copies without starting a refresh | off |

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use std::{env, fs};

use pk_cli_core::CliError;

use crate::backend::FileEntry;
use crate::private_file::{self, Replace};
use crate::rclone::{Remote, RemoteRead};

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
/// The argument a [`SpawnRevalidator`] passes to refresh one file:
/// `--revalidate-file=<rel>`. The CLI's command accepts it (hidden) and calls
/// [`run_revalidation`].
pub const REVALIDATE_FILE_ARG: &str = "--revalidate-file";
/// `--revalidate-listing=<rel>`: refresh one recursive listing.
pub const REVALIDATE_LISTING_ARG: &str = "--revalidate-listing";
/// `--remote-spec=<spec>`: the spec the spawning command read through, so the
/// refresh lands in the same cache dir even if the config changed since.
pub const REMOTE_SPEC_ARG: &str = "--remote-spec";
/// Cached directory listings live under this sibling of the mirrored content.
/// The `-v2` suffix is the listing format: v2 entries carry `modified`, so a
/// listing cached before the field existed is never read back as "no mtime".
const LISTINGS_SUBDIR: &str = ".listings-v2";
/// Listing dirs of earlier formats, dropped whenever listings are invalidated.
const LEGACY_LISTINGS_SUBDIRS: &[&str] = &[".listings"];
/// The lock, the write generation and the revalidation markers.
const CONTROL_SUBDIR: &str = ".swr";

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

fn var(bin: &str, suffix: &str) -> String {
    format!("{}_{suffix}", env_prefix(bin))
}

/// Is caching disabled for this run (`<BIN>_NO_CACHE`)? A CLI's `--no-cache`
/// global flag exports it, so a child process inherits the choice.
pub fn disabled(bin: &str) -> bool {
    truthy(&var(bin, "NO_CACHE"))
}

/// Effective read TTL (`<BIN>_CACHE_TTL`, whole seconds). `0` means "never
/// fresh": every read falls through to the remote and no copy is served
/// stale.
pub fn ttl(bin: &str) -> Duration {
    secs_env(&var(bin, "CACHE_TTL"), DEFAULT_TTL_SECS)
}

/// Effective staleness bound (`<BIN>_CACHE_MAX_STALE`, whole seconds; `0`
/// off).
pub fn max_stale(bin: &str) -> Duration {
    secs_env(&var(bin, "CACHE_MAX_STALE"), DEFAULT_MAX_STALE_SECS)
}

/// Effective bound on a background revalidation
/// (`<BIN>_CACHE_REVALIDATE_TIMEOUT`, whole seconds).
pub fn revalidate_timeout(bin: &str) -> Duration {
    secs_env(
        &var(bin, "CACHE_REVALIDATE_TIMEOUT"),
        DEFAULT_REVALIDATE_TIMEOUT_SECS,
    )
}

fn secs_env(key: &str, default: u64) -> Duration {
    Duration::from_secs(parse_secs(env::var(key).ok().as_deref()).unwrap_or(default))
}

fn parse_secs(value: Option<&str>) -> Option<u64> {
    value?.trim().parse::<u64>().ok()
}

/// The cache directory for `bin`'s cache over a remote spec, or `None` when
/// no home/cache base can be resolved (caching then simply stays off —
/// fail-open). Keyed on the spec trimmed like [`Remote::new`] trims it, so
/// `remote:State` and `remote:State/` (one remote) share a dir.
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
fn dir_name(spec: &str) -> String {
    let mut h = DefaultHasher::new();
    spec.hash(&mut h);
    format!("{}-{:016x}", slug(spec), h.finish())
}

/// Lowercase alphanumerics; every other run collapses to a single `-`.
fn slug(spec: &str) -> String {
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

fn truthy(key: &str) -> bool {
    is_truthy(env::var(key).ok().as_deref())
}

/// The flag parser behind [`truthy`], over the variable's value (`None`:
/// unset) so the semantics are testable without touching the process env.
fn is_truthy(value: Option<&str>) -> bool {
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

/// How a [`CachedRemote`] may use its copies. Resolved once, where the
/// backend is built ([`CachePolicy::from_env`]), and passed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Copies younger than this are fresh.
    pub ttl: Duration,
    /// Copies past the TTL but younger than this are served stale.
    pub max_stale: Duration,
    /// The bound on one background refresh. A refresh's in-flight marker
    /// older than this plus 30s belongs to a refresh that died, and is taken
    /// over.
    pub revalidate_timeout: Duration,
    /// `--no-cache`: every read goes to the remote and nothing read is
    /// stored, but writes still update the cache, so a later cached read
    /// never serves the pre-write copy.
    pub bypass_reads: bool,
}

impl Default for CachePolicy {
    /// The documented defaults, cache on.
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(DEFAULT_TTL_SECS),
            max_stale: Duration::from_secs(DEFAULT_MAX_STALE_SECS),
            revalidate_timeout: Duration::from_secs(DEFAULT_REVALIDATE_TIMEOUT_SECS),
            bypass_reads: false,
        }
    }
}

impl CachePolicy {
    /// The policy `bin`'s `<BIN>_CACHE_*` / `<BIN>_NO_CACHE` variables
    /// describe.
    pub fn from_env(bin: &str) -> Self {
        Self {
            ttl: ttl(bin),
            max_stale: max_stale(bin),
            revalidate_timeout: revalidate_timeout(bin),
            bypass_reads: disabled(bin),
        }
    }
}

/// Starts a background refresh of one cache entry. The cache claims the
/// entry's in-flight marker before calling [`start`](Self::start) and
/// releases it if the start fails; the refresh releases it when it ends
/// ([`CachedRemote::revalidate_claimed`]).
pub trait Revalidator: Send + Sync {
    /// Start the refresh; `false` if it could not be started.
    fn start(&self, kind: Kind, rel: &str) -> bool;
}

/// The production [`Revalidator`]: a detached
/// `<exe> <command…> --revalidate-file=<rel> --remote-spec=<spec>` (or
/// `--revalidate-listing`). The child's stdio is null, so it never holds a
/// caller's pipes (an app reading a command's output to EOF would wait on
/// it), and it sits in its own process group, so the terminal's Ctrl-C to
/// the command does not cut a refresh short. The child bounds its own
/// rclone calls ([`run_revalidation`]), which bounds its lifetime.
pub struct SpawnRevalidator {
    exe: PathBuf,
    command: Vec<String>,
    spec: String,
}

impl SpawnRevalidator {
    /// Refresh by running `exe` with `command` (the CLI's subcommand that
    /// accepts the revalidate arguments, e.g. `["sync"]`) against `spec`.
    pub fn new(exe: PathBuf, command: &[&str], spec: &str) -> Self {
        Self {
            exe,
            command: command.iter().map(|s| s.to_string()).collect(),
            spec: spec.to_string(),
        }
    }

    /// The argv the refresh of `rel` runs (after the executable).
    pub fn args(&self, kind: Kind, rel: &str) -> Vec<String> {
        let flag = match kind {
            Kind::File => REVALIDATE_FILE_ARG,
            Kind::Listing => REVALIDATE_LISTING_ARG,
        };
        let mut args = self.command.clone();
        args.push(format!("{flag}={rel}"));
        args.push(format!("{REMOTE_SPEC_ARG}={}", self.spec));
        args
    }
}

impl Revalidator for SpawnRevalidator {
    fn start(&self, kind: Kind, rel: &str) -> bool {
        let mut cmd = Command::new(&self.exe);
        cmd.args(self.args(kind, rel))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                // Reap it if this process lives long enough; a short-lived
                // CLI exits first and init reaps it.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                true
            }
            Err(_) => false,
        }
    }
}

/// The background refresh for `bin`'s cache over `spec`: this binary run
/// with `command`. `None` when `<BIN>_CACHE_NO_REVALIDATE` is set or this
/// binary's path is unknown.
pub fn background_revalidator(
    bin: &str,
    command: &[&str],
    spec: &str,
) -> Option<Box<dyn Revalidator>> {
    if truthy(&var(bin, "CACHE_NO_REVALIDATE")) {
        return None;
    }
    Some(Box::new(SpawnRevalidator::new(
        env::current_exe().ok()?,
        command,
        spec,
    )))
}

/// The child side of a [`SpawnRevalidator`]: refresh one entry of `bin`'s
/// cache over `spec` with every rclone call bounded by
/// `<BIN>_CACHE_REVALIDATE_TIMEOUT`, and release the in-flight marker the
/// spawner claimed however the refresh ends. Returns the cache dir and what
/// the refresh did.
pub fn run_revalidation(
    bin: &str,
    spec: &str,
    kind: Kind,
    rel: &str,
) -> Result<(PathBuf, Outcome), CliError> {
    let dir = cache_dir_for(bin, spec).ok_or_else(|| {
        CliError::Other("cannot resolve a cache directory (is $HOME set?)".into())
    })?;
    let policy = CachePolicy::from_env(bin);
    let remote = Remote::new(spec).with_budget(policy.revalidate_timeout);
    let cached = CachedRemote::new(remote, dir.clone(), policy);
    let outcome = cached.revalidate_claimed(kind, rel)?;
    Ok((dir, outcome))
}

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

    /// Re-pull a file from the remote into the cache regardless of TTL.
    /// Returns whether the file existed on the remote.
    pub fn refresh_file(&self, rel: &str) -> Result<bool, CliError> {
        let generation = self.cache.generation();
        let content = self.remote.cat(rel)?;
        self.cache.commit_file(rel, content.as_deref(), generation);
        Ok(content.is_some())
    }

    /// Re-pull a recursive listing from the remote into the cache.
    pub fn refresh_listing(&self, rel: &str) -> Result<(), CliError> {
        self.revalidate(Kind::Listing, rel).map(|_| ())
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
                    "{} changed on the remote after this command read an older cached \
                     copy of it, so the write was refused. The cache now holds the current \
                     copy — rerun the command.",
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

/// The on-disk cache for one remote spec. All plumbing is best-effort: a disk
/// error makes a read miss (and fall through) and a store a no-op.
struct CacheDir {
    dir: PathBuf,
}

impl CacheDir {
    /// A cached file and its age (`None`: unreadable or future mtime).
    fn file(&self, rel: &str) -> Option<(String, Option<Duration>)> {
        let path = self.content_path(rel)?;
        let content = fs::read_to_string(&path).ok()?;
        Some((content, age(&path)))
    }

    fn listing(&self, rel: &str) -> Option<(Vec<FileEntry>, Option<Duration>)> {
        let path = self.listing_path(rel)?;
        let entries = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
        Some((entries, age(&path)))
    }

    /// Store (or, for `None`, drop) a fetched file — only if no write bumped
    /// the generation since `generation` was read before the fetch. Returns
    /// whether the cache now reflects the fetch.
    fn commit_file(&self, rel: &str, content: Option<&str>, generation: u64) -> bool {
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

    fn commit_listing(&self, rel: &str, entries: &[FileEntry], generation: u64) -> bool {
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
    fn record_write<'a>(&self, paths: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) {
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
    fn generation(&self) -> u64 {
        fs::read_to_string(self.control().join("generation"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The cache-wide lock that orders a write's generation bump and stores
    /// against a fetch's check-and-store. Held only around local file
    /// operations, never across a network call. Released on drop.
    fn lock(&self) -> Option<fs::File> {
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
    fn marker(&self, kind: Kind, rel: &str) -> Option<PathBuf> {
        Some(self.control().join("revalidating").join(format!(
            "{}-{:016x}",
            kind.as_str(),
            digest(rel.as_bytes())
        )))
    }

    fn control(&self) -> PathBuf {
        self.dir.join(CONTROL_SUBDIR)
    }

    fn store_file(&self, rel: &str, content: &str) {
        if let Some(path) = self.content_path(rel) {
            let _ = self.write_atomic(&path, content.as_bytes());
        }
    }

    fn remove_file(&self, rel: &str) {
        if let Some(path) = self.content_path(rel) {
            let _ = fs::remove_file(path);
        }
    }

    /// Content mirrors the data-root relative path directly under the cache dir.
    fn content_path(&self, rel: &str) -> Option<PathBuf> {
        safe_join(&self.dir, rel)
    }

    /// Listings live under `.listings-v2/<rel>.json` (root listing → `_root_`).
    fn listing_path(&self, rel: &str) -> Option<PathBuf> {
        let key = if rel.is_empty() { "_root_" } else { rel };
        safe_join(&self.dir.join(LISTINGS_SUBDIR), &format!("{key}.json"))
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
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
    fn secure_base(&self) -> io::Result<()> {
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
struct MarkerGuard(Option<PathBuf>);

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
fn claim_marker(marker: &Path, bound: Duration) -> bool {
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

fn digest(bytes: &[u8]) -> u64 {
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

/// Join `rel` under `base`, rejecting anything but plain forward path
/// components — no `..`, absolute paths, or prefixes can escape the cache dir.
fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
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
fn age(path: &Path) -> Option<Duration> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
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
    };

    #[test]
    fn the_default_policy_is_the_documented_defaults() {
        assert_eq!(CachePolicy::default(), POLICY);
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
        assert!(err.to_string().contains("rerun"), "{err}");
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
