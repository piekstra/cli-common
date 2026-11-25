//! What a binary was built from: the commit, whether the tree was modified,
//! and when (DESIGN.md §1.6). A version alone does not say it: a binary
//! built from a branch, or from a tree with local changes, carries the
//! version of the last release.
//!
//! ```
//! use clap::Parser;
//!
//! #[derive(Parser)]
//! // `--version` prints the build, `-V` the version alone.
//! #[command(name = "svc", version, long_version = pk_cli_core::version!())]
//! struct Cli {}
//!
//! // `info`: CliInfo::new(...).with_build(pk_cli_core::build_info!())
//! let build = pk_cli_core::build_info!();
//! assert!(pk_cli_core::version!().contains(&build.built_at));
//! ```
//!
//! Both macros read the repository of the crate they are written in, when it
//! is compiled, so each belongs in the CLI's own code.

use serde::{Deserialize, Serialize};

use crate::dates;

/// How many digits of the commit `--version` shows.
const SHORT: usize = 12;

/// The `build` object of `info` (`cli-info/v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BuildInfo {
    /// The full commit. Absent when the crate was not built from a git
    /// checkout that tracks it, or without git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Whether a tracked file differed from the commit, so the commit alone
    /// does not give this binary. Absent when that could not be told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// When the CLI's code was compiled, RFC 3339 in UTC.
    pub built_at: String,
}

impl BuildInfo {
    /// What [`build_info!`](crate::build_info) calls, with the stamp.
    pub fn new(commit: Option<&str>, dirty: Option<bool>, built: i64) -> Self {
        BuildInfo {
            commit: commit.map(str::to_string),
            dirty,
            built_at: dates::fmt_rfc3339(built),
        }
    }

    /// The text `--version` prints after the binary's name:
    /// `0.3.1 (0123456789ab, built 2026-11-24T21:00:00Z)`, with
    /// `0123456789ab modified` for a tree with local changes and
    /// `commit unknown` without a commit.
    pub fn version_line(&self, version: &str) -> String {
        let commit = match &self.commit {
            Some(commit) => commit.get(..SHORT).unwrap_or(commit),
            None => "commit unknown",
        };
        let modified = match self.dirty {
            Some(true) => " modified",
            Some(false) => "",
            // A commit whose tree could not be compared is not a clean one.
            None if self.commit.is_some() => " perhaps modified",
            None => "",
        };
        format!("{version} ({commit}{modified}, built {})", self.built_at)
    }
}

/// The [`BuildInfo`] of the crate this is written in, for
/// [`CliInfo::with_build`](crate::info::CliInfo::with_build).
#[macro_export]
macro_rules! build_info {
    () => {{
        let (commit, dirty, built) = $crate::__build_stamp!();
        $crate::BuildInfo::new(commit, dirty, built)
    }};
}

/// The crate's version and what it was built from, as a `&'static str`, for
/// clap's `long_version`: what `--version` prints
/// ([`BuildInfo::version_line`](crate::BuildInfo::version_line)).
#[macro_export]
macro_rules! version {
    () => {{
        static LINE: ::std::sync::OnceLock<::std::string::String> = ::std::sync::OnceLock::new();
        LINE.get_or_init(|| $crate::build_info!().version_line(::core::env!("CARGO_PKG_VERSION")))
            .as_str()
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn line(commit: Option<&str>, dirty: Option<bool>) -> String {
        BuildInfo::new(commit, dirty, 1_700_000_000).version_line("1.2.3")
    }

    #[test]
    fn the_version_line_says_the_commit_the_tree_and_the_time() {
        assert_eq!(
            line(Some(COMMIT), Some(false)),
            "1.2.3 (0123456789ab, built 2023-11-14T22:13:20Z)"
        );
        assert_eq!(
            line(Some(COMMIT), Some(true)),
            "1.2.3 (0123456789ab modified, built 2023-11-14T22:13:20Z)"
        );
        assert_eq!(
            line(Some(COMMIT), None),
            "1.2.3 (0123456789ab perhaps modified, built 2023-11-14T22:13:20Z)"
        );
        assert_eq!(
            line(None, None),
            "1.2.3 (commit unknown, built 2023-11-14T22:13:20Z)"
        );
    }

    #[test]
    fn what_is_not_known_is_left_out() {
        let v = serde_json::to_value(BuildInfo::new(None, None, 1_700_000_000)).unwrap();
        assert_eq!(v, serde_json::json!({ "built_at": "2023-11-14T22:13:20Z" }));
        let read: BuildInfo = serde_json::from_value(v).unwrap();
        assert_eq!((read.commit, read.dirty), (None, None));
    }

    /// The macros agree with each other in one compile.
    #[test]
    fn the_macros_give_one_stamp() {
        let build = crate::build_info!();
        assert_eq!(
            crate::version!(),
            build.version_line(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(crate::build_info!(), build);
        assert!(build.built_at.ends_with('Z'), "{}", build.built_at);
        if let Some(commit) = &build.commit {
            assert_eq!(commit.len(), 40);
            assert!(build.dirty.is_some());
        }
    }
}
