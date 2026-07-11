//! HTTP plumbing for the piekstra CLI family.
//!
//! A blocking `reqwest` client builder with the family defaults (UA from
//! binary+version, cookie store, timeout), plus the standard raw `api`
//! passthrough command (SPEC v1 §1.2). Provider session logic (login dances,
//! scrape flows) stays in each CLI.

use std::time::Duration;

use pk_cli_core::CliError;
use serde_json::Value;

/// Build the family-standard blocking client: `<bin>/<version>` user agent,
/// cookie store enabled, 60s timeout.
pub fn client(binary: &str, version: &str) -> Result<reqwest::blocking::Client, CliError> {
    builder(binary, version)
        .build()
        .map_err(|e| CliError::Other(format!("failed to build HTTP client: {e}")))
}

/// The same defaults as [`client`], returned as a builder for CLIs that need
/// extra headers or redirect policies.
pub fn builder(binary: &str, version: &str) -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .user_agent(format!("{binary}/{version}"))
        .cookie_store(true)
        .timeout(Duration::from_secs(60))
}

/// The standard `api <METHOD> <PATH> [--data JSON]` arguments.
#[derive(clap::Args, Debug, Clone)]
pub struct ApiArgs {
    /// HTTP method: GET, POST, PUT, or DELETE.
    pub method: String,
    /// Path (leading slash, relative to the provider base URL) or full URL.
    pub path: String,
    /// Request body as a JSON string (for POST/PUT).
    #[arg(long)]
    pub data: Option<String>,
}

impl ApiArgs {
    /// Resolve `path` against a base URL (absolute URLs pass through).
    pub fn url(&self, base: &str) -> String {
        if self.path.starts_with("http://") || self.path.starts_with("https://") {
            self.path.clone()
        } else {
            format!("{}{}", base.trim_end_matches('/'), self.path)
        }
    }

    pub fn parsed_method(&self) -> Result<reqwest::Method, CliError> {
        match self.method.to_uppercase().as_str() {
            "GET" => Ok(reqwest::Method::GET),
            "POST" => Ok(reqwest::Method::POST),
            "PUT" => Ok(reqwest::Method::PUT),
            "DELETE" => Ok(reqwest::Method::DELETE),
            other => Err(CliError::Usage(format!(
                "unsupported HTTP method `{other}` (use GET, POST, PUT, or DELETE)"
            ))),
        }
    }

    pub fn parsed_body(&self) -> Result<Option<Value>, CliError> {
        self.data
            .as_deref()
            .map(|d| {
                serde_json::from_str(d)
                    .map_err(|e| CliError::Usage(format!("--data is not valid JSON: {e}")))
            })
            .transpose()
    }
}

/// Map a response to the exit-code contract: 401/403 → auth (3), 404 → not
/// found (4), other non-2xx → upstream (5); then parse JSON.
pub fn json_response(resp: reqwest::blocking::Response) -> Result<Value, CliError> {
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(CliError::Auth(format!(
            "provider rejected the request (HTTP {}) — try logging in again",
            status.as_u16()
        )));
    }
    if status.as_u16() == 404 {
        return Err(CliError::NotFound("HTTP 404 from provider".into()));
    }
    if !status.is_success() {
        return Err(CliError::Upstream(format!(
            "provider returned HTTP {}",
            status.as_u16()
        )));
    }
    resp.json::<Value>()
        .map_err(|e| CliError::Upstream(format!("parsing provider JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(method: &str, path: &str, data: Option<&str>) -> ApiArgs {
        ApiArgs {
            method: method.into(),
            path: path.into(),
            data: data.map(String::from),
        }
    }

    #[test]
    fn url_resolution() {
        let a = args("GET", "/v1/x", None);
        assert_eq!(
            a.url("https://api.example.com/"),
            "https://api.example.com/v1/x"
        );
        let b = args("GET", "https://other.example.com/y", None);
        assert_eq!(
            b.url("https://api.example.com"),
            "https://other.example.com/y"
        );
    }

    #[test]
    fn method_and_body_validation() {
        assert!(args("patch", "/", None).parsed_method().is_err());
        assert_eq!(
            args("post", "/", None).parsed_method().unwrap(),
            reqwest::Method::POST
        );
        assert!(args("POST", "/", Some("{not json")).parsed_body().is_err());
        assert_eq!(
            args("POST", "/", Some(r#"{"a":1}"#)).parsed_body().unwrap(),
            Some(serde_json::json!({"a":1}))
        );
    }
}
