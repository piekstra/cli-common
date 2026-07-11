//! Secret handling for the piekstra CLI family (SPEC v1 §1.7).
//!
//! Runtime secrets live only in the OS keychain, under the service name
//! `piekstra.<binary>`. Getting a secret *into* the keychain is a setup-time
//! concern (`auth login` / `auth set-credential`), which ingest via stdin or
//! a named env var — never a `--value` flag (that leaks into `ps`, shell
//! history, and pasted transcripts).
//!
//! Secrets never appear in `Debug`/`Display` output and are zeroized on drop.

use std::fmt;
use std::io::Read;

use keyring::Entry;
use pk_cli_core::CliError;
use zeroize::Zeroize;

/// The `--stdin` / `--from-env <VAR>` ingestion flags, flattenable into any
/// credential-writing subcommand.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct SecretSourceArgs {
    /// Read the secret from stdin (trailing newline trimmed).
    #[arg(long)]
    pub stdin: bool,
    /// Read the secret from a named environment variable.
    #[arg(long, value_name = "VAR")]
    pub from_env: Option<String>,
}

impl SecretSourceArgs {
    /// Resolve the secret from the chosen source. `prompt_label` enables an
    /// interactive no-echo prompt as the fallback when neither flag is given;
    /// pass `None` to require an explicit source (headless commands).
    pub fn read(&self, prompt_label: Option<&str>) -> Result<Secret, CliError> {
        match (self.stdin, &self.from_env) {
            (true, Some(_)) => Err(CliError::Usage(
                "pass exactly one of --stdin or --from-env".into(),
            )),
            (true, None) => read_stdin(),
            (false, Some(var)) => read_from_env(var),
            (false, None) => match prompt_label {
                Some(label) => {
                    use std::io::IsTerminal;
                    if std::io::stdin().is_terminal() {
                        Secret::prompt(label)
                    } else {
                        read_stdin()
                    }
                }
                None => Err(CliError::Usage(
                    "provide the secret via --stdin or --from-env <VAR>".into(),
                )),
            },
        }
    }
}

/// Read exactly one secret from stdin (all of it, trailing newline trimmed).
/// The scriptable ingress path: `op read … | <bin> auth set-credential --stdin`.
pub fn read_stdin() -> Result<Secret, CliError> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| CliError::Other(format!("reading secret from stdin: {e}")))?;
    // Trim a single trailing newline (and CR) so heredocs/echo pipes work.
    let trimmed = buf.strip_suffix('\n').unwrap_or(&buf);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    Ok(Secret::new(trimmed.to_string()))
}

/// Read one secret from a named environment variable (`--from-env APP_PASSWORD`).
/// Bounded-scope ingress for `op run --`-style invocations.
pub fn read_from_env(var: &str) -> Result<Secret, CliError> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(Secret::new(v)),
        Ok(_) => Err(CliError::Usage(format!("${var} is set but empty"))),
        Err(_) => Err(CliError::Usage(format!("${var} is not set"))),
    }
}

/// A secret string that refuses to reveal itself via `Debug`/`Display` and is
/// zeroized from memory when dropped. Read it only at the point of use, with
/// [`Secret::expose`], and never log the result.
pub struct Secret {
    inner: String,
}

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret {
            inner: value.into(),
        }
    }

    /// Borrow the underlying secret. Use at the call site only — never log it.
    pub fn expose(&self) -> &str {
        &self.inner
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// No-echo interactive prompt. Caller must have already confirmed a TTY.
    pub fn prompt(label: &str) -> Result<Secret, CliError> {
        let v = rpassword::prompt_password(format!("{label}: "))
            .map_err(|e| CliError::Other(format!("reading password: {e}")))?;
        Ok(Secret::new(v))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***redacted***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***redacted***")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

/// OS-keychain-backed credential store. The only runtime source of secrets.
/// `for_binary("fpl")` yields the family service name `piekstra.fpl`.
pub struct CredentialStore {
    service: String,
}

impl CredentialStore {
    pub fn new(service: impl Into<String>) -> Self {
        CredentialStore {
            service: service.into(),
        }
    }

    /// Family convention: service name `piekstra.<binary>` (SPEC v1 §1.7).
    pub fn for_binary(binary: &str) -> Self {
        CredentialStore::new(format!("piekstra.{binary}"))
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self, account: &str) -> Result<Entry, CliError> {
        Entry::new(&self.service, account)
            .map_err(|e| CliError::Keychain(format!("opening keychain entry: {e}")))
    }

    /// Keychain only. `None` if no entry exists.
    pub fn get(&self, account: &str) -> Result<Option<Secret>, CliError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(Secret::new(p))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CliError::Keychain(format!("reading credential: {e}"))),
        }
    }

    /// Store (or overwrite) a credential in the keychain.
    pub fn set(&self, account: &str, secret: &Secret) -> Result<(), CliError> {
        self.entry(account)?
            .set_password(secret.expose())
            .map_err(|e| CliError::Keychain(format!("storing credential: {e}")))
    }

    /// Delete a credential. Returns `true` if something was removed, `false`
    /// if there was nothing stored.
    pub fn delete(&self, account: &str) -> Result<bool, CliError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(CliError::Keychain(format!("deleting credential: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_redacts_but_exposes_on_demand() {
        let s = Secret::new("super-secret-token");
        assert_eq!(format!("{s}"), "***redacted***");
        assert_eq!(format!("{s:?}"), "Secret(***redacted***)");
        assert_eq!(s.expose(), "super-secret-token");
        assert!(!s.is_empty());
        assert!(Secret::new("").is_empty());
    }

    #[test]
    fn store_service_convention() {
        assert_eq!(CredentialStore::for_binary("fpl").service(), "piekstra.fpl");
    }

    #[test]
    fn env_ingestion() {
        std::env::set_var("PK_CLI_TEST_SECRET", "hunter2");
        assert_eq!(
            read_from_env("PK_CLI_TEST_SECRET").unwrap().expose(),
            "hunter2"
        );
        assert!(read_from_env("PK_CLI_TEST_UNSET_VAR").is_err());
    }
}
