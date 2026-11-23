//! The Google backend against the fake Drive server: sign-in, the store's
//! operations, retries and token renewal. Offline; no keychain (the vault's
//! key is in memory).

#![cfg(feature = "fake-google")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pk_cli_auth::vault::{MemoryKeys, Vault};
use pk_cli_config::profile::ProfileName;
use pk_cli_core::{CliError, Reason};
use pk_cli_drive::google::fake::{FakeDrive, When, ACCOUNT, ROOT_ID};
use pk_cli_drive::google::{
    token_entry, ClientCommand, Drive, DriveCli, DriveCommand, DriveSpec, Existing, FolderCommand,
    LoginArgs, LogoutArgs, ProfileStore, ProfilesCommand, Retry, Scope, StatusArgs,
};
use pk_cli_drive::{Backend, CachePolicy, CachedRemote, RemoteRead, RemoteStore};

const PROFILE: &str = "adult1";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/google")
        .join(name)
}

struct Harness {
    fake: FakeDrive,
    tmp: tempfile::TempDir,
    keys: Arc<MemoryKeys>,
}

impl Harness {
    fn new() -> Harness {
        Harness {
            fake: FakeDrive::start(),
            tmp: tempfile::tempdir().unwrap(),
            keys: Arc::new(MemoryKeys::default()),
        }
    }

    fn cli(&self) -> DriveCli {
        DriveCli::new("drivetest", "0.0.0", "drivetest drive")
            .unwrap()
            .with_vault(Vault::new(
                Arc::clone(&self.keys),
                self.tmp.path().join("vault"),
            ))
            .with_profiles(ProfileStore::at(self.tmp.path().join("drive.json")))
            .with_lock(self.tmp.path().join("refresh.lock"))
            .with_endpoints(self.fake.endpoints())
            .with_retry(Retry::IMMEDIATE)
            .default_scopes(&[Scope::File])
            // The "browser" follows the fake's approval to the loopback
            // listener, from another thread: login waits on that listener.
            .with_browser(|url| {
                let url = url.to_string();
                std::thread::spawn(move || {
                    let _ = reqwest::blocking::get(url);
                });
                true
            })
    }

    fn login(&self, args: LoginArgs) -> Result<(), CliError> {
        self.cli()
            .run(&DriveCommand::Login(args), Some(PROFILE), true, false)
    }

    fn signed_in(&self) {
        self.login(LoginArgs {
            client_file: Some(fixture("desktop-client.json")),
            wait: 1,
            ..LoginArgs::default()
        })
        .expect("sign-in against the fake");
    }

    fn drive(&self) -> Drive {
        let spec = DriveSpec::new(ProfileName::parse(PROFILE).unwrap(), ROOT_ID).unwrap();
        self.cli().drive(&spec).unwrap().without_backoff()
    }

    fn profiles(&self) -> ProfileStore {
        ProfileStore::at(self.tmp.path().join("drive.json"))
    }

    fn vault(&self) -> Vault {
        Vault::new(Arc::clone(&self.keys), self.tmp.path().join("vault"))
    }
}

fn reason(e: &CliError) -> Option<Reason> {
    e.reason()
}

#[test]
fn login_verifies_the_account_and_records_the_profile() {
    let h = Harness::new();
    h.signed_in();
    let name = ProfileName::parse(PROFILE).unwrap();
    let record = h.profiles().get(&name).unwrap().expect("recorded");
    assert_eq!(record.account.as_deref(), Some(ACCOUNT));
    assert_eq!(record.scopes, vec![Scope::File.url().to_string()]);
    assert!(record
        .client_id
        .unwrap()
        .ends_with(".apps.googleusercontent.com"));
    assert!(h.vault().contains(&token_entry(&name)).unwrap());
    let log = h.fake.requests();
    assert!(log.iter().any(|l| l.starts_with("GET /o/oauth2/v2/auth")
        && l.contains("access_type=offline")
        && l.contains("prompt=consent")
        && l.contains("code_challenge_method=S256")));
    assert!(log.iter().any(
        |l| l == "GET /drive/v3/about?fields=user(emailAddress),storageQuota"
            || l.starts_with("GET /drive/v3/about")
    ));
    // Status reads the stored sign-in and asks Drive once.
    h.cli()
        .run(
            &DriveCommand::Status(StatusArgs::default()),
            Some(PROFILE),
            true,
            false,
        )
        .unwrap();
}

#[test]
fn a_sign_in_to_another_account_is_refused_and_nothing_is_stored() {
    let h = Harness::new();
    h.fake.sign_in_as("someone-else@example.test");
    let err = h
        .login(LoginArgs {
            client_file: Some(fixture("desktop-client.json")),
            account: Some(ACCOUNT.into()),
            wait: 1,
            ..LoginArgs::default()
        })
        .unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(
        err.to_string().contains("someone-else@example.test"),
        "{err}"
    );
    let name = ProfileName::parse(PROFILE).unwrap();
    assert!(!h.vault().contains(&token_entry(&name)).unwrap());
    assert_eq!(h.profiles().get(&name).unwrap(), None);
}

#[test]
fn a_grant_without_the_scope_asked_for_is_refused() {
    let h = Harness::new();
    h.fake.grant_scope("openid");
    let err = h
        .login(LoginArgs {
            client_file: Some(fixture("desktop-client.json")),
            wait: 1,
            ..LoginArgs::default()
        })
        .unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(err.to_string().contains("drive.file"), "{err}");
    let name = ProfileName::parse(PROFILE).unwrap();
    assert!(!h.vault().contains(&token_entry(&name)).unwrap());
}

#[test]
fn a_profile_with_no_client_names_the_import() {
    let h = Harness::new();
    let err = h
        .login(LoginArgs {
            wait: 1,
            ..LoginArgs::default()
        })
        .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    let remedy = err.remedy().unwrap_or_default().to_string();
    assert!(remedy.contains("drivetest drive client import"), "{remedy}");
}

#[test]
fn a_store_on_an_unconfigured_profile_names_the_sign_in() {
    let h = Harness::new();
    let spec = DriveSpec::new(ProfileName::parse("nobody").unwrap(), ROOT_ID).unwrap();
    let Err(err) = h.cli().drive(&spec) else {
        panic!("an unconfigured profile opened a store");
    };
    assert_eq!(err.exit_code(), 2);
    assert!(err
        .remedy()
        .unwrap_or_default()
        .contains("drivetest drive login --profile nobody"));
    let Err(err) = h.cli().profile_flag("--drive-profile").drive(&spec) else {
        panic!("an unconfigured profile opened a store");
    };
    assert!(err
        .remedy()
        .unwrap_or_default()
        .contains("drivetest drive login --drive-profile nobody"));
}

#[test]
fn writes_replace_in_place_and_reads_and_listings_carry_md5() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    assert_eq!(d.cat("state/index.json").unwrap(), None);
    d.write("state/index.json", "{\"v\":1}").unwrap();
    let first = h.fake.find("state/index.json").expect("made");
    d.write("state/index.json", "{\"v\":2}").unwrap();
    assert_eq!(
        h.fake.find("state/index.json").as_deref(),
        Some(first.as_str())
    );
    assert_eq!(h.fake.count("state", "index.json"), 1);
    assert_eq!(
        d.cat("state/index.json").unwrap().as_deref(),
        Some("{\"v\":2}")
    );
    // A fresh store (a new command) sees the same.
    let entries = h.drive().list_entries("").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].rel, "state/index.json");
    assert_eq!(entries[0].size, Some(7));
    assert_eq!(entries[0].md5, Some(md5_hex(b"{\"v\":2}")));
    assert!(entries[0].modified.is_some());
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    Md5::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn a_large_file_goes_up_in_a_resumable_session_and_comes_back_whole() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    let local = h.tmp.path().join("big.bin");
    let body: Vec<u8> = (0..(9 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(&local, &body).unwrap();
    d.copy_in(&local, "docs/big.bin").unwrap();
    let log = h.fake.requests();
    assert!(
        log.iter().any(|l| l.contains("uploadType=resumable")),
        "{log:?}"
    );
    assert_eq!(
        log.iter().filter(|l| l.contains("upload_id=")).count(),
        2,
        "two 8 MiB pieces"
    );
    assert_eq!(h.fake.content("docs/big.bin").unwrap(), body);
    let back = h.tmp.path().join("back.bin");
    d.copy_out("docs/big.bin", &back).unwrap();
    assert_eq!(std::fs::read(&back).unwrap(), body);
}

#[test]
fn a_download_above_the_piece_size_is_read_in_ranges() {
    let h = Harness::new();
    h.signed_in();
    let body: Vec<u8> = (0..(17 * 1024 * 1024)).map(|i| (i % 253) as u8).collect();
    h.fake.add_file(ROOT_ID, "scan.pdf", &body);
    let back = h.tmp.path().join("scan.pdf");
    h.drive().copy_out("scan.pdf", &back).unwrap();
    assert_eq!(std::fs::read(&back).unwrap(), body);
    let ranged = h
        .fake
        .requests()
        .iter()
        .filter(|l| l.contains("alt=media"))
        .count();
    assert_eq!(ranged, 2);
}

#[test]
fn two_files_of_one_name_are_an_error_never_a_guess() {
    let h = Harness::new();
    h.signed_in();
    h.fake.add_file(ROOT_ID, "index.json", b"one");
    h.fake.add_file(ROOT_ID, "index.json", b"two");
    let d = h.drive();
    for err in [
        d.cat("index.json").unwrap_err(),
        d.list_entries("").unwrap_err(),
        d.write("index.json", "three").unwrap_err(),
    ] {
        assert_eq!(
            reason(&err),
            Some(Reason::Provider("duplicate_name")),
            "{err}"
        );
        assert!(
            err.to_string().contains("2 items named `index.json`"),
            "{err}"
        );
    }
    assert_eq!(h.fake.count("", "index.json"), 2, "nothing was changed");
}

#[test]
fn moves_deletes_folders_and_existence() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    d.write("inbox/a.txt", "a").unwrap();
    d.write("inbox/b.txt", "b").unwrap();
    d.move_to("inbox/a.txt", "filed/2026/a.txt").unwrap();
    assert!(!d.exists("inbox/a.txt").unwrap());
    assert!(d.exists("filed/2026/a.txt").unwrap());
    assert!(d.exists("filed/2026").unwrap());
    let err = d
        .move_no_clobber("inbox/b.txt", "filed/2026/a.txt")
        .unwrap_err();
    assert!(d.exists("inbox/b.txt").unwrap(), "{err}");
    assert_eq!(h.fake.content("filed/2026/a.txt").unwrap(), b"a");
    let b_id = h.fake.find("inbox/b.txt").unwrap();
    assert!(d.delete_file("inbox/b.txt").unwrap());
    assert!(h.fake.is_trashed(&b_id), "deletes go to the trash");
    assert!(!d.delete_file("inbox/b.txt").unwrap());
    d.mkdir("empty/one").unwrap();
    let mut dirs = d.list_dirs("").unwrap();
    dirs.sort();
    assert_eq!(dirs, vec!["empty", "filed", "inbox"]);
    // A clobbering move replaces the destination, and the old one is in
    // the trash rather than a second file of the name.
    d.write("x.txt", "new").unwrap();
    d.write("y.txt", "old").unwrap();
    d.move_to("x.txt", "y.txt").unwrap();
    assert_eq!(h.fake.count("", "y.txt"), 1);
    assert_eq!(h.fake.content("y.txt").unwrap(), b"new");
}

#[test]
fn a_tree_copy_skips_what_matches_and_keep_refuses_other_bytes() {
    let h = Harness::new();
    h.signed_in();
    let local = h.tmp.path().join("tree");
    std::fs::create_dir_all(local.join("sub")).unwrap();
    std::fs::write(local.join("a.json"), "a").unwrap();
    std::fs::write(local.join("sub/b.json"), "b").unwrap();
    std::fs::write(local.join("sub/c.json"), "c").unwrap();
    let d = h.drive();
    let first = d.put_tree(&local, "copy", Existing::Keep).unwrap();
    assert_eq!((first.uploaded, first.replaced, first.unchanged), (3, 0, 0));
    let again = h.drive().put_tree(&local, "copy", Existing::Keep).unwrap();
    assert_eq!((again.uploaded, again.replaced, again.unchanged), (0, 0, 3));
    std::fs::write(local.join("sub/c.json"), "changed").unwrap();
    let err = h
        .drive()
        .put_tree(&local, "copy", Existing::Keep)
        .unwrap_err();
    assert!(err.to_string().contains("sub/c.json"), "{err}");
    assert_eq!(h.fake.content("copy/sub/c.json").unwrap(), b"c");
    let replaced = h
        .drive()
        .put_tree(&local, "copy", Existing::Replace)
        .unwrap();
    assert_eq!(
        (replaced.uploaded, replaced.replaced, replaced.unchanged),
        (0, 1, 2)
    );
    assert_eq!(h.fake.count("copy/sub", "c.json"), 1);
    let hashes = h.drive().hashes("copy").unwrap();
    assert_eq!(hashes.len(), 3);
    let back = h.tmp.path().join("back");
    assert_eq!(h.drive().fetch_tree("copy", &back).unwrap(), 3);
    assert_eq!(
        std::fs::read_to_string(back.join("sub/c.json")).unwrap(),
        "changed"
    );
    assert_eq!(
        h.drive().fetch_tree("copy", &back).unwrap(),
        0,
        "matching md5 skipped"
    );
}

#[test]
fn rate_limits_and_server_errors_are_retried() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    d.write("a.txt", "a").unwrap();
    h.fake.throttle_next("GET /drive/v3/files", 1, 2);
    h.fake.fail_next(
        "GET /drive/v3/files",
        403,
        "userRateLimitExceeded",
        1,
        When::Before,
    );
    h.fake
        .fail_next("GET /drive/v3/files", 503, "backendError", 1, When::Before);
    assert_eq!(h.drive().cat("a.txt").unwrap().as_deref(), Some("a"));
    // A 403 that is not a rate limit is not retried.
    h.fake.fail_next(
        "GET /drive/v3/files",
        403,
        "insufficientPermissions",
        1,
        When::Before,
    );
    let err = h.drive().cat("a.txt").unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
}

#[test]
fn a_create_whose_answer_was_lost_is_not_made_twice() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    d.mkdir("docs").unwrap();
    // The upload takes effect, then the answer is a 503.
    h.fake.fail_next(
        "POST /upload/drive/v3/files",
        503,
        "backendError",
        1,
        When::After,
    );
    d.write("docs/new.json", "{}").unwrap();
    assert_eq!(h.fake.count("docs", "new.json"), 1);
    // Same for a folder.
    h.fake
        .fail_next("POST /drive/v3/files", 503, "backendError", 1, When::After);
    d.mkdir("made").unwrap();
    assert_eq!(h.fake.count("", "made"), 1);
}

#[test]
fn an_expired_access_token_is_renewed_and_a_revoked_sign_in_names_the_login() {
    let h = Harness::new();
    h.signed_in();
    h.fake.expire_access_token();
    h.fake.clear_requests();
    h.drive().write("a.txt", "a").unwrap();
    assert!(h.fake.requests().iter().any(|l| l == "POST /token"));
    h.fake.expire_access_token();
    h.fake.revoke_refresh_token();
    let err = h.drive().cat("a.txt").unwrap_err();
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(err
        .remedy()
        .unwrap_or_default()
        .contains("drivetest drive login --profile adult1"));
}

#[test]
fn a_wrong_folder_id_is_not_found_rather_than_an_empty_store() {
    let h = Harness::new();
    h.signed_in();
    let spec = DriveSpec::new(ProfileName::parse(PROFILE).unwrap(), "0BnoSuchFolderAtAll").unwrap();
    let d = h.cli().drive(&spec).unwrap().without_backoff();
    let err = d.list_entries("").unwrap_err();
    assert_eq!(err.exit_code(), 4, "{err}");
    let file = h.fake.add_file(ROOT_ID, "plain.txt", b"x");
    let spec = DriveSpec::new(ProfileName::parse(PROFILE).unwrap(), file).unwrap();
    let err = h.cli().drive(&spec).unwrap().list_entries("").unwrap_err();
    assert!(err.to_string().contains("not a folder"), "{err}");
}

#[test]
fn googles_own_documents_are_left_out_of_listings_and_refused_as_downloads() {
    let h = Harness::new();
    h.signed_in();
    h.fake.add_google_doc(ROOT_ID, "notes");
    h.fake.add_file(ROOT_ID, "real.txt", b"r");
    let d = h.drive();
    let names: Vec<String> = d
        .list_entries("")
        .unwrap()
        .into_iter()
        .map(|e| e.rel)
        .collect();
    assert_eq!(names, vec!["real.txt"]);
    let err = d
        .copy_out("notes", &h.tmp.path().join("notes"))
        .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    assert!(err.to_string().contains("export it from Drive"), "{err}");
}

#[test]
fn the_cache_and_the_backend_work_over_drive() {
    let h = Harness::new();
    h.signed_in();
    let cached = CachedRemote::new(
        h.drive(),
        h.tmp.path().join("cache"),
        CachePolicy::default(),
    );
    let b = Backend::CachedDrive(cached);
    b.write_file("state/x.json", "1").unwrap();
    assert_eq!(b.read_file("state/x.json").unwrap().as_deref(), Some("1"));
    assert!(b.ensure_dir("inbox").unwrap());
    assert!(!b.ensure_dir("inbox").unwrap());
    let copy = b.local_copy("state/x.json").unwrap();
    assert_eq!(std::fs::read_to_string(&copy.path).unwrap(), "1");
    assert!(b.display_path("state/x.json").starts_with("gdrive:adult1/"));
    let direct = Backend::Drive(h.drive());
    assert_eq!(
        direct.list_files_recursive("").unwrap(),
        vec!["state/x.json"]
    );
}

#[test]
fn logout_revokes_only_when_asked_and_forget_removes_the_profile() {
    let h = Harness::new();
    h.signed_in();
    let name = ProfileName::parse(PROFILE).unwrap();
    h.cli()
        .run(
            &DriveCommand::Logout(LogoutArgs::default()),
            Some(PROFILE),
            true,
            false,
        )
        .unwrap();
    assert!(h.fake.revoked().is_empty());
    assert!(!h.vault().contains(&token_entry(&name)).unwrap());
    assert!(
        h.profiles().get(&name).unwrap().is_some(),
        "the profile stays"
    );
    h.cli()
        .run(
            &DriveCommand::Login(LoginArgs {
                wait: 1,
                ..LoginArgs::default()
            }),
            Some(PROFILE),
            true,
            false,
        )
        .expect("the stored client signs in again");
    h.cli()
        .run(
            &DriveCommand::Logout(LogoutArgs {
                revoke: true,
                forget: true,
            }),
            Some(PROFILE),
            true,
            false,
        )
        .unwrap();
    assert_eq!(h.fake.revoked().len(), 1);
    assert_eq!(h.profiles().get(&name).unwrap(), None);
}

#[test]
fn removing_a_profile_needs_force_when_nobody_can_answer() {
    let h = Harness::new();
    h.signed_in();
    let remove = |force| {
        h.cli().run(
            &DriveCommand::Profiles(ProfilesCommand::Remove {
                name: PROFILE.into(),
                force,
            }),
            None,
            true,
            false,
        )
    };
    assert_eq!(remove(false).unwrap_err().exit_code(), 6);
    remove(true).unwrap();
    let name = ProfileName::parse(PROFILE).unwrap();
    assert_eq!(h.profiles().get(&name).unwrap(), None);
    assert!(!h.vault().contains(&token_entry(&name)).unwrap());
}

#[test]
fn importing_another_client_needs_overwrite_and_drops_the_sign_in() {
    let h = Harness::new();
    h.signed_in();
    let other = h.tmp.path().join("other.json");
    let text = std::fs::read_to_string(fixture("desktop-client.json"))
        .unwrap()
        .replace(
            "exampleexampleexampleexample0abc",
            "exampleexampleexampleexample0def",
        );
    std::fs::write(&other, text).unwrap();
    let import = |overwrite| {
        h.cli().run(
            &DriveCommand::Client(ClientCommand::Import {
                file: Some(other.clone()),
                stdin: false,
                overwrite,
            }),
            Some(PROFILE),
            true,
            false,
        )
    };
    assert_eq!(import(false).unwrap_err().exit_code(), 2);
    import(true).unwrap();
    let name = ProfileName::parse(PROFILE).unwrap();
    assert!(!h.vault().contains(&token_entry(&name)).unwrap());
    let record = h.profiles().get(&name).unwrap().unwrap();
    assert!(record.client_id.unwrap().contains("0def"));
}

#[test]
fn a_store_folder_is_made_once_and_listed() {
    let h = Harness::new();
    h.signed_in();
    let create = || {
        h.cli().run(
            &DriveCommand::Folder(FolderCommand::Create {
                name: "Records".into(),
            }),
            Some(PROFILE),
            true,
            false,
        )
    };
    create().unwrap();
    create().unwrap();
    let posts = h
        .fake
        .requests()
        .iter()
        .filter(|l| l.starts_with("POST /drive/v3/files"))
        .count();
    assert_eq!(posts, 1, "the second create reuses the first folder");
    h.cli()
        .run(
            &DriveCommand::Folder(FolderCommand::List),
            Some(PROFILE),
            true,
            false,
        )
        .unwrap();
    let bad = h.cli().run(
        &DriveCommand::Folder(FolderCommand::Create { name: "a/b".into() }),
        Some(PROFILE),
        true,
        false,
    );
    assert_eq!(bad.unwrap_err().exit_code(), 2);
}
