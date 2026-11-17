use super::*;

/// A fresh directory per test: tests run in parallel in one process.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pk-cli-core-private-file-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        })
        .count()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_file_is_owner_only_and_never_written_through_an_existing_path() {
    let dir = Dir::new("write");
    let out = dir.path().join("x.txt");
    write(&out, b"one", Replace::Never).unwrap();
    #[cfg(unix)]
    assert_eq!(mode(&out), 0o600);
    let err = write(&out, b"two", Replace::Never).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&out).unwrap(), "one");
    write(&out, b"two", Replace::Always).unwrap();
    assert_eq!(fs::read_to_string(&out).unwrap(), "two");
    #[cfg(unix)]
    assert_eq!(mode(&out), 0o600, "a replaced file is owner-only too");
    // A symlink is replaced, never written through.
    #[cfg(unix)]
    {
        let target = dir.path().join("target");
        fs::write(&target, "keep").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = write(&link, b"x", Replace::Never).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        write(&link, b"x", Replace::Always).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert_eq!(fs::read_to_string(&link).unwrap(), "x");
    }
    let missing = dir.path().join("no-such-dir").join("x.txt");
    assert!(write(&missing, b"x", Replace::Never).is_err());
    assert!(write(&missing, b"x", Replace::Always).is_err());
    assert_eq!(temp_files(dir.path()), 0);
}

#[cfg(unix)]
#[test]
fn a_wider_file_is_replaced_by_an_owner_only_one() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("wide");
    let out = dir.path().join("config.json");
    fs::write(&out, "{}").unwrap();
    fs::set_permissions(&out, fs::Permissions::from_mode(0o644)).unwrap();
    write(&out, b"{\"a\":1}", Replace::Always).unwrap();
    assert_eq!(mode(&out), 0o600);
    assert_eq!(fs::read_to_string(&out).unwrap(), "{\"a\":1}");
}

/// A rename that fails (here, onto a directory) leaves the path as it was
/// and no temporary file.
#[test]
fn a_failed_replace_leaves_the_path_and_no_temporary_file() {
    let dir = Dir::new("rename");
    let out = dir.path().join("config.json");
    fs::create_dir(&out).unwrap();
    assert!(write(&out, b"x", Replace::Always).is_err());
    assert!(out.is_dir());
    assert_eq!(temp_files(dir.path()), 0);
}

#[cfg(unix)]
#[test]
fn a_refused_write_keeps_the_old_file_whole() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("ro");
    let out = dir.path().join("config.json");
    write(&out, b"old", Replace::Always).unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let refused = write(&out, b"new", Replace::Always);
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    // Root writes into a 0500 directory, so only a refusal is checked.
    if refused.is_err() {
        assert_eq!(fs::read_to_string(&out).unwrap(), "old");
    }
    assert_eq!(temp_files(dir.path()), 0);
}

#[test]
fn a_path_with_no_file_name_is_invalid_input() {
    let err = write(Path::new("/"), b"x", Replace::Always).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[cfg(unix)]
#[test]
fn created_directories_are_owner_only_and_existing_ones_keep_their_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Dir::new("dirs");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let nested = dir.path().join("a").join("b");
    create_dir_all(&nested).unwrap();
    assert_eq!(mode(&dir.path().join("a")), 0o700);
    assert_eq!(mode(&nested), 0o700);
    assert_eq!(mode(dir.path()), 0o755);
    create_dir_all(&nested).unwrap();
}

#[test]
fn a_path_that_is_not_a_link_resolves_to_itself() {
    let dir = Dir::new("plain");
    let file = dir.path().join("config.json");
    assert_eq!(resolve_links(&file).unwrap(), file);
    fs::write(&file, "{}").unwrap();
    assert_eq!(resolve_links(&file).unwrap(), file);
}

#[cfg(unix)]
#[test]
fn links_resolve_to_their_final_target_existing_or_not() {
    use std::os::unix::fs::symlink;
    let dir = Dir::new("links");
    // A dangling relative link, reached through an absolute one.
    symlink(Path::new("dotfiles/config.json"), dir.path().join("mid")).unwrap();
    let link = dir.path().join("config.json");
    symlink(dir.path().join("mid"), &link).unwrap();
    let real = dir.path().join("dotfiles").join("config.json");
    assert_eq!(resolve_links(&link).unwrap(), real);
    assert!(!real.exists());
    // A loop is refused, not followed forever.
    symlink(dir.path().join("b"), dir.path().join("a")).unwrap();
    symlink(dir.path().join("a"), dir.path().join("b")).unwrap();
    let err = resolve_links(&dir.path().join("a")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}
