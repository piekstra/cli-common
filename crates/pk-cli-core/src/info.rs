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
        }
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
    }
}
