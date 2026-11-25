//! Where the answers a cache handle gave came from.

use std::sync::Mutex;
use std::time::Duration;

/// What one [`super::CachedRemote`] has answered so far, by where each answer
/// came from: for a command that says whether its output came from the read
/// cache or from the remote, and how old the oldest copy was.
///
/// Each read counts, so a file read twice counts twice. A read that failed
/// answered nothing and is not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reads {
    /// Answered from the cache, whatever the copy's age.
    pub cache: usize,
    /// Of `cache`: from a copy past its TTL, or of unknown age.
    pub stale: usize,
    /// Of `cache`: from a copy that stood in for a remote that failed.
    pub unreachable: usize,
    /// Answered by the remote. No file there is an answer to a file read and
    /// to `exists`; a download of a missing file fails, so it does not count.
    pub remote: usize,
    /// How old the oldest copy in `cache` was when it was served: the time
    /// since this machine fetched or wrote it. `None` when `cache` is 0, or
    /// when a copy's age is unknown (the clock went back since it was stored).
    pub oldest: Option<Duration>,
}

/// The count behind [`Reads`], kept as reads are answered.
#[derive(Default)]
pub(super) struct Tally(Mutex<Counted>);

#[derive(Default)]
struct Counted {
    reads: Reads,
    /// A copy was served whose age is unknown, so no age is the oldest.
    age_unknown: bool,
}

/// A copy served from the cache.
pub(super) struct Served {
    pub(super) age: Option<Duration>,
    pub(super) stale: bool,
    pub(super) unreachable: bool,
}

impl Tally {
    pub(super) fn cache(&self, copy: Served) {
        let mut counted = self.lock();
        counted.reads.cache += 1;
        counted.reads.stale += usize::from(copy.stale);
        counted.reads.unreachable += usize::from(copy.unreachable);
        match copy.age {
            Some(age) => counted.reads.oldest = counted.reads.oldest.max(Some(age)),
            None => counted.age_unknown = true,
        }
    }

    pub(super) fn remote(&self) {
        self.lock().reads.remote += 1;
    }

    pub(super) fn reads(&self) -> Reads {
        let counted = self.lock();
        let mut reads = counted.reads.clone();
        if counted.age_unknown {
            reads.oldest = None;
        }
        reads
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counted> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}
