//! State in the owner's Drive, for the piekstra CLI family.
//!
//! A CLI that keeps its own small state files (a registry, notes, settings
//! it shares across machines) in a cloud-drive folder the owner already
//! has gets two ways at that folder behind one interface, [`Backend`]:
//!
//! - [`Backend::Mount`]: a local filesystem path — a Drive-for-desktop
//!   mount, or any directory. Fast, but a mount can go stale without saying
//!   so: a plain local directory left where a mount used to be accepts
//!   writes and syncs nothing, and a lazily-materialized mount can answer
//!   "no such file" for a folder the remote has. Certify a mount (write a
//!   marker through it, confirm it over rclone) before trusting it.
//! - [`Backend::Remote`] / [`Backend::Cached`]: rclone ([`Remote`]) as the
//!   transactional path (`cat`/`copyto`/`moveto`/`lsf`/`mkdir`). Slower
//!   per call but authoritative: it talks to the provider's API directly and
//!   is immune to mount health. Normally fronted by the read cache
//!   ([`CachedRemote`]), which serves reads locally and keeps the remote the
//!   single source of truth (see [`cache`] for its invariants).
//!
//! The usual policy: when an rclone remote is configured, use it for every
//! state operation; keep the mount for human browsing and bulk filing.
//!
//! ```no_run
//! use pk_cli_drive::{cache, Backend, CachedRemote, Remote};
//!
//! const BIN: &str = "example-cli";
//!
//! fn backend(remote: Option<&str>, mount: &str) -> Backend {
//!     let Some(spec) = remote else {
//!         return Backend::Mount(mount.into());
//!     };
//!     let remote = Remote::new(spec).temp_prefix(BIN);
//!     let Some(dir) = cache::cache_dir_for(BIN, spec) else {
//!         return Backend::Remote(remote);
//!     };
//!     let policy = cache::CachePolicy::from_env(BIN);
//!     let mut cached = CachedRemote::new(remote, dir, policy);
//!     if !policy.bypass_reads {
//!         // `example-cli state sync --revalidate-file=<rel> --remote-spec=<spec>`
//!         if let Some(r) = cache::background_revalidator(BIN, &["state", "sync"], spec) {
//!             cached = cached.with_revalidator(r);
//!         }
//!     }
//!     Backend::Cached(cached)
//! }
//! ```
//!
//! A CLI using the cache takes on two obligations: its refresh command
//! accepts the hidden revalidate arguments ([`cache::REVALIDATE_FILE_ARG`],
//! [`cache::REVALIDATE_LISTING_ARG`], [`cache::REMOTE_SPEC_ARG`]) and calls
//! [`cache::run_revalidation`]; and it documents one more exit-1 case — a
//! write refused because a copy the command was served stale has since
//! changed on the remote (nothing written, the cache now current, rerunning
//! at once is safe).

pub mod backend;
pub mod cache;
pub mod private_file;
pub mod rclone;

pub use backend::{private_temp_dir, Backend, FileEntry, LocalCopy, TempDir};
pub use cache::{CachePolicy, CachedRemote, Kind, Outcome, Revalidator};
pub use rclone::{rclone_version, run_rclone, Remote, RemoteRead};

#[cfg(all(test, unix))]
pub(crate) mod stub;
