//! Which commit a CLI was built from, whether its tree was modified, and
//! when: the compile-time half of `pk_cli_core::build_info!` and
//! `pk_cli_core::version!` (DESIGN.md §1.6). Use those; [`stamp!`] is what
//! they expand to.
//!
//! A macro rather than a build script, because of when each one runs. A
//! dependency's build script runs in the dependency's own directory, once
//! for every CLI that shares a target directory, and not when the CLI's code
//! changes, so it can name another repository's commit or call a modified
//! tree clean. The macro runs in the CLI's own compile, every time that code
//! is compiled, so what it says is about the code in the binary.
//!
//! The expansion also makes the compiler track the repository's HEAD log,
//! so a commit, a checkout or a pull recompiles the crate that holds the
//! stamp even when no source file changed. Nothing of the log is in the
//! binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use proc_macro::TokenStream;

/// `(commit, dirty, built)` for the crate this expands in:
/// `(Option<&'static str>, Option<bool>, i64)`. The commit is 40 hex digits
/// and `dirty` says whether a tracked file differed from it; both are `None`
/// where there is no repository to ask. `built` is Unix seconds
/// (`SOURCE_DATE_EPOCH` when that is set).
///
/// Every use in one compile gives the same answer.
#[proc_macro]
pub fn stamp(_input: TokenStream) -> TokenStream {
    static STAMP: OnceLock<Stamp> = OnceLock::new();
    STAMP
        .get_or_init(|| {
            let dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
            Stamp::of(dir.as_deref(), built_at())
        })
        .tokens()
        .parse()
        .expect("the stamp is a Rust expression")
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    commit: Option<String>,
    dirty: Option<bool>,
    built: i64,
    /// Files whose change means the stamp may be out of date.
    track: Vec<PathBuf>,
}

impl Stamp {
    /// The stamp of the crate in `dir`.
    fn of(dir: Option<&Path>, built: i64) -> Self {
        let unknown = Stamp {
            commit: None,
            dirty: None,
            built,
            track: Vec::new(),
        };
        let Some(dir) = dir else { return unknown };
        // The repository around `dir` is this crate's only if it tracks the
        // crate: a tarball unpacked inside another checkout is not.
        if git(dir, &["ls-files", "--error-unmatch", "--", "Cargo.toml"]).is_none() {
            return unknown;
        }
        let Some(commit) = git(dir, &["rev-parse", "--verify", "HEAD"]).filter(|c| is_commit(c))
        else {
            return unknown;
        };
        let dirty = git(dir, &["status", "--porcelain", "--untracked-files=no"])
            .map(|changed| !changed.is_empty());
        // HEAD moves with a checkout, its log with every commit too.
        let track = ["HEAD", "logs/HEAD"]
            .iter()
            .filter_map(|name| git(dir, &["rev-parse", "--git-path", name]))
            .filter_map(|path| dir.join(path).canonicalize().ok())
            .filter(|path| path.is_file() && path.to_str().is_some())
            .collect();
        Stamp {
            commit: Some(commit),
            dirty,
            built,
            track,
        }
    }

    /// The stamp as an expression. The tracked files are read by constants
    /// nothing refers to, which the compiler records and does not emit.
    fn tokens(&self) -> String {
        let track: String = self
            .track
            .iter()
            .filter_map(|path| path.to_str())
            .map(|path| format!("const _: &[u8] = ::core::include_bytes!({path:?});"))
            .collect();
        let commit = match &self.commit {
            Some(commit) => format!("::core::option::Option::Some({commit:?})"),
            None => "::core::option::Option::None".into(),
        };
        let dirty = match self.dirty {
            Some(dirty) => format!("::core::option::Option::Some({dirty})"),
            None => "::core::option::Option::None".into(),
        };
        let built = self.built;
        format!("{{ {track} ({commit}, {dirty}, {built}i64) }}")
    }
}

/// What git answers in `dir`, trimmed, or `None` when there is no git, no
/// repository, or it refuses. It runs nothing of the repository's own
/// (no file-system monitor), takes no optional lock, and ignores the
/// `GIT_*` variables a hook sets, which would name the hook's repository.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(name);
    }
    let out = command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// A full object name, SHA-1 or SHA-256.
fn is_commit(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Now, or the `SOURCE_DATE_EPOCH` of a reproducible build.
fn built_at() -> i64 {
    epoch_from(std::env::var("SOURCE_DATE_EPOCH").ok().as_deref()).unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64)
    })
}

fn epoch_from(var: Option<&str>) -> Option<i64> {
    var?.trim().parse().ok().filter(|secs| *secs >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A repository of the test's own, removed when it ends.
    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pk-cli-buildinfo-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            Repo(dir.canonicalize().unwrap())
        }

        fn git(&self, args: &[&str]) {
            let out = Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .arg("-C")
                .arg(&self.0)
                .args(["-c", "user.name=Example", "-c", "user.email=e@example.com"])
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn write(&self, name: &str, text: &str) {
            std::fs::write(self.0.join(name), text).unwrap();
        }

        /// One commit holding a crate.
        fn committed() -> Self {
            let repo = Repo::new();
            repo.git(&["init", "-q", "-b", "main"]);
            repo.write("Cargo.toml", "[package]\n");
            repo.write("src/main.rs", "fn main() {}\n");
            repo.git(&["add", "."]);
            repo.git(&["commit", "-q", "-m", "one"]);
            repo
        }

        fn stamp(&self) -> Stamp {
            Stamp::of(Some(&self.0), 7)
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_clean_tree_is_its_commit() {
        let repo = Repo::committed();
        let stamp = repo.stamp();
        let head = git(&repo.0, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(stamp.commit.as_deref(), Some(head.as_str()));
        assert_eq!(head.len(), 40);
        assert_eq!(stamp.dirty, Some(false));
        assert_eq!(stamp.built, 7);
        let names: Vec<_> = stamp
            .track
            .iter()
            .map(|p| p.strip_prefix(&repo.0).unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [PathBuf::from(".git/HEAD"), PathBuf::from(".git/logs/HEAD")]
        );
    }

    #[test]
    fn a_changed_or_staged_tracked_file_is_dirty_and_an_untracked_one_is_not() {
        let repo = Repo::committed();
        repo.write("notes.txt", "scratch\n");
        assert_eq!(repo.stamp().dirty, Some(false));
        repo.write("src/main.rs", "fn main() { let _ = 1; }\n");
        assert_eq!(repo.stamp().dirty, Some(true));
        repo.git(&["add", "src/main.rs"]);
        assert_eq!(repo.stamp().dirty, Some(true));
        repo.git(&["commit", "-q", "-m", "two"]);
        assert_eq!(repo.stamp().dirty, Some(false));
    }

    #[test]
    fn a_commit_moves_a_tracked_file() {
        let repo = Repo::committed();
        let read = |stamp: &Stamp| -> Vec<Vec<u8>> {
            stamp
                .track
                .iter()
                .map(|p| std::fs::read(p).unwrap())
                .collect()
        };
        let before = read(&repo.stamp());
        repo.git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        assert_ne!(read(&repo.stamp()), before);
    }

    #[test]
    fn a_worktree_is_stamped_with_its_own_commit() {
        let repo = Repo::committed();
        let first = repo.stamp();
        repo.git(&["worktree", "add", "-q", "-b", "other", "tree"]);
        repo.git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        let tree = Stamp::of(Some(&repo.0.join("tree")), 7);
        assert_eq!(tree.commit, first.commit);
        assert_ne!(repo.stamp().commit, first.commit);
        assert_eq!(tree.dirty, Some(false));
        assert!(
            tree.track
                .iter()
                .all(|p| p.starts_with(repo.0.join(".git/worktrees/tree"))),
            "{:?}",
            tree.track
        );
    }

    /// A crate inside a repository that does not track it (an unpacked
    /// tarball, a build directory) is not built from that repository.
    #[test]
    fn a_crate_the_repository_does_not_track_has_no_commit() {
        let repo = Repo::committed();
        let inner = repo.0.join("vendor-copy");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(inner.join("Cargo.toml"), "[package]\n").unwrap();
        let stamp = Stamp::of(Some(&inner), 7);
        assert_eq!((stamp.commit, stamp.dirty), (None, None));
        assert!(stamp.track.is_empty());
    }

    #[test]
    fn no_repository_no_commit_and_no_directory_are_unknown() {
        let repo = Repo::new();
        repo.write("Cargo.toml", "[package]\n");
        // Not a repository (the temporary directory is in none).
        assert_eq!(repo.stamp().commit, None);
        // A repository with no commit yet.
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["add", "Cargo.toml"]);
        assert_eq!(repo.stamp().commit, None);
        assert_eq!(Stamp::of(None, 7).commit, None);
    }

    #[test]
    fn the_expression_names_the_facts_and_tracks_the_files() {
        let known = Stamp {
            commit: Some("a".repeat(40)),
            dirty: Some(true),
            built: 7,
            track: vec![PathBuf::from("/r/.git/HEAD")],
        };
        assert_eq!(
            known.tokens(),
            format!(
                "{{ const _: &[u8] = ::core::include_bytes!(\"/r/.git/HEAD\"); \
                 (::core::option::Option::Some(\"{}\"), ::core::option::Option::Some(true), 7i64) }}",
                "a".repeat(40)
            )
        );
        let unknown = Stamp::of(None, 7);
        assert_eq!(
            unknown.tokens(),
            "{  (::core::option::Option::None, ::core::option::Option::None, 7i64) }"
        );
    }

    #[test]
    fn source_date_epoch_is_seconds_or_nothing() {
        assert_eq!(epoch_from(Some("1700000000")), Some(1_700_000_000));
        assert_eq!(epoch_from(Some(" 5 ")), Some(5));
        for bad in ["", "now", "-1", "1.5"] {
            assert_eq!(epoch_from(Some(bad)), None, "{bad:?}");
        }
        assert_eq!(epoch_from(None), None);
    }

    #[test]
    fn only_a_full_object_name_is_a_commit() {
        assert!(is_commit(&"0".repeat(40)));
        assert!(is_commit(&"f".repeat(64)));
        for bad in ["", "HEAD", "abc123", &"g".repeat(40)] {
            assert!(!is_commit(bad), "{bad:?}");
        }
    }
}
