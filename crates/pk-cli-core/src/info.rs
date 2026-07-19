//! The `info` machine-discovery DTO (`cli-info/v1`, SPEC v1 §1.6). Driver
//! tools (e.g. utiman) use this to bootstrap a provider from conventions
//! instead of a hand-written manifest.

use serde::{Deserialize, Serialize};

pub const SPEC: &str = "piekstra-cli/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliInfo {
    pub schema: String,
    pub name: String,
    pub version: String,
    pub spec: String,
    pub repo: String,
    pub auth: AuthInfo,
    pub capabilities: Vec<String>,
    /// Domain profiles this CLI conforms to (SPEC v1.1 §1.8), e.g.
    /// `utility/v1`. Additive: absent for CLIs that predate profiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthInfo {
    pub required: bool,
    /// "password" | "browser-session" | "none"
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_hint: Option<String>,
}

impl CliInfo {
    pub fn new(
        name: &str,
        version: &str,
        repo: &str,
        auth: AuthInfo,
        capabilities: &[&str],
    ) -> Self {
        CliInfo {
            schema: "cli-info/v1".into(),
            name: name.into(),
            version: version.into(),
            spec: SPEC.into(),
            repo: repo.into(),
            auth,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
            profiles: Vec::new(),
        }
    }

    /// Declare domain-profile conformance (e.g. `utility/v1`).
    pub fn with_profiles(mut self, profiles: &[&str]) -> Self {
        self.profiles = profiles.iter().map(|s| s.to_string()).collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_shape() {
        let info = CliInfo::new(
            "demo",
            "0.1.0",
            "https://github.com/piekstra/demo",
            AuthInfo {
                required: false,
                method: "none".into(),
                login_hint: None,
            },
            &["bills", "payments"],
        );
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["schema"], "cli-info/v1");
        assert_eq!(v["spec"], "piekstra-cli/1");
        assert_eq!(v["capabilities"][1], "payments");
        assert!(v["auth"].get("login_hint").is_none());
        // profiles is omitted entirely when empty (additive within v1)
        assert!(v.get("profiles").is_none());
    }

    #[test]
    fn info_declares_profiles() {
        let info = CliInfo::new(
            "demo",
            "0.1.0",
            "https://github.com/piekstra/demo",
            AuthInfo {
                required: true,
                method: "password".into(),
                login_hint: None,
            },
            &["summary", "bills"],
        )
        .with_profiles(&["utility/v1"]);
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["profiles"][0], "utility/v1");
    }
}
