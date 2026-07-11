//! Template family CLI (SPEC v1). Copy this crate to start a new one: it
//! wires every pk-cli-* crate into the standard surface — `auth`, `config`,
//! `self-update`, `completions`, `info` — with a stub domain command.

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use pk_cli_auth::{AuthStatus, LoginArgs, LogoutArgs, SetCredentialArgs};
use pk_cli_config::ConfigStore;
use pk_cli_core::info::{AuthInfo, CliInfo};
use pk_cli_core::{output, CliError, CommonArgs};
use pk_cli_secrets::CredentialStore;
use pk_cli_selfupdate::{SelfUpdateArgs, Updater};
use serde::{Deserialize, Serialize};

const BIN: &str = "example-cli";
const REPO: &str = "piekstra/cli-common";

/// Example member of the piekstra CLI family (conforms to piekstra-cli/1).
#[derive(Parser, Debug)]
#[command(name = BIN, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Credential management and session status.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Non-secret settings.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// A stub domain read, to show the output contract.
    Balance,
    /// Update to the latest release from GitHub.
    SelfUpdate(SelfUpdateArgs),
    /// Print a shell completion script.
    Completions { shell: Shell },
    /// Machine-readable capability discovery (cli-info/v1).
    Info,
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Store the demo credential in the OS keychain.
    Login(LoginArgs),
    /// Report credential/session state (auth-status/v1).
    Status,
    /// Clear the session; --forget also removes the stored credential.
    Logout(LogoutArgs),
    /// Raw keychain write for rotation / headless setup.
    SetCredential(SetCredentialArgs),
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Print the resolved config file path.
    Path,
    /// Show the effective configuration.
    Show,
    /// Set a config key (e.g. `config set account 123`).
    Set { key: String, value: String },
    /// Remove a config key.
    Unset { key: String },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        std::process::exit(output::fail(&e, cli.common.json));
    }
}

fn run(cli: &Cli) -> Result<(), CliError> {
    let store = ConfigStore::new(BIN);
    let creds = CredentialStore::for_binary(BIN);

    match &cli.command {
        Command::Auth(cmd) => auth(cli, cmd, &store, &creds),
        Command::Config(cmd) => config(cli, cmd, &store),
        Command::Balance => {
            let dto = serde_json::json!({
                "schema": "balance/v1",
                "balance": pk_cli_core::Money::usd("42.00"),
                "due_date": "2026-08-01",
            });
            if cli.common.json {
                output::json(&dto);
            } else {
                output::render(&dto);
            }
            Ok(())
        }
        Command::SelfUpdate(args) => Updater {
            repo: REPO,
            binary: BIN,
            target: env!("BUILD_TARGET"),
            current: env!("CARGO_PKG_VERSION"),
        }
        .run(args, cli.common.json, cli.common.quiet),
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut Cli::command(), BIN, &mut std::io::stdout());
            Ok(())
        }
        Command::Info => {
            let info = CliInfo::new(
                BIN,
                env!("CARGO_PKG_VERSION"),
                &format!("https://github.com/{REPO}"),
                AuthInfo {
                    required: true,
                    method: "password".into(),
                    login_hint: Some(format!("{BIN} auth login")),
                },
                &["balance"],
            );
            output::json(&serde_json::to_value(&info).unwrap());
            Ok(())
        }
    }
}

fn auth(
    cli: &Cli,
    cmd: &AuthCmd,
    store: &ConfigStore,
    creds: &CredentialStore,
) -> Result<(), CliError> {
    let cfg: Config = store.load()?;
    let user = cfg.username.clone().unwrap_or_else(|| "demo".into());
    match cmd {
        AuthCmd::Login(args) => {
            if creds.get(&user)?.is_some() && !args.overwrite {
                return Err(CliError::Usage(
                    "a credential is already stored; pass --overwrite to replace it".into(),
                ));
            }
            let prompt = if args.non_interactive { None } else { Some("Password") };
            let secret = args.source.read(prompt)?;
            creds.set(&user, &secret)?;
            eprintln!("credential stored in the OS keychain");
            Ok(())
        }
        AuthCmd::Status => {
            let mut status = AuthStatus::new(true, false, pk_cli_auth::AuthMethod::Password);
            status.username = Some(user.clone());
            status.account = cfg.account.clone();
            let stored = creds.get(&user)?.is_some();
            status.credential_in_keychain = Some(stored);
            status.authenticated = stored;
            status.emit(cli.common.json);
            Ok(())
        }
        AuthCmd::Logout(args) => {
            if args.forget {
                creds.delete(&user)?;
                store.clear()?;
            }
            eprintln!("logged out");
            Ok(())
        }
        AuthCmd::SetCredential(args) => {
            if creds.get(&user)?.is_some() && !args.overwrite {
                return Err(CliError::Usage(
                    "a credential is already stored; pass --overwrite to replace it".into(),
                ));
            }
            let secret = args.source.read(None)?;
            creds.set(&user, &secret)?;
            eprintln!("credential stored");
            Ok(())
        }
    }
}

fn config(cli: &Cli, cmd: &ConfigCmd, store: &ConfigStore) -> Result<(), CliError> {
    match cmd {
        ConfigCmd::Path => {
            println!("{}", store.path()?.display());
            Ok(())
        }
        ConfigCmd::Show => {
            let cfg: Config = store.load()?;
            let v = serde_json::to_value(&cfg).unwrap_or_default();
            if cli.common.json {
                output::json(&v);
            } else {
                output::render(&v);
            }
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let mut cfg: Config = store.load()?;
            match key.as_str() {
                "username" => cfg.username = Some(value.clone()),
                "account" => cfg.account = Some(value.clone()),
                other => {
                    return Err(CliError::Usage(format!(
                        "unknown config key `{other}` (known: username, account)"
                    )))
                }
            }
            store.save(&cfg)
        }
        ConfigCmd::Unset { key } => {
            let mut cfg: Config = store.load()?;
            match key.as_str() {
                "username" => cfg.username = None,
                "account" => cfg.account = None,
                other => {
                    return Err(CliError::Usage(format!(
                        "unknown config key `{other}` (known: username, account)"
                    )))
                }
            }
            store.save(&cfg)
        }
    }
}
