//! What the binary says it was built from (DESIGN.md §1.6), black-box: the
//! stamp is made when the binary is compiled.

use std::path::Path;
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_example-cli");

fn run(args: &[&str]) -> String {
    let out = Command::new(EXE)
        .args(args)
        .output()
        .expect("example-cli runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// What git says of this checkout; `None` outside one (an unpacked archive).
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

#[test]
fn the_short_flag_is_the_version_alone() {
    assert_eq!(
        run(&["-V"]),
        format!("example-cli {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_and_info_name_the_same_build() {
    let info: serde_json::Value = serde_json::from_str(&run(&["info"])).unwrap();
    let build = &info["build"];
    let built_at = build["built_at"].as_str().unwrap();
    let long = run(&["--version"]);
    assert!(
        long.starts_with(&format!("example-cli {} (", env!("CARGO_PKG_VERSION"))),
        "{long}"
    );
    assert!(long.ends_with(&format!(", built {built_at})\n")), "{long}");
    match git(&["rev-parse", "HEAD"]) {
        Some(head) => {
            assert_eq!(build["commit"], head.as_str());
            assert!(long.contains(&head[..12]), "{long}");
            assert!(build["dirty"].is_boolean(), "{build}");
        }
        None => assert!(long.contains("commit unknown"), "{long}"),
    }
}

/// The compiler tracks HEAD's log, which names people; none of it is in the
/// binary.
#[test]
fn the_log_the_stamp_tracks_is_not_in_the_binary() {
    let Some(log) = git(&["rev-parse", "--git-path", "logs/HEAD"]) else {
        return;
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(log);
    let Ok(log) = std::fs::read(path) else {
        return;
    };
    let last = log
        .split(|b| *b == b'\n')
        .rfind(|line| !line.is_empty())
        .expect("a line");
    // Past the two commits, from the name on.
    let named = &last[last.len().min(82)..];
    assert!(named.len() > 8, "{}", String::from_utf8_lossy(last));
    let binary = std::fs::read(EXE).unwrap();
    assert!(!binary.windows(named.len()).any(|window| window == named));
}
