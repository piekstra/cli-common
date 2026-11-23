//! The Google backend against the fake Drive server: sign-in, the store's
//! operations, retries and token renewal. Offline; no keychain (the vault's
//! key is in memory).

#![cfg(feature = "fake-google")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pk_cli_auth::vault::{MemoryKeys, Vault};
use pk_cli_config::profile::ProfileName;
use pk_cli_core::{CliError, Reason};
use pk_cli_drive::google::fake::{FakeDrive, When, ACCOUNT, MY_DRIVE, ROOT_ID};
use pk_cli_drive::google::{
    token_entry, ClientCommand, Drive, DriveCli, DriveCommand, DriveSpec, Duplicate, Existing,
    FolderCommand, Listing, LoginArgs, LogoutArgs, ProfileStore, ProfilesCommand, Retry, Scope,
    StatusArgs,
};
use pk_cli_drive::{reason, Backend, CachePolicy, CachedRemote, RemoteRead, RemoteStore};

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

    fn drive_at(&self, spec: &DriveSpec) -> Drive {
        self.cli().drive(spec).unwrap().without_backoff()
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
    let abouts = |log: &[String]| {
        log.iter()
            .filter(|l| l.starts_with("GET /drive/v3/about"))
            .count()
    };
    assert_eq!(abouts(&log), 1, "the sign-in reads the account once");
    // Status reads the stored sign-in and asks Drive once; --local not at all.
    h.fake.clear_requests();
    h.cli()
        .run(
            &DriveCommand::Status(StatusArgs::default()),
            Some(PROFILE),
            true,
            false,
        )
        .unwrap();
    assert_eq!(abouts(&h.fake.requests()), 1);
    h.fake.clear_requests();
    h.cli()
        .run(
            &DriveCommand::Status(StatusArgs { local: true }),
            Some(PROFILE),
            true,
            false,
        )
        .unwrap();
    assert!(h.fake.requests().is_empty());
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
    assert_eq!(err.exit_code(), 2, "{err}");
    assert_eq!(reason(&err), Some(reason::OCCUPIED));
    assert!(err.to_string().contains("filed/2026/a.txt"), "{err}");
    assert!(d.exists("inbox/b.txt").unwrap());
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
    assert_eq!(reason(&err), Some(reason::EXISTING_DIFFERS));
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
            &DriveCommand::Profiles(ProfilesCommand::Remove { name: None, force }),
            Some(PROFILE),
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

#[test]
fn an_upload_session_on_another_host_gets_no_bytes_and_no_token() {
    let h = Harness::new();
    h.signed_in();
    h.fake.foreign_upload_sessions();
    let local = h.tmp.path().join("big.bin");
    std::fs::write(&local, vec![7u8; 6 * 1024 * 1024]).unwrap();
    let err = h.drive().copy_in(&local, "big.bin").unwrap_err();
    assert!(err.to_string().contains("another host"), "{err}");
    assert!(!h.fake.requests().iter().any(|l| l.contains("upload_id=")));
}

#[test]
fn a_damaged_upload_or_download_is_an_error_and_a_local_copy_survives_it() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    h.fake.corrupt_md5_next(1);
    let err = d.write("a.txt", "a").unwrap_err();
    assert!(err.to_string().contains("after the upload"), "{err}");
    d.write("b.txt", "fresh").unwrap();
    let local = h.tmp.path().join("b.txt");
    std::fs::write(&local, "an earlier copy").unwrap();
    // The listing the download checks against reports another md5.
    h.fake.corrupt_md5_next(1);
    let err = h.drive().copy_out("b.txt", &local).unwrap_err();
    assert!(err.to_string().contains("md5"), "{err}");
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "an earlier copy");
    h.drive().copy_out("b.txt", &local).unwrap();
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "fresh");
}

#[test]
fn an_update_in_place_whose_answer_was_lost_is_retried_safely() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    d.write("a.txt", "one").unwrap();
    h.fake.fail_next(
        "PATCH /upload/drive/v3/files/",
        503,
        "backendError",
        1,
        When::After,
    );
    d.write("a.txt", "two").unwrap();
    assert_eq!(h.fake.content("a.txt").unwrap(), b"two");
    assert_eq!(h.fake.count("", "a.txt"), 1);
}

#[test]
fn a_resumable_piece_that_fails_resumes_and_a_refusal_stops_at_once() {
    let h = Harness::new();
    h.signed_in();
    let local = h.tmp.path().join("big.bin");
    let body: Vec<u8> = (0..(9 * 1024 * 1024)).map(|i| (i % 249) as u8).collect();
    std::fs::write(&local, &body).unwrap();
    // Every retry of the first piece fails: the session is asked what it
    // holds, and the upload goes on from there.
    h.fake
        .fail_next("upload_id=", 503, "backendError", 7, When::Before);
    h.drive().copy_in(&local, "big.bin").unwrap();
    assert_eq!(h.fake.content("big.bin").unwrap(), body);
    // A full Drive is a refusal: no status probe, no further piece.
    h.fake.clear_requests();
    h.fake
        .fail_next("upload_id=", 403, "storageQuotaExceeded", 1, When::Before);
    let err = h.drive().copy_in(&local, "big2.bin").unwrap_err();
    assert_eq!(
        err.reason(),
        Some(Reason::Provider("drive_storage_full")),
        "{err}"
    );
    let puts = h
        .fake
        .requests()
        .iter()
        .filter(|l| l.contains("upload_id="))
        .count();
    assert_eq!(puts, 1);
}

#[test]
fn a_clobbering_move_that_fails_leaves_the_destination() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    d.write("a.txt", "new").unwrap();
    d.write("b.txt", "old").unwrap();
    // The rename of a.txt fails; the trash of the old b.txt, by its own id,
    // must not have happened first.
    let a = h.fake.find("a.txt").unwrap();
    h.fake.fail_next(
        &format!("PATCH /drive/v3/files/{a}"),
        403,
        "insufficientPermissions",
        1,
        When::Before,
    );
    d.move_to("a.txt", "b.txt").unwrap_err();
    assert_eq!(h.fake.content("b.txt").unwrap(), b"old");
    assert_eq!(h.fake.content("a.txt").unwrap(), b"new");
}

#[test]
fn a_lost_create_never_replaces_another_writers_file() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    d.mkdir("docs").unwrap();
    let docs = h.fake.find("docs").unwrap();
    h.fake.fail_next_while_another_adds(
        "POST /upload/drive/v3/files",
        503,
        &docs,
        "new.json",
        b"theirs",
    );
    let err = d.write("docs/new.json", "ours").unwrap_err();
    assert_eq!(
        err.reason(),
        Some(Reason::Provider("duplicate_name")),
        "{err}"
    );
    assert_eq!(h.fake.count("docs", "new.json"), 1);
    assert_eq!(h.fake.content("docs/new.json").unwrap(), b"theirs");
}

#[test]
fn a_tree_copy_refuses_to_land_on_a_google_document() {
    let h = Harness::new();
    h.signed_in();
    let local = h.tmp.path().join("tree");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("notes"), "n").unwrap();
    std::fs::write(local.join("a.json"), "a").unwrap();
    h.fake.add_google_doc(ROOT_ID, "notes");
    let err = h
        .drive()
        .put_tree(&local, "", Existing::Replace)
        .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    assert_eq!(reason(&err), Some(reason::GOOGLE_DOCUMENT));
    assert_eq!(h.fake.count("", "a.json"), 0, "nothing was uploaded");
}

#[test]
fn a_spec_with_a_path_is_a_store_under_that_folder() {
    let h = Harness::new();
    h.signed_in();
    let base = DriveSpec::new(ProfileName::parse(PROFILE).unwrap(), ROOT_ID).unwrap();
    let inbox = DriveSpec::parse(&format!("{base}/inbox")).unwrap();
    assert_eq!(inbox, base.join("inbox").unwrap());
    // The path must be there: reading and writing are exit 4 naming it,
    // never an empty store or a second folder of the name.
    h.fake.add_folder(ROOT_ID, "Inbox");
    let d = h.cli().drive(&inbox).unwrap().without_backoff();
    let err = d.list_entries("").unwrap_err();
    assert_eq!(err.exit_code(), 4, "{err}");
    assert!(err.to_string().contains("`inbox`"), "{err}");
    let d = h.cli().drive(&inbox).unwrap().without_backoff();
    assert_eq!(d.write("scan.pdf", "s").unwrap_err().exit_code(), 4);
    assert_eq!(h.fake.count("", "inbox"), 0);
    // exists answers false, as for any missing path, so the backend's
    // ensure_dir makes it, below the path too.
    let d = h.cli().drive(&inbox).unwrap().without_backoff();
    assert!(!d.exists("").unwrap());
    assert!(!d.exists("sub").unwrap());
    let backend = Backend::Drive(h.cli().drive(&inbox).unwrap().without_backoff());
    assert!(backend.ensure_dir("sub").unwrap());
    assert!(h.fake.find("inbox/sub").is_some());
    assert!(!backend.ensure_dir("").unwrap());
    let fresh = DriveSpec::parse(&format!("{base}/new")).unwrap();
    let backend = Backend::Drive(h.cli().drive(&fresh).unwrap().without_backoff());
    assert!(backend.ensure_dir("").unwrap());
    assert!(h.fake.find("new").is_some());
    let d = h.cli().drive(&inbox).unwrap().without_backoff();
    assert!(d.exists("").unwrap());
    d.write("scan.pdf", "s").unwrap();
    assert_eq!(h.fake.content("inbox/scan.pdf").unwrap(), b"s");
    assert_eq!(d.list_entries("").unwrap()[0].rel, "scan.pdf");
    assert_eq!(d.hashes("").unwrap()[0].rel, "scan.pdf");
    assert_eq!(d.target("scan.pdf"), format!("{base}/inbox/scan.pdf"));
    d.move_to("scan.pdf", "done/scan.pdf").unwrap();
    assert!(h.fake.find("inbox/done/scan.pdf").is_some());
}

/// A store holding a file, a Google document, a shortcut to a file and two
/// shortcuts to one folder, the targets outside the store (as a file shared
/// from another account is).
fn store_with_shortcuts(h: &Harness) {
    h.fake.add_file(ROOT_ID, "real.txt", b"r");
    h.fake.add_google_doc(ROOT_ID, "notes");
    let year = h.fake.add_folder(ROOT_ID, "reports");
    let shared = h.fake.add_file(MY_DRIVE, "report.pdf", b"shared bytes");
    h.fake.add_shortcut(&year, "shared.pdf", &shared);
    let folder = h.fake.add_folder(MY_DRIVE, "Shared folder");
    h.fake.add_file(&folder, "inside.txt", b"inside");
    h.fake.add_shortcut(ROOT_ID, "shared with me", &folder);
    h.fake.add_shortcut(&year, "again", &folder);
}

fn rels(entries: Vec<pk_cli_drive::FileEntry>) -> Vec<String> {
    entries.into_iter().map(|e| e.rel).collect()
}

#[test]
fn a_whole_listing_shows_documents_and_follows_shortcuts_as_rclone_does() {
    let h = Harness::new();
    h.signed_in();
    store_with_shortcuts(&h);
    assert_eq!(rels(h.drive().list_entries("").unwrap()), vec!["real.txt"]);

    let d = h.drive().listing(Listing::Whole);
    let entries = d.list_entries("").unwrap();
    assert_eq!(
        rels(entries.clone()),
        vec![
            "notes",
            "real.txt",
            "reports/again/inside.txt",
            "reports/shared.pdf",
            "shared with me/inside.txt"
        ]
    );
    let at = |rel: &str| entries.iter().find(|e| e.rel == rel).unwrap().clone();
    let shared = at("reports/shared.pdf");
    assert_eq!(
        shared.md5.as_deref(),
        Some(md5_hex(b"shared bytes").as_str())
    );
    assert_eq!(shared.size, Some(12));
    assert!(shared.modified.is_some());
    let notes = at("notes");
    assert_eq!((notes.md5, notes.size), (None, None));
    assert!(notes.modified.is_some());

    // What is listed can be read, in this value or a fresh one.
    let fresh = h.drive().listing(Listing::Whole);
    for d in [&d, &fresh] {
        assert_eq!(
            d.cat("reports/shared.pdf").unwrap().as_deref(),
            Some("shared bytes")
        );
        assert_eq!(
            d.cat("shared with me/inside.txt").unwrap().as_deref(),
            Some("inside")
        );
        assert!(d.exists("reports/shared.pdf").unwrap());
        assert!(d.exists("shared with me").unwrap());
    }
    assert_eq!(
        fresh.md5("reports/again/inside.txt").unwrap(),
        Some(md5_hex(b"inside"))
    );
    let out = h.tmp.path().join("shared.pdf");
    fresh.copy_out("reports/shared.pdf", &out).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"shared bytes");
    assert_eq!(
        rels(fresh.list_here("reports").unwrap()),
        vec!["shared.pdf"]
    );
    assert_eq!(
        rels(fresh.list_here("").unwrap()),
        vec!["notes", "real.txt"]
    );
    assert_eq!(fresh.list_dirs("reports").unwrap(), vec!["again"]);
    assert_eq!(
        fresh.list_dirs("").unwrap(),
        vec!["reports", "shared with me"]
    );
    let hashed: Vec<String> = fresh
        .hashes("")
        .unwrap()
        .into_iter()
        .map(|h| h.rel)
        .collect();
    assert_eq!(
        hashed,
        vec![
            "real.txt",
            "reports/again/inside.txt",
            "reports/shared.pdf",
            "shared with me/inside.txt"
        ]
    );
    let err = fresh
        .copy_out("notes", &h.tmp.path().join("notes"))
        .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    assert!(err.to_string().contains("export it from Drive"), "{err}");
}

#[test]
fn writes_moves_and_deletes_do_not_go_through_shortcuts() {
    let h = Harness::new();
    h.signed_in();
    store_with_shortcuts(&h);
    let d = h.drive().listing(Listing::Whole);
    let refused = |r: Result<(), CliError>| {
        let err = r.unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        assert_eq!(reason(&err), Some(Reason::Provider("shortcut")), "{err}");
    };
    refused(d.write("reports/shared.pdf", "x"));
    refused(d.write("shared with me/new.txt", "x"));
    refused(d.write("reports/again/inside.txt", "x"));
    refused(d.move_to("reports/shared.pdf", "moved.pdf"));
    refused(d.move_to("real.txt", "reports/shared.pdf"));
    refused(d.move_no_clobber("real.txt", "shared with me/real.txt"));
    refused(d.delete_file("reports/shared.pdf").map(|_| ()));
    refused(d.mkdir("shared with me/sub"));
    let local = h.tmp.path().join("tree");
    std::fs::create_dir_all(local.join("shared with me")).unwrap();
    std::fs::write(local.join("shared with me/new.txt"), "n").unwrap();
    std::fs::write(local.join("a.txt"), "a").unwrap();
    refused(d.put_tree(&local, "", Existing::Replace).map(|_| ()));
    assert_eq!(h.fake.count("", "a.txt"), 0, "nothing was uploaded");

    // Nothing reached a target, and the store's own files still take writes.
    assert_eq!(
        d.cat("reports/shared.pdf").unwrap().as_deref(),
        Some("shared bytes")
    );
    assert_eq!(
        d.cat("shared with me/inside.txt").unwrap().as_deref(),
        Some("inside")
    );
    assert_eq!(h.fake.count("reports", "shared.pdf"), 1);
    d.write("real.txt", "new").unwrap();
    d.write("reports/own.txt", "own").unwrap();
    assert_eq!(h.fake.content("reports/own.txt").unwrap(), b"own");
}

#[test]
fn a_shortcut_whose_target_is_gone_stops_a_whole_listing() {
    let h = Harness::new();
    h.signed_in();
    h.fake.add_file(ROOT_ID, "real.txt", b"r");
    let target = h.fake.add_file(MY_DRIVE, "gone.pdf", b"g");
    let shortcut = h.fake.add_shortcut(ROOT_ID, "gone.pdf", &target);
    h.fake.trash(&target);
    let err = h
        .drive()
        .listing(Listing::Whole)
        .list_entries("")
        .unwrap_err();
    assert_eq!(
        reason(&err),
        Some(Reason::Provider("shortcut_target_missing")),
        "{err}"
    );
    assert!(
        err.to_string().contains(&shortcut),
        "names the shortcut: {err}"
    );
    // The default listing does not look at shortcuts.
    assert_eq!(rels(h.drive().list_entries("").unwrap()), vec!["real.txt"]);
}

#[test]
fn a_shortcut_to_a_folder_above_it_is_an_error_not_an_endless_listing() {
    let h = Harness::new();
    h.signed_in();
    let sub = h.fake.add_folder(ROOT_ID, "sub");
    h.fake.add_file(&sub, "x.txt", b"x");
    h.fake.add_shortcut(&sub, "back to the top", ROOT_ID);
    let err = h
        .drive()
        .listing(Listing::Whole)
        .list_entries("")
        .unwrap_err();
    assert_eq!(
        reason(&err),
        Some(Reason::Provider("shortcut_loop")),
        "{err}"
    );
    assert_eq!(rels(h.drive().list_entries("").unwrap()), vec!["sub/x.txt"]);
}

#[test]
fn a_shortcut_named_like_a_file_beside_it_is_two_items_of_one_name() {
    let h = Harness::new();
    h.signed_in();
    h.fake.add_file(ROOT_ID, "a.pdf", b"mine");
    let other = h.fake.add_file(MY_DRIVE, "a.pdf", b"theirs");
    h.fake.add_shortcut(ROOT_ID, "a.pdf", &other);
    let d = h.drive().listing(Listing::Whole);
    for err in [
        d.list_entries("").unwrap_err(),
        d.cat("a.pdf").unwrap_err(),
        d.write("a.pdf", "x").unwrap_err(),
    ] {
        assert_eq!(
            reason(&err),
            Some(Reason::Provider("duplicate_name")),
            "{err}"
        );
    }
    assert_eq!(h.drive().cat("a.pdf").unwrap().as_deref(), Some("mine"));
}

#[test]
fn drive_clis_sharing_one_vault_read_its_key_once() {
    let h = Harness::new();
    h.signed_in();
    h.fake.add_file(ROOT_ID, "a.txt", b"a");
    let keys = Arc::new(MemoryKeys::default());
    let dir = h.tmp.path().join("shared-vault");
    // The sign-in's entries, sealed under a key in `keys` by another run.
    let writer = Vault::new(Arc::clone(&keys), &dir);
    for name in [
        token_entry(&ProfileName::parse(PROFILE).unwrap()),
        pk_cli_drive::google::client_entry(&ProfileName::parse(PROFILE).unwrap()),
    ] {
        let secret = h.vault().get(&name).unwrap().expect("signed in");
        writer.put(&name, &secret).unwrap();
    }
    let spec = DriveSpec::new(ProfileName::parse(PROFILE).unwrap(), ROOT_ID).unwrap();
    let shared = Arc::new(Vault::new(Arc::clone(&keys), &dir));
    let before = keys.reads();
    for _ in 0..3 {
        let cli = h.cli().with_vault(Arc::clone(&shared));
        let d = cli.drive(&spec).unwrap().without_backoff();
        assert_eq!(d.cat("a.txt").unwrap().as_deref(), Some("a"));
    }
    assert_eq!(keys.reads() - before, 1, "one key read for three sign-ins");
    // A vault per DriveCli reads it every time.
    let fresh_reads = keys.reads();
    for _ in 0..2 {
        let cli = h.cli().with_vault(Vault::new(Arc::clone(&keys), &dir));
        cli.drive(&spec)
            .unwrap()
            .without_backoff()
            .cat("a.txt")
            .unwrap();
    }
    assert_eq!(keys.reads() - fresh_reads, 2);
}

#[test]
fn duplicates_lists_every_shared_name_with_its_ids_and_walks_past_each() {
    let h = Harness::new();
    h.signed_in();
    let a1 = h.fake.add_file(ROOT_ID, "index.json", b"one");
    let a2 = h.fake.add_file(ROOT_ID, "index.json", b"two");
    // Two folders of one name; each is walked, and a clash inside the
    // second is found too.
    let y1 = h.fake.add_folder(ROOT_ID, "2024");
    let y2 = h.fake.add_folder(ROOT_ID, "2024");
    h.fake.add_file(&y1, "w2.pdf", b"a");
    let b1 = h.fake.add_file(&y2, "w2.pdf", b"b");
    let b2 = h.fake.add_file(&y2, "w2.pdf", b"c");
    // A file and a folder of one name read as two items, not a clash.
    h.fake.add_folder(ROOT_ID, "notes");
    h.fake.add_file(ROOT_ID, "notes", b"x");
    // A file in two folders is in each: it clashes in the first only.
    let shared = h.fake.add_file(&y1, "1099.pdf", b"s");
    h.fake.add_parent(&shared, &y2);
    let y1_own = h.fake.add_file(&y1, "1099.pdf", b"t");
    let d = h.drive();
    let found = d.duplicates("").unwrap();
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let want = vec![
        Duplicate {
            dir: "".into(),
            dir_id: ROOT_ID.into(),
            name: "2024".into(),
            folders: true,
            ids: sorted(vec![y1.clone(), y2.clone()]),
        },
        Duplicate {
            dir: "".into(),
            dir_id: ROOT_ID.into(),
            name: "index.json".into(),
            folders: false,
            ids: sorted(vec![a1, a2]),
        },
        Duplicate {
            dir: "2024".into(),
            dir_id: y1.clone(),
            name: "1099.pdf".into(),
            folders: false,
            ids: sorted(vec![shared.clone(), y1_own]),
        },
        Duplicate {
            dir: "2024".into(),
            dir_id: y2.clone(),
            name: "w2.pdf".into(),
            folders: false,
            ids: sorted(vec![b1, b2]),
        },
    ];
    assert_eq!(found, want);
    assert!(d.duplicates("missing").unwrap().is_empty());
    // The listing still stops at the first, as before.
    assert_eq!(
        reason(&d.list_entries("").unwrap_err()),
        Some(Reason::Provider("duplicate_name"))
    );
}

#[test]
fn a_whole_listings_duplicates_count_shortcuts_and_walk_their_folders_once() {
    let h = Harness::new();
    h.signed_in();
    let elsewhere = h.fake.add_folder(MY_DRIVE, "shared");
    h.fake.add_file(&elsewhere, "k1.pdf", b"one");
    h.fake.add_file(&elsewhere, "k1.pdf", b"two");
    h.fake.add_shortcut(ROOT_ID, "shared", &elsewhere);
    h.fake.add_shortcut(ROOT_ID, "again", &elsewhere);
    let mine = h.fake.add_file(ROOT_ID, "a.pdf", b"mine");
    let theirs = h.fake.add_file(MY_DRIVE, "a.pdf", b"theirs");
    let cut = h.fake.add_shortcut(ROOT_ID, "a.pdf", &theirs);
    // The default listing skips shortcuts: nothing clashes.
    assert!(h.drive().duplicates("").unwrap().is_empty());
    let found = h.drive().listing(Listing::Whole).duplicates("").unwrap();
    let names: Vec<(String, String, usize)> = found
        .iter()
        .map(|d| (d.dir.clone(), d.name.clone(), d.ids.len()))
        .collect();
    // The shared folder is walked once, under the first path that reaches it.
    assert_eq!(names.len(), 2, "{found:?}");
    assert_eq!(names[0], ("".into(), "a.pdf".into(), 2));
    assert_eq!(names[1].1, "k1.pdf");
    let mut ids = vec![mine, cut];
    ids.sort();
    assert_eq!(found[0].ids, ids);
}

#[test]
fn login_args_default_is_login_with_no_flags() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        login: LoginArgs,
    }
    let parsed = Cli::parse_from(["login"]).login;
    let default = LoginArgs::default();
    assert_eq!(default.wait, parsed.wait);
    assert_eq!(default.wait, 5);
    assert_eq!(default.no_browser, parsed.no_browser);
    assert_eq!(default.client_stdin, parsed.client_stdin);
    assert_eq!(default.client_file, parsed.client_file);
    assert_eq!(default.account, parsed.account);
    assert_eq!(default.scopes, parsed.scopes);
}

/// The ids of the untrashed items named `name` at the top of My Drive.
fn in_my_drive(h: &Harness, name: &str) -> Vec<String> {
    h.fake.children(MY_DRIVE, name)
}

#[test]
fn root_names_my_drive_for_every_operation() {
    let h = Harness::new();
    h.signed_in();
    h.fake.add_file(MY_DRIVE, "top.txt", b"top");
    let spec = DriveSpec::parse(&format!("gdrive:{PROFILE}/root")).unwrap();
    assert_eq!(spec.folder(), "root");
    assert_eq!(spec.to_string(), format!("gdrive:{PROFILE}/root"));
    let d = h.cli().drive(&spec).unwrap().without_backoff();
    assert!(d.exists("").unwrap());
    assert!(d.exists("top.txt").unwrap());
    // The fake's store folder is a folder at the top of My Drive.
    assert!(d.exists("Store").unwrap());
    d.write("scratch/a.txt", "a").unwrap();
    assert_eq!(in_my_drive(&h, "scratch").len(), 1, "made in My Drive");
    assert_eq!(
        rels(d.list_entries("").unwrap()),
        vec!["scratch/a.txt", "top.txt"]
    );
    assert_eq!(d.list_dirs("").unwrap(), vec!["Store", "scratch"]);
    assert_eq!(rels(d.list_here("").unwrap()), vec!["top.txt"]);
    assert_eq!(d.cat("top.txt").unwrap().as_deref(), Some("top"));
    assert_eq!(d.md5("top.txt").unwrap(), Some(md5_hex(b"top")));
    let hashed: Vec<String> = d.hashes("").unwrap().into_iter().map(|h| h.rel).collect();
    assert_eq!(hashed, vec!["scratch/a.txt", "top.txt"]);
    // A move into and out of My Drive's top level, then a delete there.
    d.move_no_clobber("scratch/a.txt", "a.txt").unwrap();
    assert_eq!(in_my_drive(&h, "a.txt").len(), 1);
    d.move_no_clobber("a.txt", "scratch/a.txt").unwrap();
    assert!(in_my_drive(&h, "a.txt").is_empty());
    // Trees both ways.
    let local = h.tmp.path().join("up");
    std::fs::create_dir_all(local.join("deep")).unwrap();
    std::fs::write(local.join("deep/b.txt"), b"b").unwrap();
    let copied = d.put_tree(&local, "scratch", Existing::Keep).unwrap();
    assert_eq!(copied.uploaded, 1);
    let again = h
        .drive_at(&spec)
        .put_tree(&local, "scratch", Existing::Keep)
        .unwrap();
    assert_eq!((again.uploaded, again.unchanged), (0, 1));
    let down = h.tmp.path().join("down");
    assert_eq!(d.fetch_tree("scratch", &down).unwrap(), 2);
    assert_eq!(std::fs::read(down.join("deep/b.txt")).unwrap(), b"b");
    // Duplicates at the top of My Drive name My Drive's own id.
    let second = h.fake.add_file(MY_DRIVE, "top.txt", b"other");
    let found = h.drive_at(&spec).duplicates("").unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].dir, "");
    assert_eq!(found[0].dir_id, MY_DRIVE);
    assert!(found[0].ids.contains(&second));
    assert!(h.drive_at(&spec).delete_file("scratch/a.txt").unwrap());
    // A store at a path under My Drive.
    let sub = DriveSpec::parse(&format!("gdrive:{PROFILE}/root/scratch")).unwrap();
    assert_eq!(
        rels(h.drive_at(&sub).list_entries("").unwrap()),
        vec!["deep/b.txt"]
    );
    let store = DriveSpec::parse(&format!("gdrive:{PROFILE}/root/Store")).unwrap();
    h.drive_at(&store).write("x.txt", "x").unwrap();
    assert_eq!(h.fake.content("x.txt").unwrap(), b"x");
}

#[test]
fn a_tree_download_refuses_names_that_leave_the_folder_or_land_twice() {
    let h = Harness::new();
    h.signed_in();
    let shared = h.fake.add_folder(ROOT_ID, "shared");
    h.fake.add_file(&shared, "fine.txt", b"fine");
    let down = h.tmp.path().join("down");
    for name in ["..", "/escaped.txt"] {
        let id = h.fake.add_file(&shared, name, b"theirs");
        let err = h.drive().fetch_tree("shared", &down).unwrap_err();
        assert_eq!(reason(&err), Some(reason::UNSAFE_NAME), "{name}: {err}");
        assert!(!down.exists(), "{name}: nothing was written");
        h.fake.trash(&id);
    }
    // A name holding `/` that spells a path another item already takes,
    // and two names that differ only in case.
    let sub = h.fake.add_folder(&shared, "a");
    h.fake.add_file(&sub, "b.txt", b"one");
    let slash = h.fake.add_file(&shared, "a/b.txt", b"two");
    let err = h.drive().fetch_tree("shared", &down).unwrap_err();
    assert_eq!(reason(&err), Some(reason::UNSAFE_NAME), "{err}");
    h.fake.trash(&slash);
    let upper = h.fake.add_file(&shared, "FINE.txt", b"other");
    let err = h.drive().fetch_tree("shared", &down).unwrap_err();
    assert_eq!(reason(&err), Some(reason::UNSAFE_NAME), "{err}");
    assert!(!down.exists());
    h.fake.trash(&upper);
    // A file named like the folder another file lands in.
    let file_a = h.fake.add_file(&shared, "A", b"in the way");
    let err = h.drive().fetch_tree("shared", &down).unwrap_err();
    assert_eq!(reason(&err), Some(reason::UNSAFE_NAME), "{err}");
    assert!(!down.exists());
    h.fake.trash(&file_a);
    // Google's own documents are never written, so their names do not
    // refuse the copy.
    h.fake.add_google_doc(&shared, "..");
    h.fake.add_google_doc(&shared, "Fine.TXT");
    assert_eq!(h.drive().fetch_tree("shared", &down).unwrap(), 2);
    assert_eq!(std::fs::read(down.join("a/b.txt")).unwrap(), b"one");
}

#[test]
fn only_an_empty_folder_is_trashed_and_never_through_a_shortcut() {
    let h = Harness::new();
    h.signed_in();
    let d = h.drive();
    let empty = h.fake.add_folder(ROOT_ID, "empty");
    let full = h.fake.add_folder(ROOT_ID, "full");
    let kept = h.fake.add_file(&full, "kept.txt", b"kept");
    let leftover = h.fake.add_folder(ROOT_ID, "leftover");
    let old = h.fake.add_file(&leftover, "old.txt", b"old");
    h.fake.trash(&old);
    let theirs = h.fake.add_folder(MY_DRIVE, "Theirs");
    let inner = h.fake.add_folder(&theirs, "inner");
    h.fake.add_shortcut(ROOT_ID, "linked", &theirs);
    h.fake.add_file(ROOT_ID, "plain.txt", b"p");

    assert!(d.trash_empty_folder("empty").unwrap());
    assert!(h.fake.is_trashed(&empty));
    assert!(!d.exists("empty").unwrap());
    // What is already in the trash does not count.
    assert!(d.trash_empty_folder("leftover").unwrap());
    assert!(h.fake.is_trashed(&leftover));

    let e = d.trash_empty_folder("full").unwrap_err();
    assert_eq!((e.exit_code(), reason(&e)), (2, Some(reason::NOT_EMPTY)));
    assert!(!h.fake.is_trashed(&full) && !h.fake.is_trashed(&kept));

    // A shortcut is refused under either listing. Under the whole listing
    // a path through one is refused too; under the bytes listing it is not
    // there at all.
    let whole = h.drive().listing(Listing::Whole);
    for through in ["linked", "linked/inner"] {
        let e = whole.trash_empty_folder(through).unwrap_err();
        assert_eq!(
            (e.exit_code(), reason(&e)),
            (2, Some(reason::SHORTCUT)),
            "{through}"
        );
    }
    let e = d.trash_empty_folder("linked").unwrap_err();
    assert_eq!(reason(&e), Some(reason::SHORTCUT));
    assert!(!d.trash_empty_folder("linked/inner").unwrap());
    assert!(!h.fake.is_trashed(&theirs) && !h.fake.is_trashed(&inner));

    assert!(!d.trash_empty_folder("missing").unwrap());
    assert!(
        !d.trash_empty_folder("plain.txt").unwrap(),
        "a file is no folder"
    );
    assert_eq!(d.trash_empty_folder("").unwrap_err().exit_code(), 2);

    let top = h.fake.add_folder(MY_DRIVE, "top-empty");
    let at_root = h.drive_at(&DriveSpec::parse(&format!("gdrive:{PROFILE}/root")).unwrap());
    assert!(at_root.trash_empty_folder("top-empty").unwrap());
    assert!(h.fake.is_trashed(&top));
}
