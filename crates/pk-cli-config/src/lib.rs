//! Non-secret on-disk config for the piekstra CLI family.
//!
//! Stores only preferences (default username, default account, …) at
//! `${XDG_CONFIG_HOME:-~/.config}/<app>/config.json`. Secrets never land
//! here — they live in the OS keychain (see `pk-cli-secrets`).

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

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

    pub fn save<T: Serialize>(&self, config: &T) -> Result<(), CliError> {
        let path = self.path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| CliError::Other(format!("creating {}: {e}", dir.display())))?;
        }
        let body = serde_json::to_string_pretty(config)
            .map_err(|e| CliError::Other(format!("serializing config: {e}")))?;
        fs::write(&path, body)
            .map_err(|e| CliError::Other(format!("writing {}: {e}", path.display())))
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

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Demo {
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<String>,
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
