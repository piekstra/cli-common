use super::*;

/// A data dir of the test's own, standing in for the per-user one.
#[cfg(unix)]
fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pk-cli-browser-{tag}-{}", unpredictable()));
    std::fs::create_dir(&root).unwrap();
    root
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn the_data_directory_follows_the_platform() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    };
    let home = &[("HOME", "/home/o"), ("XDG_DATA_HOME", "/xdg")];
    assert_eq!(
        data_home_from(Platform::MacOs, env(home)),
        Some(PathBuf::from("/home/o/Library/Application Support"))
    );
    assert_eq!(
        data_home_from(Platform::Xdg, env(home)),
        Some(PathBuf::from("/xdg"))
    );
    assert_eq!(
        data_home_from(Platform::Xdg, env(&[("HOME", "/home/o")])),
        Some(PathBuf::from("/home/o/.local/share"))
    );
    // An empty or relative XDG_DATA_HOME is ignored, as the XDG spec says.
    for pairs in [
        &[("HOME", "/home/o"), ("XDG_DATA_HOME", "")],
        &[("HOME", "/home/o"), ("XDG_DATA_HOME", "rel/data")],
    ] {
        assert_eq!(
            data_home_from(Platform::Xdg, env(pairs)),
            Some(PathBuf::from("/home/o/.local/share")),
            "{pairs:?}"
        );
    }
    assert_eq!(
        data_home_from(Platform::Windows, env(&[("LOCALAPPDATA", "/appdata")])),
        Some(PathBuf::from("/appdata"))
    );
    // Nothing usable: no directory, never one relative to the working dir.
    for platform in [Platform::MacOs, Platform::Xdg, Platform::Windows] {
        assert_eq!(data_home_from(platform, env(&[])), None);
        assert_eq!(
            data_home_from(platform, env(&[("HOME", "relative"), ("LOCALAPPDATA", "")])),
            None
        );
    }
}

#[test]
fn a_name_a_launch_would_refuse_is_refused_before_anything_is_looked_up() {
    let err = forget_named_profile("t", "a b").unwrap_err();
    assert!(matches!(err, BrowserError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("browser profile name `a b`"),
        "{err}"
    );
}

#[test]
#[should_panic(expected = "a test reached the real data directory")]
fn tests_never_reach_the_real_data_directory() {
    let _ = default_root();
}

#[test]
fn the_kept_profile_is_named_for_the_bin() {
    let root = Path::new("/data");
    assert_eq!(
        kept_profile_path(root, "sfi", None),
        Path::new("/data/piekstra.sfi/browser-profile")
    );
    assert_eq!(
        kept_profile_path(root, "rhd", Some("member-a")),
        Path::new("/data/piekstra.rhd/browser-profile-member-a")
    );
    // A bin that is not a plain name cannot climb out of the data dir.
    let odd = kept_profile_path(root, "my cli/../x", None);
    assert_eq!(odd, Path::new("/data/piekstra.my_cli____x/browser-profile"));
    assert_eq!(odd.parent().unwrap().parent(), Some(root));
}

#[test]
fn profile_names_are_checked_not_rewritten() {
    for good in ["a", "member-a", "member_b", "A1", &"x".repeat(64)] {
        assert!(profile_name_ok(good), "{good:?}");
    }
    for bad in ["", "a b", "a.b", "../x", "a/b", "é", &"x".repeat(65)] {
        assert!(!profile_name_ok(bad), "{bad:?}");
    }
}

#[test]
fn chromes_lock_is_judged_by_its_pid() {
    let alive = |pid: u32| pid == 4242;
    assert_eq!(judge_chrome_lock(None, alive), ChromeLock::Free);
    assert_eq!(
        judge_chrome_lock(Some("host.local-4242"), alive),
        ChromeLock::Held(4242)
    );
    // The pid follows the last `-`; host names have their own.
    assert_eq!(
        judge_chrome_lock(Some("Owners-Mac-mini.local-4242"), alive),
        ChromeLock::Held(4242)
    );
    // Another host's name is no reason to doubt a live pid.
    assert_eq!(
        judge_chrome_lock(Some("renamed-host-4242"), alive),
        ChromeLock::Held(4242)
    );
    assert_eq!(
        judge_chrome_lock(Some("host.local-4243"), alive),
        ChromeLock::Stale
    );
    // No pid to check (or pid 0, which kill(2) reads as "my group"): stale,
    // and the liveness check is never asked.
    let never = |pid: u32| panic!("asked about {pid}");
    for target in [
        "",
        "host.local",
        "host.local-",
        "host.local-0",
        "host.local-abc",
        "host.local-99999999999",
    ] {
        assert_eq!(
            judge_chrome_lock(Some(target), never),
            ChromeLock::Stale,
            "{target:?}"
        );
    }
}

#[test]
fn a_directory_is_usable_only_when_it_is_a_private_one_of_the_owners() {
    let me = 501;
    let facts = |symlink, dir, owner, mode| DirFacts {
        symlink,
        dir,
        owner,
        mode,
    };
    assert_eq!(facts(false, true, me, 0o700).problem(me, true), None);
    let cases = [
        (facts(true, false, me, 0o777), "is a symlink"),
        (facts(false, false, me, 0o600), "is not a directory"),
        (
            facts(false, true, 0, 0o700),
            "is owned by uid 0, not by you (uid 501)",
        ),
        (
            facts(false, true, me, 0o755),
            "has mode 0755, not 0700 (`chmod 700` it to use it)",
        ),
    ];
    for (facts, want) in cases {
        assert_eq!(facts.problem(me, true).as_deref(), Some(want));
    }
    // Removing one needs no particular mode: an exposed profile is the one
    // most worth removing. Everything else still applies.
    assert_eq!(facts(false, true, me, 0o755).problem(me, false), None);
    assert!(facts(false, true, 0, 0o700).problem(me, false).is_some());
    assert!(facts(true, false, me, 0o777).problem(me, false).is_some());
}

#[cfg(unix)]
#[test]
fn a_kept_profile_is_created_private_and_kept_on_drop() {
    let root = temp_root("create");
    // The data dir's own parents may be missing too (a fresh ~/.local/share).
    let data = root.join("missing").join("share");
    let profile = ProfileDir::open_kept(&data, "sfi", None).unwrap();
    let path = profile.path().to_path_buf();
    assert_eq!(path, data.join("piekstra.sfi").join("browser-profile"));
    assert_eq!(mode_of(&path), 0o700);
    assert_eq!(mode_of(path.parent().unwrap()), 0o700);
    assert_eq!(profile.scratch_path(), None, "never removed on Ctrl-C");
    std::fs::write(path.join("Cookies"), "x").unwrap();
    drop(profile);
    assert!(path.join("Cookies").exists(), "kept on drop");

    // Opened again as it is.
    let again = ProfileDir::open_kept(&data, "sfi", None).unwrap();
    assert_eq!(again.path(), path);
    assert!(path.join("Cookies").exists());
    drop(again);
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn a_kept_profile_that_is_not_private_is_refused_untouched() {
    let root = temp_root("refuse");
    let path = kept_profile_path(&root, "t", None);
    std::fs::create_dir(path.parent().unwrap()).unwrap();

    // A symlink, even to a private directory of the owner's.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    set_mode(&elsewhere, 0o700);
    std::fs::write(elsewhere.join("keep"), "x").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let err = ProfileDir::open_kept(&root, "t", None).unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("is a symlink; refusing to launch"), "{msg}");
    assert!(msg.contains(&path.display().to_string()), "{msg}");
    assert!(elsewhere.join("keep").exists());
    assert!(
        !elsewhere.join(RUN_LOCK).exists(),
        "nothing written through it"
    );
    std::fs::remove_file(&path).unwrap();

    // A directory others can read.
    std::fs::create_dir(&path).unwrap();
    set_mode(&path, 0o755);
    let err = ProfileDir::open_kept(&root, "t", None).unwrap_err();
    assert!(err.to_string().contains("has mode 0755, not 0700"), "{err}");
    assert_eq!(mode_of(&path), 0o755, "left as found");
    assert!(!path.join(RUN_LOCK).exists());
    std::fs::remove_dir(&path).unwrap();

    // Not a directory at all.
    std::fs::write(&path, "x").unwrap();
    let err = ProfileDir::open_kept(&root, "t", None).unwrap_err();
    assert!(err.to_string().contains("is not a directory"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn one_sign_in_holds_the_kept_profile_at_a_time() {
    let root = temp_root("lock");
    let first = ProfileDir::open_kept(&root, "t", None).unwrap();
    let err = ProfileDir::open_kept(&root, "t", None).unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.starts_with("another t browser sign-in is using the browser profile "),
        "{msg}"
    );
    assert!(msg.ends_with("finish or cancel that one first, or pass --fresh-profile"));
    // A named profile is a profile of its own.
    let named = ProfileDir::open_kept(&root, "t", Some("other")).unwrap();
    drop(first);
    let second = ProfileDir::open_kept(&root, "t", None).unwrap();
    drop((second, named));
    std::fs::remove_dir_all(&root).unwrap();
}

/// A process to stand in for a running Chrome, and its pid once it is gone.
#[cfg(unix)]
fn running_process() -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn plant_chrome_leftovers(profile: &Path, lock_target: &str, outside: &Path) {
    use std::os::unix::fs::symlink;
    symlink(lock_target, profile.join(SINGLETON_LOCK)).unwrap();
    symlink(outside, profile.join("SingletonSocket")).unwrap();
    symlink(outside, profile.join("SingletonCookie")).unwrap();
    std::fs::write(
        profile.join("DevToolsActivePort"),
        "1111\n/devtools/browser/old\n",
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn a_running_chrome_keeps_the_profile_and_a_dead_ones_leftovers_are_cleared() {
    let root = temp_root("singleton");
    let path = kept_profile_path(&root, "t", None);
    drop(ProfileDir::open_kept(&root, "t", None).unwrap());
    let outside = root.join("chrome-socket-dir");
    std::fs::write(&outside, "not ours to touch").unwrap();

    let mut chrome = running_process();
    let pid = chrome.id();
    plant_chrome_leftovers(&path, &format!("some-host.local-{pid}"), &outside);
    let err = ProfileDir::open_kept(&root, "t", None).unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "a Chrome (pid {pid}) is still running in t's browser profile"
        )),
        "{msg}"
    );
    assert!(msg.contains("close that window"), "{msg}");
    for name in LEFTOVERS {
        assert!(
            std::fs::symlink_metadata(path.join(name)).is_ok(),
            "{name} left alone while Chrome runs"
        );
    }
    // Nor can it be forgotten from under the running Chrome.
    let err = forget_in(&root, "t", None).unwrap_err();
    assert!(err.to_string().contains(&format!("pid {pid}")), "{err}");
    assert!(path.is_dir());

    // Once that process is gone its lock is stale: cleared, launch goes on.
    chrome.kill().unwrap();
    chrome.wait().unwrap();
    let profile = ProfileDir::open_kept(&root, "t", None).unwrap();
    for name in LEFTOVERS {
        assert!(
            std::fs::symlink_metadata(path.join(name)).is_err(),
            "{name} removed"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "not ours to touch",
        "a link is removed, never what it points to"
    );
    drop(profile);

    // A lock that is not a symlink names no pid: stale too.
    std::fs::write(path.join(SINGLETON_LOCK), "x").unwrap();
    drop(ProfileDir::open_kept(&root, "t", None).unwrap());
    assert!(!path.join(SINGLETON_LOCK).exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn forgetting_removes_only_the_profile_and_says_whether_there_was_one() {
    let root = temp_root("forget");
    // None there: nothing to do, and nothing created.
    assert!(!forget_in(&root, "t", None).unwrap());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);

    let path = kept_profile_path(&root, "t", None);
    drop(ProfileDir::open_kept(&root, "t", None).unwrap());
    drop(ProfileDir::open_kept(&root, "t", Some("other")).unwrap());
    std::fs::create_dir(path.join("Default")).unwrap();
    std::fs::write(path.join("Default").join("Cookies"), "x").unwrap();
    let outside = root.join("outside");
    std::fs::write(&outside, "x").unwrap();
    std::os::unix::fs::symlink(&outside, path.join("SingletonSocket")).unwrap();
    let sibling = path.parent().unwrap().join("other-state");
    std::fs::write(&sibling, "x").unwrap();

    // While a sign-in holds it, it stays.
    let held = ProfileDir::open_kept(&root, "t", None).unwrap();
    let err = forget_in(&root, "t", None).unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    assert!(
        err.to_string()
            .ends_with("finish or cancel it, then log out again"),
        "{err}"
    );
    assert!(path.join("Default").join("Cookies").exists());
    drop(held);

    assert!(forget_in(&root, "t", None).unwrap());
    assert!(!path.exists());
    assert!(outside.exists(), "a link's target is never removed");
    assert!(sibling.exists(), "nothing beside the profile is touched");
    assert!(
        kept_profile_path(&root, "t", Some("other")).is_dir(),
        "a named profile is forgotten on its own"
    );
    assert!(!forget_in(&root, "t", None).unwrap(), "already gone");
    assert!(forget_in(&root, "t", Some("other")).unwrap());

    // An exposed profile is still removed: it is the one most worth it.
    std::fs::create_dir(&path).unwrap();
    set_mode(&path, 0o755);
    assert!(forget_in(&root, "t", None).unwrap());
    assert!(!path.exists());

    // A symlink in its place is refused, and what it points to untouched.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep"), "x").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let err = forget_in(&root, "t", None).unwrap_err();
    assert!(
        err.to_string()
            .contains("is a symlink; refusing to remove it"),
        "{err}"
    );
    assert!(elsewhere.join("keep").exists());
    assert!(
        std::fs::symlink_metadata(&path).is_ok(),
        "the link stays too"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// The give-up path: a profile that cannot be removed (here: its parent
/// made read-only) ends the retries with the error, never a hang, and
/// the warning names the path. Skipped where permissions do not bind
/// (running as root).
#[cfg(unix)]
#[test]
fn a_profile_that_cannot_be_removed_is_reported_not_hung_on() {
    let parent = std::env::temp_dir().join(format!("pk-cli-browser-stuck-{}", unpredictable()));
    std::fs::create_dir(&parent).unwrap();
    let profile = ProfileDir::create_in(&parent, "t").unwrap();
    let path = profile.path().to_path_buf();
    set_mode(&parent, 0o500);
    let started = std::time::Instant::now();
    let result = remove_dir_patiently(&path, 3, Duration::from_millis(1));
    set_mode(&parent, 0o700);
    assert!(started.elapsed() < Duration::from_secs(2));
    if let Err(e) = result {
        assert!(path.exists());
        let warning = removal_warning(&path, &e);
        assert!(warning.starts_with("warning: could not remove the browser profile "));
        assert!(warning.contains(&path.display().to_string()), "{warning}");
        assert!(
            warning.contains("may hold the signed-in session"),
            "{warning}"
        );
    }
    drop(profile);
    assert!(!path.exists(), "removed once it can be");
    // A path already gone is not an error.
    assert!(remove_dir_patiently(&path, 1, Duration::ZERO).is_ok());
    std::fs::remove_dir(&parent).unwrap();
}

#[cfg(unix)]
#[test]
fn the_profile_is_private_exclusive_and_removed_on_drop() {
    let parent = std::env::temp_dir().join(format!("pk-cli-browser-prof-{}", unpredictable()));
    std::fs::create_dir(&parent).unwrap();

    let profile = ProfileDir::create_in(&parent, "my cli/../x").unwrap();
    let path = profile.path().to_path_buf();
    assert_eq!(path.parent(), Some(parent.as_path()), "no traversal");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("my_cli____x-browser-login-"), "{name}");
    assert_eq!(mode_of(&path), 0o700);
    assert_eq!(profile.scratch_path(), Some(path.clone()));
    // Two profiles never share a name.
    let other = ProfileDir::create_in(&parent, "my cli/../x").unwrap();
    assert_ne!(other.path(), path);

    // An existing path is refused, never adopted.
    let err = create_private_dir(&path).unwrap_err();
    assert!(err.to_string().contains("must not already exist"), "{err}");

    // Dropping removes the directory and whatever Chrome wrote into it.
    std::fs::create_dir(path.join("Default")).unwrap();
    std::fs::write(path.join("Default").join("Cookies"), "x").unwrap();
    drop(profile);
    assert!(!path.exists());
    drop(other);
    std::fs::remove_dir(&parent).unwrap();
}
