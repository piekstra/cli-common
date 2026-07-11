//! Non-secret on-disk config for the piekstra CLI family.
//!
//! Stores only preferences (default username, default account, …) at
//! `${XDG_CONFIG_HOME:-~/.config}/<app>/config.json`. Secrets never land
//! here — they live in the OS keychain (see `pk-cli-secrets`).

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

    /// `${XDG_CONFIG_HOME:-~/.config}/<app>/config.json`, unless overridden.
    pub fn path(&self) -> Result<PathBuf, CliError> {
        if let Some(p) = &self.override_path {
            return Ok(p.clone());
        }
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(x) if !x.is_empty() => PathBuf::from(x),
            _ => {
                let home = std::env::var("HOME").map_err(|_| {
                    CliError::Other("cannot locate home directory ($HOME unset)".into())
                })?;
                PathBuf::from(home).join(".config")
            }
        };
        Ok(base.join(&self.app).join("config.json"))
    }

    /// Load the typed config; a missing file yields `T::default()`.
    pub fn load<T: DeserializeOwned + Default>(&self) -> Result<T, CliError> {
        let path = self.path()?;
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| CliError::Other(format!("parsing {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(CliError::Other(format!(
                "reading {}: {e}",
                path.display()
            ))),
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
            Err(e) => Err(CliError::Other(format!(
                "removing {}: {e}",
                path.display()
            ))),
        }
    }
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

    #[test]
    fn default_path_is_xdg() {
        let store = ConfigStore::new("demo-app");
        let p = store.path().unwrap();
        assert!(p.ends_with("demo-app/config.json"));
    }
}
