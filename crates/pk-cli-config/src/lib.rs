//! Non-secret on-disk config for the piekstra CLI family.
//!
//! Stores only preferences (default username, default account, …) at
//! `${XDG_CONFIG_HOME:-~/.config}/<app>/config.json`. Secrets never land
//! here — they live in the OS keychain (see `pk-cli-secrets`).
//!
//! What is not secret can still be private (a profile's grants name a
//! person's account at a provider), so [`ConfigStore::save`] writes the file
//! owner-only and replaces it atomically.

pub mod profile;

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use pk_cli_core::private_file::{self, Replace};

use pk_cli_core::CliError;
use serde::{de::DeserializeOwned, Serialize};

/// A config store bound to an app directory name (usually the binary name).
pub struct ConfigStore {
    app: String,
    /// Explicit path override (the global `--config <PATH>` flag).
    override_path: Option<PathBuf>,
}

impl ConfigStore {
    pub fn new(app: impl Into<String>) -> Self {
        ConfigStore {
            app: app.into(),
            override_path: None,
        }
    }

    pub fn with_override(mut self, path: Option<impl Into<PathBuf>>) -> Self {
        self.override_path = path.map(Into::into);
        self
    }

    /// `${XDG_CONFIG_HOME:-~/.config}/<app>/config.json`, unless overridden. `~` is `HOME`,
    /// then `USERPROFILE`: Windows normally sets only the second, and a client that starts
    /// outside a shell, as Claude Desktop does, sees no other.
    pub fn path(&self) -> Result<PathBuf, CliError> {
        if let Some(p) = &self.override_path {
            return Ok(p.clone());
        }
        let base = config_base(|name| std::env::var_os(name)).ok_or_else(|| {
            CliError::Other(
                "cannot locate the home directory (HOME and USERPROFILE are unset)".into(),
            )
        })?;
        Ok(base.join(&self.app).join("config.json"))
    }

    /// Load the typed config; a missing file yields `T::default()`.
    pub fn load<T: DeserializeOwned + Default>(&self) -> Result<T, CliError> {
        let path = self.path()?;
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| CliError::Other(format!("parsing {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(CliError::Other(format!("reading {}: {e}", path.display()))),
        }
    }

    /// Write the config owner-only and atomically, through
    /// [`pk_cli_core::private_file`]: a `0600` temporary file beside it,
    /// synced, renamed over it, the directory synced. A crash leaves the old
    /// file or the new one, never part of either, and no other local account
    /// can read it. A file an earlier release left at a wider mode is
    /// replaced by a `0600` one on the first write. A directory this creates
    /// is `0700`; one that exists keeps its mode, since `--config` may name a
    /// file in a shared directory. A `config.json` that is a symlink (a
    /// dotfiles checkout's) is written through to its final target, created
    /// with its directory if missing, so the link survives.
    pub fn save<T: Serialize>(&self, config: &T) -> Result<(), CliError> {
        let path = self.path()?;
        let body = serde_json::to_string_pretty(config)
            .map_err(|e| CliError::Other(format!("serializing config: {e}")))?;
        let target = private_file::resolve_links(&path)
            .map_err(|e| CliError::Other(format!("resolving {}: {e}", path.display())))?;
        if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
            private_file::create_dir_all(dir)
                .map_err(|e| CliError::Other(format!("creating {}: {e}", dir.display())))?;
        }
        private_file::write(&target, body.as_bytes(), Replace::Always)
            .map_err(|e| CliError::Other(format!("writing {}: {e}", target.display())))
    }

    /// Remove the config file entirely (used by `auth logout --forget`).
    /// Returns `true` if a file was removed.
    pub fn clear(&self) -> Result<bool, CliError> {
        let path = self.path()?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CliError::Other(format!("removing {}: {e}", path.display()))),
        }
    }
}

/// Where configs live, from the environment `var` reads. An empty variable counts as unset.
fn config_base(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    set("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| {
        set("HOME")
            .or_else(|| set("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".config"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Demo {
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    }

    /// A fresh directory per test: tests run in parallel in one process.
    fn fresh_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pk-cli-config-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn demo() -> Demo {
        Demo {
            username: Some("user@example.com".into()),
            account: None,
        }
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn a_new_config_is_owner_only_in_an_owner_only_directory() {
        let root = fresh_dir("new");
        let path = root.join("app").join("config.json");
        let store = ConfigStore::new("test").with_override(Some(path.clone()));
        store.save(&demo()).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root.join("app")), 0o700);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(store.load::<Demo>().unwrap(), demo());
        assert!(leftovers(&root.join("app")).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_wider_file_is_tightened_and_its_directory_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fresh_dir("wide");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let store = ConfigStore::new("test").with_override(Some(path.clone()));
        store.save(&demo()).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o755);
        assert_eq!(store.load::<Demo>().unwrap(), demo());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through_and_the_link_kept() {
        let dir = fresh_dir("link");
        fs::create_dir_all(dir.join("real")).unwrap();
        let target = dir.join("real").join("config.json");
        fs::write(&target, "{}").unwrap();
        let link = dir.join("config.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let store = ConfigStore::new("test").with_override(Some(link.clone()));
        store.save(&demo()).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(mode(&target), 0o600);
        assert_eq!(store.load::<Demo>().unwrap(), demo());
        let _ = fs::remove_dir_all(dir);
    }

    /// A dotfiles link made before its file exists: the first save creates
    /// the target, and its missing directory `0700`.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_gets_its_target_created() {
        let dir = fresh_dir("dangling");
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join("config.json");
        std::os::unix::fs::symlink(Path::new("dotfiles/app/config.json"), &link).unwrap();
        let store = ConfigStore::new("test").with_override(Some(link.clone()));
        store.save(&demo()).unwrap();
        let target = dir.join("dotfiles").join("app").join("config.json");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(mode(&target), 0o600);
        assert_eq!(mode(&dir.join("dotfiles")), 0o700);
        assert_eq!(store.load::<Demo>().unwrap(), demo());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_and_leaves_no_temporary_one() {
        let dir = fresh_dir("fail");
        fs::create_dir_all(&dir).unwrap();
        // A directory where the config should be: the rename fails after the
        // temporary file is written.
        let path = dir.join("config.json");
        fs::create_dir(&path).unwrap();
        let store = ConfigStore::new("test").with_override(Some(path.clone()));
        assert!(store.save(&demo()).is_err());
        assert!(path.is_dir());
        assert!(leftovers(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_write_leaves_the_old_config_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fresh_dir("ro");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let store = ConfigStore::new("test").with_override(Some(path.clone()));
        store.save(&demo()).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let other = Demo {
            username: None,
            account: Some("acct".into()),
        };
        let refused = store.save(&other);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        // Root writes into a 0500 directory; the refusal is then not testable.
        if refused.is_err() {
            assert_eq!(store.load::<Demo>().unwrap(), demo());
        }
        assert!(leftovers(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    fn temp_store() -> (ConfigStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pk-cli-config-test-{}", std::process::id()));
        let path = dir.join("config.json");
        (
            ConfigStore::new("test").with_override(Some(path.clone())),
            dir,
        )
    }

    #[test]
    fn roundtrip_and_clear() {
        let (store, dir) = temp_store();
        assert_eq!(store.load::<Demo>().unwrap(), Demo::default());
        let cfg = Demo {
            username: Some("user@example.com".into()),
            account: None,
        };
        store.save(&cfg).unwrap();
        assert_eq!(store.load::<Demo>().unwrap(), cfg);
        assert!(store.clear().unwrap());
        assert!(!store.clear().unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    fn base_from(vars: &[(&str, &str)]) -> Option<PathBuf> {
        config_base(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn the_config_base_is_xdg_then_home_then_userprofile() {
        let xdg = ("XDG_CONFIG_HOME", "/x");
        let home = ("HOME", "/h");
        let profile = ("USERPROFILE", "/p");
        assert_eq!(base_from(&[xdg, home, profile]), Some(PathBuf::from("/x")));
        assert_eq!(
            base_from(&[home, profile]),
            Some(PathBuf::from("/h").join(".config"))
        );
        assert_eq!(
            base_from(&[profile]),
            Some(PathBuf::from("/p").join(".config"))
        );
        assert_eq!(base_from(&[]), None);
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            base_from(&[("XDG_CONFIG_HOME", ""), ("HOME", ""), ("USERPROFILE", "/p")]),
            Some(PathBuf::from("/p").join(".config"))
        );
        assert_eq!(base_from(&[("HOME", "")]), None);
    }

    #[test]
    fn default_path_is_xdg() {
        let store = ConfigStore::new("demo-app");
        let p = store.path().unwrap();
        assert!(p.ends_with("demo-app/config.json"));
    }
}
