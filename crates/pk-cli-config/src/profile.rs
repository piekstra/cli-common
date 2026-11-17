//! Profiles: one per login, for a CLI that signs in more than one person or
//! account at a provider.
//!
//! A command that uses a login names its profile (`--profile <NAME>`, with
//! `<PREFIX>_PROFILE` as the environment's fallback). There is no default
//! and a lone profile is never picked for the caller: [`resolve`] refuses a
//! missing or unknown name as a usage error (exit 2) that lists the
//! configured ones, so one login's command never runs as another's. A
//! command that needs no login (a login-free source, `info`) does not ask.
//!
//! The configured profiles are [`Profiles`], a field of the CLI's own config
//! (`#[serde(default)] profiles: Profiles`), each with an object of
//! settings the CLI shapes. `profiles list --json` is [`ProfileList`]
//! (`profile-list/v1`). Names are the household's own (`adult1`) and stay in
//! local config; a CLI keys what it keeps for a login by the name (a vault
//! entry `<profile>/…`, a browser profile `Launch::profile_name`).

use std::collections::BTreeMap;
use std::fmt;

use pk_cli_core::CliError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// The longest profile name.
pub const MAX_LEN: usize = 32;

/// A profile's name: 1 to [`MAX_LEN`] lowercase ASCII letters, digits, `-`
/// or `_`, starting with a letter or a digit. Lowercase only, so two names
/// never differ by case alone where a file system or a keychain folds it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProfileName(String);

impl ProfileName {
    /// Check `name`; anything else is exit 2, quoting it.
    pub fn parse(name: &str) -> Result<Self, CliError> {
        let ok = !name.is_empty()
            && name.len() <= MAX_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            && name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if ok {
            Ok(Self(name.to_string()))
        } else {
            Err(CliError::Usage(format!(
                "`{}` is not a profile name: use 1 to {MAX_LEN} lowercase letters, digits, \
                 `-` or `_`, starting with a letter or a digit",
                name.escape_debug()
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProfileName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        ProfileName::parse(&name).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// The configured profiles, name to the CLI's settings for it. Serialised as
/// an object, `{"adult1": {}, "adult2": {"org": "…"}}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Profiles(BTreeMap<ProfileName, Map<String, Value>>);

impl Profiles {
    pub fn new() -> Self {
        Self::default()
    }

    /// The names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &ProfileName> {
        self.0.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, name: &ProfileName) -> bool {
        self.0.contains_key(name)
    }

    /// Add `name` with no settings. `false` when it was there already, its
    /// settings kept.
    pub fn add(&mut self, name: ProfileName) -> bool {
        if self.0.contains_key(&name) {
            return false;
        }
        self.0.insert(name, Map::new());
        true
    }

    /// Remove `name` and its settings. `false` when it was not there. What
    /// the CLI keeps for the login (its sign-ins, its browser profile) is the
    /// CLI's to remove, before this.
    pub fn remove(&mut self, name: &ProfileName) -> bool {
        self.0.remove(name).is_some()
    }

    /// `name`'s settings as `T`; `T::default()` when it has none. Settings
    /// that do not read as `T` are an error naming the profile.
    pub fn settings<T: DeserializeOwned + Default>(
        &self,
        name: &ProfileName,
    ) -> Result<T, CliError> {
        match self.0.get(name) {
            Some(map) if !map.is_empty() => serde_json::from_value(Value::Object(map.clone()))
                .map_err(|e| {
                    CliError::Other(format!("the settings of profile `{name}` do not read: {e}"))
                }),
            _ => Ok(T::default()),
        }
    }

    /// Replace `name`'s settings; the profile must be configured.
    pub fn set_settings<T: Serialize>(
        &mut self,
        name: &ProfileName,
        settings: &T,
    ) -> Result<(), CliError> {
        let Some(slot) = self.0.get_mut(name) else {
            return Err(unknown(name, self));
        };
        match serde_json::to_value(settings) {
            Ok(Value::Object(map)) => {
                *slot = map;
                Ok(())
            }
            Ok(_) => Err(CliError::Other(
                "profile settings must serialise as a JSON object".into(),
            )),
            Err(e) => Err(CliError::Other(format!("encoding profile settings: {e}"))),
        }
    }

    /// The `profiles list --json` document.
    pub fn list(&self) -> ProfileList {
        ProfileList {
            schema: ProfileList::SCHEMA.to_string(),
            profiles: self
                .names()
                .map(|name| ProfileEntry {
                    name: name.to_string(),
                })
                .collect(),
        }
    }

    fn joined(&self) -> String {
        self.names()
            .map(ProfileName::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The profile a command runs as: `given` (the CLI's `--profile`), else the
/// environment variable `env_var` (`<PREFIX>_PROFILE`) when set and not
/// empty. It must be configured. Missing, malformed or unknown is exit 2,
/// listing the configured names, or, with none, naming
/// `<bin> profiles add <NAME>` as the remedy.
pub fn resolve(
    given: Option<&str>,
    env_var: &str,
    configured: &Profiles,
    bin: &str,
) -> Result<ProfileName, CliError> {
    let from_env = std::env::var(env_var).ok().filter(|v| !v.is_empty());
    resolve_from(given, from_env.as_deref(), env_var, configured, bin)
}

/// [`resolve`] with the environment's value passed in, for a test.
pub fn resolve_from(
    given: Option<&str>,
    from_env: Option<&str>,
    env_var: &str,
    configured: &Profiles,
    bin: &str,
) -> Result<ProfileName, CliError> {
    let Some(raw) = given.or(from_env) else {
        let mut message = format!("this command needs a profile: `--profile <NAME>` or {env_var}");
        if configured.is_empty() {
            message.push_str("; no profile is configured");
            return Err(CliError::Usage(message)
                .with_remedy(format!("add one with `{bin} profiles add <NAME>`")));
        }
        message.push_str(&format!("; configured: {}", configured.joined()));
        return Err(CliError::Usage(message));
    };
    let name = ProfileName::parse(raw)?;
    if !configured.contains(&name) {
        return Err(unknown(&name, configured)
            .with_remedy(format!("add it with `{bin} profiles add {name}`")));
    }
    Ok(name)
}

fn unknown(name: &ProfileName, configured: &Profiles) -> CliError {
    if configured.is_empty() {
        CliError::Usage(format!("no profile `{name}`; no profile is configured"))
    } else {
        CliError::Usage(format!(
            "no profile `{name}`; configured: {}",
            configured.joined()
        ))
    }
}

/// `profiles list --json`. Fields are added within `/v1` only as optional
/// ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileList {
    pub schema: String,
    /// Sorted by name.
    pub profiles: Vec<ProfileEntry>,
}

impl ProfileList {
    pub const SCHEMA: &'static str = "profile-list/v1";
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> ProfileName {
        ProfileName::parse(s).unwrap()
    }

    fn two() -> Profiles {
        let mut p = Profiles::new();
        p.add(name("adult2"));
        p.add(name("adult1"));
        p
    }

    #[test]
    fn names_are_lowercase_letters_digits_dash_and_underscore() {
        for ok in [
            "adult1",
            "a",
            "1st",
            "kid_one",
            "kid-one",
            &"x".repeat(MAX_LEN),
        ] {
            assert!(ProfileName::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Adult1",
            "adult 1",
            "a/b",
            "a%2Fb",
            "-a",
            "_a",
            "é",
            &"x".repeat(MAX_LEN + 1),
        ] {
            let err = ProfileName::parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}");
        }
    }

    #[test]
    fn a_missing_profile_is_exit_2_and_lists_the_configured_ones() {
        let err = resolve_from(None, None, "EX_PROFILE", &two(), "ex").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        let text = err.to_string();
        assert!(
            text.contains("--profile <NAME>") && text.contains("EX_PROFILE"),
            "{text}"
        );
        assert!(text.contains("configured: adult1, adult2"), "{text}");
    }

    #[test]
    fn a_lone_profile_is_never_picked() {
        let mut one = Profiles::new();
        one.add(name("adult1"));
        let err = resolve_from(None, None, "EX_PROFILE", &one, "ex").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("configured: adult1"), "{err}");
    }

    #[test]
    fn with_none_configured_the_remedy_is_profiles_add() {
        let err = resolve_from(None, None, "EX_PROFILE", &Profiles::new(), "ex").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.remedy(), Some("add one with `ex profiles add <NAME>`"));
    }

    #[test]
    fn the_flag_wins_over_the_environment_and_must_be_configured() {
        let p = two();
        assert_eq!(
            resolve_from(Some("adult1"), Some("adult2"), "EX_PROFILE", &p, "ex").unwrap(),
            name("adult1")
        );
        assert_eq!(
            resolve_from(None, Some("adult2"), "EX_PROFILE", &p, "ex").unwrap(),
            name("adult2")
        );
        let err = resolve_from(Some("adult3"), None, "EX_PROFILE", &p, "ex").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .contains("no profile `adult3`; configured: adult1, adult2"),
            "{err}"
        );
        let err = resolve_from(Some("Adult1"), None, "EX_PROFILE", &p, "ex").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn the_environment_is_read_and_an_empty_value_counts_as_unset() {
        // A variable no other test reads, so setting it races nothing.
        const VAR: &str = "PK_CLI_CONFIG_PROFILE_ENV_TEST";
        let p = two();
        std::env::set_var(VAR, "");
        let err = resolve(None, VAR, &p, "ex").unwrap_err();
        assert!(err.to_string().contains("needs a profile"), "{err}");
        std::env::set_var(VAR, "adult2");
        assert_eq!(resolve(None, VAR, &p, "ex").unwrap(), name("adult2"));
        assert_eq!(
            resolve(Some("adult1"), VAR, &p, "ex").unwrap(),
            name("adult1")
        );
        std::env::remove_var(VAR);
        assert!(resolve(None, VAR, &p, "ex").is_err());
    }

    #[test]
    fn profiles_keep_settings_and_serialise_as_an_object() {
        #[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
        struct Settings {
            org: Option<String>,
        }
        let mut p = two();
        assert!(!p.add(name("adult1")));
        p.set_settings(
            &name("adult2"),
            &Settings {
                org: Some("o-1".into()),
            },
        )
        .unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"adult1":{},"adult2":{"org":"o-1"}}"#);
        let back: Profiles = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.settings::<Settings>(&name("adult2"))
                .unwrap()
                .org
                .as_deref(),
            Some("o-1")
        );
        assert_eq!(
            back.settings::<Settings>(&name("adult1")).unwrap(),
            Settings::default()
        );
        assert!(serde_json::from_str::<Profiles>(r#"{"Bad":{}}"#).is_err());
        assert!(p.remove(&name("adult1")) && !p.remove(&name("adult1")));
        assert!(p
            .set_settings(&name("adult1"), &Settings::default())
            .is_err());
    }

    #[test]
    fn the_list_document_is_fixed() {
        let json = serde_json::to_value(two().list()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "schema": "profile-list/v1",
                "profiles": [{"name": "adult1"}, {"name": "adult2"}]
            })
        );
        let empty = serde_json::to_value(Profiles::new().list()).unwrap();
        assert_eq!(
            empty,
            serde_json::json!({"schema": "profile-list/v1", "profiles": []})
        );
    }
}
