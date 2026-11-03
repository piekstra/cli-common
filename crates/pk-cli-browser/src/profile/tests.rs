use super::*;

/// The Chrome a test's launch would run: none, so no running process is
/// taken for it by its path.
#[cfg(unix)]
const NO_CHROME: &str = "/nonexistent/chrome";

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
    for err in [
        forget_named_profile("t", "a b").unwrap_err(),
        forget_named_profile_in(Path::new("/nonexistent"), "t", "a b").unwrap_err(),
        named_profile_exists("t", "a b").unwrap_err(),
        named_profile_exists_in(Path::new("/nonexistent"), "t", "a b").unwrap_err(),
    ] {
        assert!(matches!(err, BrowserError::Invalid(_)), "{err:?}");
        assert!(
            err.to_string().contains("browser profile name `a b`"),
            "{err}"
        );
    }
}

#[test]
fn a_platform_without_a_kept_profile_signs_in_with_a_scratch_one() {
    assert_eq!(profile_choice(false, true), ProfileChoice::Kept);
    assert_eq!(profile_choice(true, true), ProfileChoice::Scratch);
    assert_eq!(profile_choice(true, false), ProfileChoice::Scratch);
    assert_eq!(
        profile_choice(false, false),
        ProfileChoice::ScratchUnsupported
    );
    assert!(UNSUPPORTED_NOTE.starts_with("note: "));
    assert!(!UNSUPPORTED_NOTE.contains('\n'), "one line");
    assert_eq!(KEPT_SUPPORTED, cfg!(unix));
}

#[test]
#[should_panic(expected = "a test reached the real data directory")]
fn tests_never_reach_the_real_data_directory() {
    let _ = default_root();
}

#[test]
#[should_panic(expected = "a test reached the real data directory")]
fn asking_about_the_real_profile_goes_through_the_guarded_directory() {
    let _ = profile_exists("t");
}

#[test]
#[should_panic(expected = "a test reached the real data directory")]
fn asking_about_a_real_named_profile_goes_through_the_guarded_directory() {
    let _ = named_profile_exists("t", "member-a");
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
    // Uppercase is spelled out, so names that differ only in case never
    // meet on a filesystem that folds case.
    assert_eq!(
        kept_profile_path(root, "rhd", Some("Alice")),
        Path::new("/data/piekstra.rhd/browser-profile-+alice")
    );
    assert_ne!(
        kept_profile_path(root, "rhd", Some("Alice")),
        kept_profile_path(root, "rhd", Some("alice"))
    );
    // A bin that is not a plain name cannot climb out of the data dir.
    let odd = kept_profile_path(root, "my cli/../x", None);
    assert_eq!(odd, Path::new("/data/piekstra.my_cli____x/browser-profile"));
    assert_eq!(odd.parent().unwrap().parent(), Some(root));
}

#[test]
fn a_name_maps_to_a_directory_no_other_name_or_case_folding_reaches() {
    assert_eq!(name_dir("alice"), "alice");
    assert_eq!(name_dir("Alice"), "+alice");
    assert_eq!(name_dir("MiXed-1_Z"), "+mi+xed-1_+z");
    // Every spelling of one name up to case, folded as APFS folds it,
    // stays distinct: no uppercase is left to fold.
    let spellings = ["ab", "Ab", "aB", "AB", "a-b", "A-b", "a_B", "a+b"];
    let dirs: Vec<String> = spellings[..7].iter().map(|n| name_dir(n)).collect();
    for (i, a) in dirs.iter().enumerate() {
        assert_eq!(a, &a.to_ascii_lowercase(), "{a}");
        for b in &dirs[i + 1..] {
            assert_ne!(a.to_ascii_lowercase(), b.to_ascii_lowercase());
        }
    }
    // `+` is never in a name, so an escaped letter cannot be forged.
    assert!(!profile_name_ok(spellings[7]));
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
    let alive = |pid: u32| match pid {
        4242 => PidNow::Chrome,
        4244 => PidNow::NotChrome,
        _ => PidNow::Gone,
    };
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
    // A live pid that is not a Chrome: the pid was reused (after a reboot,
    // say) and the lock is stale. Refusing would lock the profile out until
    // that unrelated process ended.
    assert_eq!(
        judge_chrome_lock(Some("host.local-4244"), alive),
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
fn a_chrome_is_known_by_its_program() {
    for exe in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/opt/google/chrome/chrome",
        "/opt/google/chrome/chrome (deleted)",
        "/usr/lib/chromium/chromium",
        "/snap/chromium/3000/usr/lib/chromium-browser/chrome",
        "/opt/microsoft/msedge/msedge",
    ] {
        assert!(is_chrome_exe(Path::new(exe), &[]), "{exe}");
    }
    for exe in ["/bin/sleep", "/usr/bin/zsh", "/opt/brave/brave", "/"] {
        assert!(!is_chrome_exe(Path::new(exe), &[]), "{exe}");
    }
    // A recorded or launched program counts, whatever its name.
    let known = [PathBuf::from("/opt/brave/brave")];
    assert!(is_chrome_exe(Path::new("/opt/brave/brave"), &known));
    assert!(!is_chrome_exe(Path::new("/opt/brave/brave-helper"), &known));
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
        (facts(false, true, 0, 0o700), "belongs to another user"),
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
    let profile = ProfileDir::open_kept(&data, "sfi", None, Path::new(NO_CHROME)).unwrap();
    let path = profile.path().to_path_buf();
    assert_eq!(path, data.join("piekstra.sfi").join("browser-profile"));
    assert_eq!(mode_of(&path), 0o700);
    assert_eq!(mode_of(path.parent().unwrap()), 0o700);
    assert_eq!(profile.scratch_path(), None, "never removed on Ctrl-C");
    std::fs::write(path.join("Cookies"), "x").unwrap();
    drop(profile);
    assert!(path.join("Cookies").exists(), "kept on drop");

    // Opened again as it is.
    let again = ProfileDir::open_kept(&data, "sfi", None, Path::new(NO_CHROME)).unwrap();
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
    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
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
    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
    assert!(err.to_string().contains("has mode 0755, not 0700"), "{err}");
    assert_eq!(mode_of(&path), 0o755, "left as found");
    assert!(!path.join(RUN_LOCK).exists());
    std::fs::remove_dir(&path).unwrap();

    // Not a directory at all.
    std::fs::write(&path, "x").unwrap();
    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
    assert!(err.to_string().contains("is not a directory"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn one_sign_in_holds_the_kept_profile_at_a_time() {
    let root = temp_root("lock");
    let first = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
    assert!(matches!(err, BrowserError::ProfileInUse(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.starts_with("another t browser sign-in is using the browser profile "),
        "{msg}"
    );
    assert!(msg.ends_with("finish or cancel that one first, or pass --fresh-profile"));
    // A named profile is a profile of its own.
    let named = ProfileDir::open_kept(&root, "t", Some("other"), Path::new(NO_CHROME)).unwrap();
    drop(first);
    let second = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    drop((second, named));
    std::fs::remove_dir_all(&root).unwrap();
}

/// What a process another thread is spawning holds until it execs: a copy of
/// the lock's descriptor. It must not keep the next sign-in out.
#[cfg(unix)]
#[test]
fn a_copy_of_the_locks_descriptor_does_not_hold_the_profile() {
    let root = temp_root("lock-copy");
    let first = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let copy = first.run_lock.as_ref().unwrap().0.try_clone().unwrap();
    drop(first);
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());
    drop(copy);
    std::fs::remove_dir_all(&root).unwrap();
}

/// A running process to stand in for Chrome: `sleep`, by a path the
/// tests name as the launched Chrome when it should count as one.
#[cfg(unix)]
const STAND_IN: &str = "/bin/sleep";

#[cfg(unix)]
fn running_process() -> std::process::Child {
    std::process::Command::new(STAND_IN)
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
fn assert_leftovers(path: &Path, present: bool) {
    for name in LEFTOVERS {
        assert_eq!(
            std::fs::symlink_metadata(path.join(name)).is_ok(),
            present,
            "{name}"
        );
    }
}

/// The stand-in is outside the Chromium family by name, so it counts as a
/// Chrome only when the profile's record names it: what a browser picked
/// with `Launch::chrome` or `<PREFIX>_CHROME` relies on.
#[cfg(unix)]
#[test]
fn a_recorded_browser_keeps_the_profile_and_a_stale_locks_leftovers_are_cleared() {
    let root = temp_root("singleton");
    let path = kept_profile_path(&root, "t", None);
    assert!(
        !is_chrome_exe(Path::new(STAND_IN), &[]),
        "not a family name"
    );
    // An earlier sign-in launched the stand-in: its open recorded it.
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(STAND_IN)).unwrap());
    let outside = root.join("chrome-socket-dir");
    std::fs::write(&outside, "not ours to touch").unwrap();

    // It is still running (its CLI was killed, say). Neither this launch's
    // own Chrome nor a logout knows the stand-in: the record does.
    let mut process = running_process();
    let pid = process.id();
    plant_chrome_leftovers(&path, &format!("some-host.local-{pid}"), &outside);
    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
    assert!(matches!(err, BrowserError::ProfileInUse(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "a Chrome (pid {pid}) is still running in t's browser profile"
        )),
        "{msg}"
    );
    assert!(msg.contains("close that window"), "{msg}");
    let by_hand = format!(
        "If no Chrome has it open, remove {} and try again",
        path.join(SINGLETON_LOCK).display()
    );
    assert!(msg.ends_with(&by_hand), "{msg}");
    assert_leftovers(&path, true);
    // A refused open leaves the record as it was, so a logout is refused
    // too and the profile is not removed from under the running browser.
    let err = forget_in(&root, "t", None).unwrap_err();
    assert!(matches!(err, BrowserError::ProfileInUse(_)), "{err:?}");
    assert!(err.to_string().contains(&format!("pid {pid}")), "{err}");
    assert!(err.to_string().ends_with(&by_hand), "{err}");
    assert!(path.is_dir());

    // The same live pid in a profile whose record does not name it (a pid
    // reused after a reboot): the lock is stale, cleared, and the launch
    // goes on.
    let other = kept_profile_path(&root, "t", Some("other"));
    drop(ProfileDir::open_kept(&root, "t", Some("other"), Path::new(NO_CHROME)).unwrap());
    plant_chrome_leftovers(&other, &format!("some-host.local-{pid}"), &outside);
    drop(ProfileDir::open_kept(&root, "t", Some("other"), Path::new(NO_CHROME)).unwrap());
    assert_leftovers(&other, false);
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "not ours to touch",
        "a link is removed, never what it points to"
    );

    // A pid that is gone: stale, even though the record names its program.
    process.kill().unwrap();
    process.wait().unwrap();
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());
    assert_leftovers(&path, false);

    // A lock that is not a symlink names no pid: stale too.
    std::fs::write(path.join(SINGLETON_LOCK), "x").unwrap();
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());
    assert!(!path.join(SINGLETON_LOCK).exists());
    std::fs::remove_dir_all(&root).unwrap();
}

/// A launcher that starts the browser under another program (a wrapper
/// script): once the browser is up, the program its lock's pid runs is
/// added to the record, so the next check knows it.
#[cfg(unix)]
#[test]
fn the_program_chrome_runs_as_is_recorded_once_it_is_up() {
    let root = temp_root("note");
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let mut process = running_process();
    let pid = process.id();
    std::os::unix::fs::symlink(
        format!("some-host.local-{pid}"),
        profile.path().join(SINGLETON_LOCK),
    )
    .unwrap();
    profile.note_running_chrome();
    profile.note_running_chrome();
    let mut record = std::fs::File::open(profile.path().join(RUN_LOCK)).unwrap();
    assert_eq!(
        read_record(&mut record),
        [
            PathBuf::from(NO_CHROME),
            std::fs::canonicalize(STAND_IN).unwrap()
        ],
        "what was spawned, then what runs, once"
    );
    drop(profile);

    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
    assert!(matches!(err, BrowserError::ProfileInUse(_)), "{err:?}");
    process.kill().unwrap();
    process.wait().unwrap();
    // Gone now: the next sign-in goes ahead and the record names only what
    // it launches.
    let profile = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let mut record = std::fs::File::open(profile.path().join(RUN_LOCK)).unwrap();
    assert_eq!(read_record(&mut record), [PathBuf::from(NO_CHROME)]);
    drop(profile);
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn the_record_holds_absolute_paths_one_per_line() {
    let root = temp_root("record");
    let file_path = root.join("record");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&file_path)
        .unwrap();
    assert!(read_record(&mut file).is_empty());
    let chrome = PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    write_record(
        &mut file,
        &[
            PathBuf::from("/opt/brave/brave"),
            PathBuf::from("relative/browser"),
            PathBuf::from("/a\nb"),
            chrome.clone(),
        ],
    )
    .unwrap();
    assert_eq!(
        read_record(&mut file),
        [PathBuf::from("/opt/brave/brave"), chrome]
    );
    // A shorter record replaces a longer one whole.
    write_record(&mut file, &[PathBuf::from("/x")]).unwrap();
    assert_eq!(read_record(&mut file), [PathBuf::from("/x")]);
    // Anything but an absolute path in the file is skipped.
    std::fs::write(&file_path, "junk\n\n/y\nrelative\n").unwrap();
    assert_eq!(read_record(&mut file), [PathBuf::from("/y")]);
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn names_that_differ_only_in_case_keep_their_own_profiles() {
    let root = temp_root("case");
    // Both open at once: on a filesystem that folds case (APFS) one shared
    // directory would refuse the second as busy.
    let upper = ProfileDir::open_kept(&root, "t", Some("Alice"), Path::new(NO_CHROME)).unwrap();
    let lower = ProfileDir::open_kept(&root, "t", Some("alice"), Path::new(NO_CHROME)).unwrap();
    assert_ne!(upper.path(), lower.path());
    std::fs::write(upper.path().join("whose"), "Alice").unwrap();
    std::fs::write(lower.path().join("whose"), "alice").unwrap();
    drop((upper, lower));
    // Forgetting one leaves the other.
    assert!(forget_named_profile_in(&root, "t", "Alice").unwrap());
    let lower = kept_profile_path(&root, "t", Some("alice"));
    assert_eq!(
        std::fs::read_to_string(lower.join("whose")).unwrap(),
        "alice"
    );
    assert!(!forget_named_profile_in(&root, "t", "Alice").unwrap());
    assert!(forget_named_profile_in(&root, "t", "alice").unwrap());
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
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());
    drop(ProfileDir::open_kept(&root, "t", Some("other"), Path::new(NO_CHROME)).unwrap());
    std::fs::create_dir(path.join("Default")).unwrap();
    std::fs::write(path.join("Default").join("Cookies"), "x").unwrap();
    let outside = root.join("outside");
    std::fs::write(&outside, "x").unwrap();
    std::os::unix::fs::symlink(&outside, path.join("SingletonSocket")).unwrap();
    let sibling = path.parent().unwrap().join("other-state");
    std::fs::write(&sibling, "x").unwrap();

    // While a sign-in holds it, it stays.
    let held = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let err = forget_in(&root, "t", None).unwrap_err();
    assert!(matches!(err, BrowserError::ProfileInUse(_)), "{err:?}");
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

#[cfg(unix)]
#[test]
fn whether_a_profile_exists_is_read_without_creating_anything() {
    let root = temp_root("exists");
    assert!(!profile_exists_in(&root, "t").unwrap());
    assert!(!named_profile_exists_in(&root, "t", "member-a").unwrap());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    // The data dir itself may be missing too.
    let missing = root.join("missing");
    assert!(!profile_exists_in(&missing, "t").unwrap());
    assert!(!missing.exists());

    // Each profile answers for itself, under the launch's naming of it.
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());
    assert!(profile_exists_in(&root, "t").unwrap());
    assert!(!named_profile_exists_in(&root, "t", "member-a").unwrap());
    drop(ProfileDir::open_kept(&root, "t", Some("Member-A"), Path::new(NO_CHROME)).unwrap());
    assert!(named_profile_exists_in(&root, "t", "Member-A").unwrap());
    assert!(!named_profile_exists_in(&root, "t", "member-a").unwrap());
    assert!(!profile_exists_in(&root, "other").unwrap());

    let bad = named_profile_exists_in(&root, "t", "../x").unwrap_err();
    assert!(matches!(bad, BrowserError::Invalid(_)), "{bad:?}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn asking_whether_a_profile_exists_takes_no_lock() {
    let root = temp_root("exists-lock");
    let held = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    assert!(
        profile_exists_in(&root, "t").unwrap(),
        "answers mid sign-in"
    );
    let err = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap_err();
    assert!(matches!(err, BrowserError::ProfileInUse(_)), "still held");
    drop(held);

    // A directory no sign-in has opened: found, and left as it was.
    let path = kept_profile_path(&root, "t", Some("bare"));
    std::fs::create_dir(&path).unwrap();
    set_mode(&path, 0o755);
    assert!(named_profile_exists_in(&root, "t", "bare").unwrap());
    assert!(!path.join(RUN_LOCK).exists());
    assert_eq!(mode_of(&path), 0o755, "an exposed profile is still one");
    std::fs::remove_dir_all(&root).unwrap();
}

#[cfg(unix)]
#[test]
fn what_is_no_profile_is_refused_as_a_logout_refuses_it() {
    let root = temp_root("exists-refuse");
    let path = kept_profile_path(&root, "t", None);
    std::fs::create_dir(path.parent().unwrap()).unwrap();

    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let err = profile_exists_in(&root, "t").unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains(&path.display().to_string()), "{msg}");
    assert!(msg.contains("is a symlink"), "{msg}");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    std::fs::remove_file(&path).unwrap();

    std::fs::write(&path, "x").unwrap();
    let err = profile_exists_in(&root, "t").unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    assert!(err.to_string().contains("is not a directory"), "{err}");
    std::fs::remove_dir_all(&root).unwrap();
}

/// A kept profile holding every kind of site storage, a file in each.
#[cfg(unix)]
fn plant_site_storage(profile: &Path) {
    for name in SITE_STORAGE {
        let dir = profile.join(CHROME_PROFILE).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("000003.log"), "dummy-refresh-token").unwrap();
    }
}

#[cfg(unix)]
fn assert_no_site_storage(profile: &Path) {
    for name in SITE_STORAGE {
        let path = profile.join(CHROME_PROFILE).join(name);
        assert!(std::fs::symlink_metadata(&path).is_err(), "{name}");
    }
}

/// What the option is for: script-readable storage goes, and what the
/// provider remembers the device by stays.
#[cfg(unix)]
#[test]
fn clearing_site_storage_leaves_the_rest_of_the_profile() {
    let root = temp_root("clear");
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let path = profile.path().to_path_buf();
    plant_site_storage(&path);
    let default = path.join(CHROME_PROFILE);
    let rest = [
        path.join("Local State"),
        default.join("Cookies"),
        default.join("Network").join("Cookies"),
        default.join("Preferences"),
        default.join("Cache").join("Cache_Data").join("index"),
    ];
    for file in &rest {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "x").unwrap();
    }

    profile.clear_site_storage().unwrap();
    assert_no_site_storage(&path);
    for file in &rest {
        assert_eq!(std::fs::read_to_string(file).unwrap(), "x", "{file:?}");
    }

    // Written while Chrome runs, gone once this is dropped. A file where a
    // directory belongs goes the same way.
    plant_site_storage(&path);
    std::fs::remove_dir_all(default.join("IndexedDB")).unwrap();
    std::fs::write(default.join("IndexedDB"), "x").unwrap();
    drop(profile);
    assert_no_site_storage(&path);
    for file in &rest {
        assert!(file.exists(), "{file:?}");
    }
    assert!(path.join(RUN_LOCK).exists());

    // Dropped without asking: nothing is removed.
    plant_site_storage(&path);
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());
    assert!(default.join(SITE_STORAGE[0]).exists());
    std::fs::remove_dir_all(&root).unwrap();
}

/// Chrome makes `Default` on its first run; clearing does not get ahead of it.
#[cfg(unix)]
#[test]
fn clearing_a_profile_chrome_has_not_used_creates_nothing() {
    let root = temp_root("clear-new");
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let path = profile.path().to_path_buf();
    profile.clear_site_storage().unwrap();
    assert!(!path.join(CHROME_PROFILE).exists());
    drop(profile);
    assert!(!path.join(CHROME_PROFILE).exists());
    std::fs::remove_dir_all(&root).unwrap();
}

/// A `Default` that is no directory of the owner's refuses the clear, and
/// the refused profile does not try again when dropped.
#[cfg(unix)]
#[test]
fn a_chrome_profile_that_is_not_a_directory_is_refused_untouched() {
    let root = temp_root("clear-refuse");
    let outside = root.join("outside");
    plant_site_storage(&outside);
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let default = profile.path().join(CHROME_PROFILE);

    std::os::unix::fs::symlink(outside.join(CHROME_PROFILE), &default).unwrap();
    let err = profile.clear_site_storage().unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains(&default.display().to_string()), "{msg}");
    assert!(msg.contains("is a symlink; refusing to launch"), "{msg}");
    std::fs::remove_file(&default).unwrap();

    std::fs::write(&default, "x").unwrap();
    let err = profile.clear_site_storage().unwrap_err();
    assert!(matches!(err, BrowserError::Profile(_)), "{err:?}");
    assert!(err.to_string().contains("is not a directory"), "{err}");

    std::fs::remove_file(&default).unwrap();
    std::os::unix::fs::symlink(outside.join(CHROME_PROFILE), &default).unwrap();
    drop(profile);
    for name in SITE_STORAGE {
        let kept = outside.join(CHROME_PROFILE).join(name).join("000003.log");
        assert!(kept.exists(), "{name}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}

/// A lock naming a live program the record knows, at the close: nothing is
/// removed. One naming a pid that is gone is stale, and the removal goes ahead.
#[cfg(unix)]
#[test]
fn the_close_leaves_site_storage_under_a_running_chrome_only() {
    let root = temp_root("clear-holder");
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(STAND_IN)).unwrap();
    let path = profile.path().to_path_buf();
    profile.clear_site_storage().unwrap();
    plant_site_storage(&path);
    let mut process = running_process();
    let pid = process.id();
    std::os::unix::fs::symlink(format!("some-host.local-{pid}"), path.join(SINGLETON_LOCK))
        .unwrap();
    drop(profile);
    assert!(path.join(CHROME_PROFILE).join(SITE_STORAGE[0]).exists());

    let warning = still_running_warning(&path, pid);
    assert!(warning.starts_with("warning: a Chrome (pid "), "{warning}");
    assert!(warning.contains(&path.display().to_string()), "{warning}");
    assert!(
        warning.contains("site storage was not removed"),
        "{warning}"
    );
    assert!(warning.contains("next launch clears it"), "{warning}");

    process.kill().unwrap();
    process.wait().unwrap();
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(STAND_IN)).unwrap();
    profile.clear_site_storage().unwrap();
    plant_site_storage(&path);
    std::os::unix::fs::symlink(format!("some-host.local-{pid}"), path.join(SINGLETON_LOCK))
        .unwrap();
    drop(profile);
    assert_no_site_storage(&path);
    std::fs::remove_dir_all(&root).unwrap();
}

/// The close's removal is best effort: a path that will not go neither
/// panics nor holds the CLI up, and the profile is released all the same.
/// Skipped where permissions do not bind (running as root).
#[cfg(unix)]
#[test]
fn site_storage_that_will_not_go_on_close_is_warned_about_not_hung_on() {
    if current_uid() == 0 {
        return;
    }
    let root = temp_root("clear-stuck");
    let mut profile = ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap();
    let path = profile.path().to_path_buf();
    profile.clear_site_storage().unwrap();
    plant_site_storage(&path);
    let default = path.join(CHROME_PROFILE);
    set_mode(&default, 0o500);

    let started = std::time::Instant::now();
    drop(profile);
    let elapsed = started.elapsed();
    set_mode(&default, 0o700);

    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    assert!(default.join(SITE_STORAGE[0]).exists());
    drop(ProfileDir::open_kept(&root, "t", None, Path::new(NO_CHROME)).unwrap());

    let stuck = default.join(SITE_STORAGE[0]);
    let warning = storage_warning(&stuck, &std::io::Error::other("stuck"));
    assert!(warning.starts_with("warning: could not remove the site storage "));
    assert!(warning.contains(&stuck.display().to_string()), "{warning}");
    assert!(
        warning.contains("may hold the provider's tokens"),
        "{warning}"
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
