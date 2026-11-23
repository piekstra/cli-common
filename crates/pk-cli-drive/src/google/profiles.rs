//! Drive profiles: one per Google account. What is not secret (the account
//! email verified at sign-in, the scopes granted, the client id) is in
//! `drive.json` beside the CLI's config; the OAuth client and the token set
//! are vault entries (`drive/<profile>/client`, `drive/<profile>/token`), so
//! the CLI's one keychain item covers every profile.

use std::path::{Path, PathBuf};

use pk_cli_config::profile::{ProfileName, Profiles};
use pk_cli_config::ConfigStore;
use pk_cli_core::CliError;
use serde::{Deserialize, Serialize};

/// What a Drive profile records about its account, none of it secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveProfile {
    /// The account's email, as Drive's `about` named it at sign-in. A later
    /// sign-in to another account is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The scopes Google granted at the last sign-in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// The OAuth client's id (not its secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// RFC 3339, the last sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: Profiles,
}

/// The vault entry holding a profile's OAuth client.
pub fn client_entry(profile: &ProfileName) -> String {
    format!("drive/{profile}/client")
}

/// The vault entry holding a profile's token set.
pub fn token_entry(profile: &ProfileName) -> String {
    format!("drive/{profile}/token")
}

/// `drive.json`: the configured Drive profiles.
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    /// `${XDG_CONFIG_HOME:-~/.config}/<bin>/drive.json`.
    pub fn for_binary(bin: &str) -> Result<Self, CliError> {
        let config = ConfigStore::new(bin).path()?;
        let dir = config.parent().unwrap_or(Path::new("."));
        Ok(Self::at(dir.join("drive.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        ProfileStore { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn store(&self) -> ConfigStore {
        ConfigStore::new("unused").with_override(Some(self.path.clone()))
    }

    pub fn load(&self) -> Result<Profiles, CliError> {
        Ok(self.store().load::<ProfileFile>()?.profiles)
    }

    pub fn save(&self, profiles: &Profiles) -> Result<(), CliError> {
        self.store().save(&ProfileFile {
            profiles: profiles.clone(),
        })
    }

    /// `name`'s record; `None` when it is not configured.
    pub fn get(&self, name: &ProfileName) -> Result<Option<DriveProfile>, CliError> {
        let profiles = self.load()?;
        if !profiles.contains(name) {
            return Ok(None);
        }
        profiles.settings(name).map(Some)
    }

    /// Add or replace `name`'s record.
    pub fn put(&self, name: &ProfileName, profile: &DriveProfile) -> Result<(), CliError> {
        let mut profiles = self.load()?;
        profiles.add(name.clone());
        profiles.set_settings(name, profile)?;
        self.save(&profiles)
    }

    /// Remove `name`; `false` when it was not there.
    pub fn remove(&self, name: &ProfileName) -> Result<bool, CliError> {
        let mut profiles = self.load()?;
        let removed = profiles.remove(name);
        if removed {
            self.save(&profiles)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_owner_only_and_names_key_the_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::at(tmp.path().join("cfg/drive.json"));
        let name = ProfileName::parse("adult1").unwrap();
        assert_eq!(store.get(&name).unwrap(), None);
        let record = DriveProfile {
            account: Some("person@example.test".into()),
            scopes: vec!["https://www.googleapis.com/auth/drive.file".into()],
            client_id: Some("1-x.apps.googleusercontent.com".into()),
            signed_in_at: Some("2026-01-02T03:04:05Z".into()),
        };
        store.put(&name, &record).unwrap();
        assert_eq!(store.get(&name).unwrap(), Some(record));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(client_entry(&name), "drive/adult1/client");
        assert_eq!(token_entry(&name), "drive/adult1/token");
        assert!(store.remove(&name).unwrap());
        assert!(!store.remove(&name).unwrap());
        assert_eq!(store.get(&name).unwrap(), None);
    }
}
