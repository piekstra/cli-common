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
        let asset_url = self.asset_download_url(&check.release).ok_or_else(|| {
            CliError::NotFound(format!(
                "release v{} has no `{}-{}.tar.gz` asset",
                check.latest, self.binary, self.target
            ))
        })?;
        let archive = self.download(&asset_url)?;
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
        let url = format!("https://api.github.com/repos/{}/releases/latest", self.repo);
        let resp = self
            .http()?
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .map_err(|e| CliError::Upstream(e.to_string()))?;
        if resp.status().as_u16() == 404 {
            return Err(CliError::NotFound(format!(
                "no published releases for {} yet — build from source",
                self.repo
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

    fn asset_download_url(&self, release: &Value) -> Option<String> {
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
            .and_then(|a| a.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .map(String::from)
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, CliError> {
        let resp = self
            .http()?
            .get(url)
            .send()
            .map_err(|e| CliError::Upstream(e.to_string()))?;
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
}
