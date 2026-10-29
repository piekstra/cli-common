//! Ctrl-C while a Chrome window is open.
//!
//! Without a handler, Ctrl-C ends the process with every `Drop` un-run, so
//! the window stays open and the scratch profile (which holds the session)
//! stays on disk. The handler installed by [`install`] turns the first
//! Ctrl-C into a cancel that the waiting code notices within a fraction of
//! a second, so the [`Browser`](crate::Browser) unwinds normally; a second
//! Ctrl-C, for when the unwinding is stuck, kills every live Chrome, removes
//! its profile and exits 130. With no Chrome live, Ctrl-C exits 130 at
//! once, as it would with no handler.
//!
//! The handler is process-wide and installed once. A CLI that runs its own
//! Ctrl-C handler opts out with [`Launch::ctrl_c(false)`](crate::Launch::ctrl_c)
//! and calls [`cancel`] from it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

static CANCELED: AtomicBool = AtomicBool::new(false);
static LIVE: Mutex<Registry> = Mutex::new(Registry {
    next: 0,
    entries: Vec::new(),
});
static HANDLER: OnceLock<Result<(), String>> = OnceLock::new();

struct Registry {
    next: u64,
    entries: Vec<Entry>,
}

struct Entry {
    id: u64,
    /// `None` once the process has been reaped, so its pid (free for reuse)
    /// is never signalled.
    pid: Option<u32>,
    profile: PathBuf,
}

/// Cancel every wait on a live Chrome window: each returns
/// "browser login canceled" and its [`Browser`](crate::Browser) cleans up as
/// it drops. For a CLI with its own Ctrl-C handler; a cancel applies to the
/// windows open when it is called.
pub fn cancel() {
    CANCELED.store(true, Ordering::SeqCst);
}

pub(crate) fn canceled() -> bool {
    CANCELED.load(Ordering::SeqCst)
}

fn registry() -> MutexGuard<'static, Registry> {
    LIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Install the handler once per process. `Err` carries why it could not be
/// (another handler is already installed), for a warning.
pub(crate) fn install() -> Result<(), String> {
    HANDLER
        .get_or_init(|| ctrlc::set_handler(on_interrupt).map_err(|e| e.to_string()))
        .clone()
}

fn on_interrupt() {
    let registry = registry();
    if registry.entries.is_empty() {
        std::process::exit(130);
    }
    if CANCELED.swap(true, Ordering::SeqCst) {
        for entry in &registry.entries {
            if let Some(pid) = entry.pid {
                force_kill(pid);
            }
            let _ = std::fs::remove_dir_all(&entry.profile);
        }
        std::process::exit(130);
    }
}

#[cfg(unix)]
fn force_kill(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill(2) with a pid this process spawned and has not
        // reaped (the registry clears it under the same lock first).
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn force_kill(_pid: u32) {}

/// Track a launched Chrome. The first registration after none were live
/// starts with a clear cancel flag.
pub(crate) fn register(pid: u32, profile: PathBuf) -> u64 {
    let mut registry = registry();
    if registry.entries.is_empty() {
        CANCELED.store(false, Ordering::SeqCst);
    }
    registry.next += 1;
    let id = registry.next;
    registry.entries.push(Entry {
        id,
        pid: Some(pid),
        profile,
    });
    id
}

/// Run `reap` (a `try_wait` on the child) under the registry lock, and if it
/// reports the process gone, forget its pid in the same critical section, so
/// the handler can never signal a pid that has been reaped and reused.
pub(crate) fn reap_with<T>(id: u64, reap: impl FnOnce() -> Option<T>) -> Option<T> {
    let mut registry = registry();
    let status = reap();
    if status.is_some() {
        if let Some(entry) = registry.entries.iter_mut().find(|e| e.id == id) {
            entry.pid = None;
        }
    }
    status
}

/// Stop tracking a Chrome. The last one out clears the cancel flag.
pub(crate) fn unregister(id: u64) {
    let mut registry = registry();
    registry.entries.retain(|e| e.id != id);
    if registry.entries.is_empty() {
        CANCELED.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry bookkeeping only. The cancel flag is process-wide, so no
    /// test sets it: a parallel test's launch would see it.
    #[test]
    fn a_reaped_process_loses_its_pid_and_unregisters() {
        let id = register(u32::MAX, PathBuf::from("/nonexistent/profile"));
        let pid = |id| {
            registry()
                .entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.pid)
        };
        assert_eq!(pid(id), Some(Some(u32::MAX)));
        assert_eq!(reap_with(id, || None::<()>), None);
        assert_eq!(pid(id), Some(Some(u32::MAX)), "still running: pid kept");
        assert_eq!(reap_with(id, || Some(0)), Some(0));
        assert_eq!(pid(id), Some(None), "reaped: pid forgotten");
        unregister(id);
        assert_eq!(pid(id), None);
    }
}
