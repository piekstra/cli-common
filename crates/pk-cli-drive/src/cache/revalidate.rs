//! The background refresh of a copy served stale: how it is started (a
//! detached child of the CLI's own binary) and what the child runs.

use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use pk_cli_core::CliError;

use super::{CachePolicy, CachedRemote, Kind, Outcome};
use crate::rclone::Remote;

/// The argument a [`SpawnRevalidator`] passes to refresh one file:
/// `--revalidate-file=<rel>`. The CLI's command accepts it (hidden) and calls
/// [`run_revalidation`].
pub const REVALIDATE_FILE_ARG: &str = "--revalidate-file";
/// `--revalidate-listing=<rel>`: refresh one recursive listing.
pub const REVALIDATE_LISTING_ARG: &str = "--revalidate-listing";
/// `--remote-spec=<spec>`: the spec the spawning command read through, so the
/// refresh lands in the same cache dir even if the config changed since.
pub const REMOTE_SPEC_ARG: &str = "--remote-spec";

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

/// The background refresh for a cache over `spec`: this binary run with
/// `command`. `None` when the policy bypasses reads or turns background
/// refresh off, or this binary's path is unknown.
pub fn background_revalidator(
    policy: &CachePolicy,
    command: &[&str],
    spec: &str,
) -> Option<Box<dyn Revalidator>> {
    if policy.bypass_reads || !policy.background_refresh {
        return None;
    }
    Some(Box::new(SpawnRevalidator::new(
        env::current_exe().ok()?,
        command,
        spec,
    )))
}

/// The child side of a [`SpawnRevalidator`]: refresh one entry of the cache
/// in `dir`, with every rclone call bounded by the policy's
/// `revalidate_timeout`, and release the in-flight marker the spawner claimed
/// however the refresh ends.
///
/// Build `remote`, `dir` and `policy` exactly as the spawning command built
/// its cache (the same `Remote::program`, the same `cache_dir_for(bin, spec)`
/// with the spec from [`REMOTE_SPEC_ARG`], the same policy source), so the
/// child runs the same rclone against the same entry under the same bound.
pub fn run_revalidation(
    remote: Remote,
    dir: PathBuf,
    policy: CachePolicy,
    kind: Kind,
    rel: &str,
) -> Result<Outcome, CliError> {
    let remote = remote.with_budget(policy.revalidate_timeout);
    CachedRemote::new(remote, dir, policy).revalidate_claimed(kind, rel)
}
