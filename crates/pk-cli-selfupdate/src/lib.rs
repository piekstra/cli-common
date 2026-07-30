//! `<bin> self-update` — self-update from GitHub Releases (SPEC v1 §1.2).
//!
//! `--check` reports whether a newer version exists without installing;
//! otherwise the running binary is downloaded for this platform and replaced
//! in place. Release assets are named `<bin>-<target-triple>.tar.gz`; bake
//! the triple in with a two-line `build.rs`:
//!
//! ```no_run
//! // build.rs
//! println!("cargo:rustc-env=BUILD_TARGET={}", std::env::var("TARGET").unwrap());
//! ```
//!
//! ## Private repos
//!
//! Public repos need nothing extra. For a private repo, export a GitHub
//! token before running `self-update` — `GITHUB_TOKEN` is checked first,
//! then `GH_TOKEN` (first non-empty wins; this crate never reads a token
//! from anywhere else — no keychain, no shelling out to `gh` — to stay
//! dependency-light and side-effect-free). With a token set, the release
//! lookup is authenticated and assets are downloaded through the GitHub API
//! asset endpoint (the only way to fetch a private release's assets); with
//! no token, behavior is unchanged from a public-repo check. The token is
//! never logged.

use std::io::Read;
use std::time::Duration;

use pk_cli_core::{output, CliError};
use serde_json::{json, Value};

/// The standard `self-update` flags (SPEC v1): `--check` and `-y`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct SelfUpdateArgs {
    /// Only report whether a newer release exists; don't install it.
    #[arg(long)]
    pub check: bool,
    /// Don't prompt for confirmation before replacing the binary.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Emit the result as JSON (also implied by a global --json flag).
    #[arg(long)]
    pub json: bool,
}

/// One CLI's update identity. `target` is the built target triple
/// (`env!("BUILD_TARGET")`), `current` is `env!("CARGO_PKG_VERSION")`.
pub struct Updater {
    /// GitHub `owner/repo`.
    pub repo: String,
    /// Binary name inside the release archive.
    pub binary: String,
    /// Substring identifying this platform's release asset — a target triple
    /// (`env!("BUILD_TARGET")`) or an `<os>-<arch>` pair, whatever the
    /// repo's release workflow names assets with.
    pub target: String,
    pub current: String,
}

/// `<os>-<arch>` (e.g. `macos-aarch64`) for repos whose release assets are
/// named that way instead of with a full target triple.
pub fn os_arch() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Discover a GitHub token for authenticated API calls: `GITHUB_TOKEN` first,
/// then `GH_TOKEN`; empty values are skipped. No other sources — no
/// keychain, no `gh` invocation — so this crate stays dependency-light and
/// side-effect-free. `None` means "no token", which keeps every call
/// unauthenticated and byte-identical to a public-repo check. Never logged.
fn github_token() -> Option<String> {
    github_token_from(|key| std::env::var(key).ok())
}

/// Pure precedence logic behind [`github_token`], parameterized over the
/// lookup so tests exercise it without touching the real environment.
fn github_token_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .into_iter()
        .find_map(|key| lookup(key).filter(|v| !v.is_empty()))
}

/// The GitHub API asset-download endpoint for asset `id` in `repo` — the
/// only URL that works for a private repo's release assets (a bare token
/// against `browser_download_url` does not).
fn asset_api_url(repo: &str, id: i64) -> String {
    format!("https://api.github.com/repos/{repo}/releases/assets/{id}")
}

/// The message for a 404 on `releases/latest`. With a token, 404 plainly
/// means no releases. Without one, it could also mean the repo is private
/// (GitHub 404s rather than 401/403 on private-repo release lookups to
/// unauthenticated callers), so the message names both possibilities.
fn no_releases_message(repo: &str, has_token: bool) -> String {
    if has_token {
        format!("no published releases for {repo} yet — build from source")
    } else {
        format!(
            "no published releases for {repo} yet — build from source (or the repo is private — export GITHUB_TOKEN to check private repos)"
        )
    }
}

pub struct UpdateCheck {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub release_url: Option<String>,
    release: Value,
}

impl UpdateCheck {
    /// The `self-update/v1` DTO (SPEC v1 §1.4).
    pub fn to_json(&self) -> Value {
        json!({
            "schema": "self-update/v1",
            "current": self.current,
            "latest": self.latest,
            "update_available": self.available,
            "release_url": self.release_url,
        })
    }
}

impl Updater {
    fn ua(&self) -> String {
        format!("{}/{}", self.binary, self.current)
    }

    fn http(&self) -> Result<reqwest::blocking::Client, CliError> {
        reqwest::blocking::Client::builder()
            .user_agent(self.ua())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| CliError::Other(format!("failed to build HTTP client: {e}")))
    }

    pub fn check(&self) -> Result<UpdateCheck, CliError> {
        let release = self.latest_release()?;
        let tag = release
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let latest = tag.trim_start_matches('v').to_string();
        Ok(UpdateCheck {
            current: self.current.clone(),
            available: version_gt(&latest, &self.current),
            release_url: release
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(String::from),
            latest,
            release,
        })
    }

    /// Download the release asset for this platform and atomically replace
    /// the running binary.
    pub fn install(&self, check: &UpdateCheck) -> Result<(), CliError> {
        let token = github_token();
        let url = self
            .find_asset(&check.release)
            .and_then(|asset| self.asset_target(asset, token.as_deref()))
            .ok_or_else(|| {
                CliError::NotFound(format!(
                    "release v{} has no `{}-{}.tar.gz` asset",
                    check.latest, self.binary, self.target
                ))
            })?;
        let archive = self.download(&url, token.as_deref())?;
        let binary = self.extract_binary(&archive)?;
        replace_self(&self.binary, &binary)
    }

    /// The full standard `self-update` command: check, report, and (unless
    /// `--check`) install. `json_mode` is the global/local `--json` OR.
    pub fn run(&self, args: &SelfUpdateArgs, json_mode: bool, quiet: bool) -> Result<(), CliError> {
        let json_mode = json_mode || args.json;
        let check = self.check()?;

        if args.check {
            if json_mode {
                output::json(&check.to_json());
            } else if check.available {
                println!(
                    "update available: {} -> {} (run `{} self-update`)",
                    check.current, check.latest, self.binary
                );
            } else {
                println!("up to date ({})", check.current);
            }
            return Ok(());
        }

        if !check.available {
            if !quiet {
                eprintln!("already up to date ({})", check.current);
            }
            if json_mode {
                output::json(&json!({ "updated": false, "version": check.current }));
            }
            return Ok(());
        }

        if !quiet {
            eprintln!("downloading {} for {}…", check.latest, self.target);
        }
        self.install(&check)?;
        if !quiet {
            eprintln!("updated to {}", check.latest);
        }
        if json_mode {
            output::json(&json!({ "updated": true, "version": check.latest }));
        }
        Ok(())
    }

    fn latest_release(&self) -> Result<Value, CliError> {
        let token = github_token();
        let url = format!("https://api.github.com/repos/{}/releases/latest", self.repo);
        let mut req = self
            .http()?
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(t) = &token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.send().map_err(|e| CliError::Upstream(e.to_string()))?;
        if resp.status().as_u16() == 404 {
            return Err(CliError::NotFound(no_releases_message(
                &self.repo,
                token.is_some(),
            )));
        }
        if !resp.status().is_success() {
            return Err(CliError::Upstream(format!(
                "GitHub API HTTP {} checking for releases",
                resp.status().as_u16()
            )));
        }
        resp.json::<Value>()
            .map_err(|e| CliError::Other(format!("parsing GitHub release JSON: {e}")))
    }

    /// Find this platform's release asset (the `.tar.gz` whose name contains
    /// `self.target`) in a `releases/latest` (or single-release) JSON payload.
    fn find_asset<'a>(&self, release: &'a Value) -> Option<&'a Value> {
        release
            .get("assets")
            .and_then(|a| a.as_array())?
            .iter()
            .find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.contains(self.target.as_str()) && n.ends_with(".tar.gz"))
                    .unwrap_or(false)
            })
    }

    /// Where to fetch an asset's bytes from, given the current auth state.
    ///
    /// With a token: the GitHub API asset endpoint (`asset_api_url`), which
    /// works for private repos and requires `Accept: application/octet-stream`
    /// plus the `Authorization` header (added in [`Updater::download`]).
    /// Without one: the plain `browser_download_url` — unauthenticated,
    /// exactly as before private-repo support existed, so public-repo
    /// behavior is unchanged.
    fn asset_target(&self, asset: &Value, token: Option<&str>) -> Option<String> {
        if token.is_some() {
            let id = asset.get("id").and_then(|v| v.as_i64())?;
            Some(asset_api_url(&self.repo, id))
        } else {
            asset
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .map(String::from)
        }
    }

    /// Fetch asset bytes from `url`. When `token` is `Some`, `url` is
    /// expected to be the API asset endpoint: sends `Accept:
    /// application/octet-stream` and `Authorization: Bearer <token>`.
    /// GitHub's asset endpoint responds with a redirect to a different host
    /// (its blob storage); reqwest's default redirect policy strips
    /// `Authorization` (and `Cookie`/`Proxy-Authorization`) on any cross-host
    /// hop (see `reqwest::redirect::remove_sensitive_headers`), so the token
    /// is never forwarded past api.github.com. When `token` is `None`, this
    /// sends a bare `GET` with no extra headers — byte-identical to the
    /// pre-private-repo-support request.
    fn download(&self, url: &str, token: Option<&str>) -> Result<Vec<u8>, CliError> {
        let mut req = self.http()?.get(url);
        if let Some(t) = token {
            req = req
                .header("Accept", "application/octet-stream")
                .header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.send().map_err(|e| CliError::Upstream(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CliError::Upstream(format!(
                "download failed: HTTP {}",
                resp.status().as_u16()
            )));
        }
        Ok(resp
            .bytes()
            .map_err(|e| CliError::Upstream(e.to_string()))?
            .to_vec())
    }

    /// Pull the named binary out of a `.tar.gz` archive.
    fn extract_binary(&self, archive: &[u8]) -> Result<Vec<u8>, CliError> {
        let decoder = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(decoder);
        let entries = tar
            .entries()
            .map_err(|e| CliError::Other(format!("reading update archive: {e}")))?;
        for entry in entries {
            let mut entry =
                entry.map_err(|e| CliError::Other(format!("reading archive entry: {e}")))?;
            let is_bin = entry
                .path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n == self.binary.as_str()))
                .unwrap_or(false);
            if is_bin {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| CliError::Other(format!("extracting binary: {e}")))?;
                return Ok(buf);
            }
        }
        Err(CliError::NotFound(format!(
            "the release archive did not contain a `{}` binary",
            self.binary
        )))
    }
}

/// Write the new binary next to the current one and atomically swap it in.
fn replace_self(binary_name: &str, binary: &[u8]) -> Result<(), CliError> {
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Other(format!("locating current executable: {e}")))?;
    let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(".{binary_name}-update.tmp"));
    std::fs::write(&tmp, binary)
        .map_err(|e| CliError::Other(format!("writing new binary to {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| CliError::Other(format!("setting permissions: {e}")))?;
    }
    #[cfg(target_os = "macos")]
    resign_stable_identity(&tmp);
    let result = self_replace::self_replace(&tmp)
        .map_err(|e| CliError::Other(format!("replacing the running binary: {e}")));
    let _ = std::fs::remove_file(&tmp);
    result
}

/// The stable, self-signed family code-signing identity (created once by
/// `cli-common/scripts/setup-dev-signing.sh`). A keychain "Always Allow" grant
/// binds to this identity's designated requirement, so signing every installed
/// build with it keeps the grant valid across versions — a self-updated binary
/// is not re-prompted.
#[cfg(target_os = "macos")]
const CODESIGN_IDENTITY: &str = "pk-cli-codesign";

/// Best-effort re-sign of the incoming binary with [`CODESIGN_IDENTITY`].
/// Silently a no-op when the identity or `codesign` isn't available (e.g. a
/// machine that never ran `setup-dev-signing.sh`) — the OS then prompts once,
/// exactly as it would for any unsigned binary, so this never makes things worse.
#[cfg(target_os = "macos")]
fn resign_stable_identity(path: &std::path::Path) {
    // Only attempt it when the identity actually exists, so we don't shell out
    // for nothing on machines without the dev-signing setup.
    let have_identity = std::process::Command::new("/usr/bin/security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(CODESIGN_IDENTITY))
        .unwrap_or(false);
    if !have_identity {
        return;
    }
    let _ = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", CODESIGN_IDENTITY])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|x| x.trim().parse().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (
            pa.get(i).copied().unwrap_or(0),
            pb.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(version_gt("0.2.0", "0.1.0"));
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(version_gt("0.1.1", "0.1.0"));
        assert!(!version_gt("0.1.0", "0.1.0"));
        assert!(!version_gt("0.1.0", "0.2.0"));
    }

    #[test]
    fn check_dto_shape() {
        let check = UpdateCheck {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            available: true,
            release_url: Some("https://example.invalid/rel".into()),
            release: Value::Null,
        };
        let v = check.to_json();
        assert_eq!(v["schema"], "self-update/v1");
        assert_eq!(v["update_available"], true);
        assert_eq!(v["current"], "0.1.0");
    }

    // -- token discovery ---------------------------------------------------
    // Exercised through `github_token_from`'s injected lookup so these never
    // read or write the real process environment (no flakiness from a dev
    // shell or CI runner that happens to already export GITHUB_TOKEN).

    #[test]
    fn token_discovery_prefers_github_token_over_gh_token() {
        let lookup = |key: &str| match key {
            "GITHUB_TOKEN" => Some("primary".to_string()),
            "GH_TOKEN" => Some("fallback".to_string()),
            _ => None,
        };
        assert_eq!(github_token_from(lookup).as_deref(), Some("primary"));
    }

    #[test]
    fn token_discovery_falls_back_to_gh_token() {
        let lookup = |key: &str| match key {
            "GH_TOKEN" => Some("fallback".to_string()),
            _ => None,
        };
        assert_eq!(github_token_from(lookup).as_deref(), Some("fallback"));
    }

    #[test]
    fn token_discovery_ignores_empty_values() {
        // Empty GITHUB_TOKEN is skipped in favor of a non-empty GH_TOKEN.
        let lookup = |key: &str| match key {
            "GITHUB_TOKEN" => Some(String::new()),
            "GH_TOKEN" => Some("fallback".to_string()),
            _ => None,
        };
        assert_eq!(github_token_from(lookup).as_deref(), Some("fallback"));

        // All-empty-or-unset yields no token, not `Some("")`.
        let all_empty = |key: &str| match key {
            "GITHUB_TOKEN" => Some(String::new()),
            "GH_TOKEN" => Some(String::new()),
            _ => None,
        };
        assert_eq!(github_token_from(all_empty), None);
    }

    #[test]
    fn token_discovery_none_when_unset() {
        assert_eq!(github_token_from(|_| None), None);
    }

    // -- asset endpoint + error messaging -----------------------------------

    #[test]
    fn asset_api_url_construction() {
        assert_eq!(
            asset_api_url("piekstra/schwab-options-cli", 123456789),
            "https://api.github.com/repos/piekstra/schwab-options-cli/releases/assets/123456789"
        );
    }

    #[test]
    fn no_releases_message_mentions_private_repo_only_without_token() {
        let without_token = no_releases_message("piekstra/schwab-options-cli", false);
        assert!(without_token.contains("no published releases"));
        assert!(without_token.contains("private"));
        assert!(without_token.contains("GITHUB_TOKEN"));

        let with_token = no_releases_message("piekstra/schwab-options-cli", true);
        assert!(with_token.contains("no published releases"));
        assert!(!with_token.contains("private"));
        assert!(!with_token.contains("GITHUB_TOKEN"));
    }

    // -- asset resolution ----------------------------------------------------

    fn sample_release() -> Value {
        json!({
            "tag_name": "v0.1.0",
            "html_url": "https://github.com/piekstra/schwab-options-cli/releases/tag/v0.1.0",
            "assets": [
                {
                    "id": 111,
                    "name": "schwopts-aarch64-apple-darwin.tar.gz",
                    "browser_download_url": "https://github.com/piekstra/schwab-options-cli/releases/download/v0.1.0/schwopts-aarch64-apple-darwin.tar.gz"
                },
                {
                    "id": 222,
                    "name": "schwopts-x86_64-apple-darwin.tar.gz",
                    "browser_download_url": "https://github.com/piekstra/schwab-options-cli/releases/download/v0.1.0/schwopts-x86_64-apple-darwin.tar.gz"
                }
            ]
        })
    }

    fn sample_updater(target: &str) -> Updater {
        Updater {
            repo: "piekstra/schwab-options-cli".into(),
            binary: "schwopts".into(),
            target: target.into(),
            current: "0.1.0".into(),
        }
    }

    #[test]
    fn asset_target_uses_browser_url_without_token() {
        let updater = sample_updater("aarch64-apple-darwin");
        let release = sample_release();
        let asset = updater.find_asset(&release).expect("asset found");
        let url = updater.asset_target(asset, None).expect("url");
        assert_eq!(
            url,
            "https://github.com/piekstra/schwab-options-cli/releases/download/v0.1.0/schwopts-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn asset_target_uses_api_endpoint_with_token() {
        let updater = sample_updater("x86_64-apple-darwin");
        let release = sample_release();
        let asset = updater.find_asset(&release).expect("asset found");
        let url = updater
            .asset_target(asset, Some("secret-token"))
            .expect("url");
        assert_eq!(
            url,
            "https://api.github.com/repos/piekstra/schwab-options-cli/releases/assets/222"
        );
    }

    #[test]
    fn find_asset_none_when_no_match() {
        let updater = sample_updater("windows-x86_64");
        let release = sample_release();
        assert!(updater.find_asset(&release).is_none());
    }
}
