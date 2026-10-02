//! Template family CLI (SPEC v1). Copy this crate to start a new one: it
//! wires every pk-cli-* crate into the standard surface — `auth`, `config`,
//! `self-update`, `completions`, `info` — plus two domain profiles: utility/v1
//! (`summary`, `balance`, `bills list`) and documents/v1 (`documents list`),
//! to show the shared DTOs in use, and a plain `devices` noun showing the
//! generic list/get/mutate mechanisms (`emit_list`, `resolve::pick`, the
//! `confirm` gate). Credentials show both 1Password paths: `auth login --op`
//! (or a configured `op_ref`) at login, and a `SecretResolver` walking env,
//! keychain and 1Password in the configured `secret_sources` order when
//! `summary` needs the credential. The `state` noun keeps small files in the
//! owner's Drive (`pk-cli-drive`): a mounted folder (`data_root`) or an
//! rclone remote (`remote`) behind the stale-while-revalidate read cache,
//! with `state sync` as the background refresh's child.

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use pk_cli_auth::{AuthStatus, LoginArgs, LogoutArgs, SetCredentialArgs};
use pk_cli_config::ConfigStore;
use pk_cli_core::info::{AuthInfo, CliInfo};
use pk_cli_core::{confirm, output, resolve, CliError, CommonArgs};
use pk_cli_documents::Document;
use pk_cli_drive::{cache, Backend, CachedRemote, Kind, Remote};
use pk_cli_secrets::{
    CredentialStore, OnePassword, OpArgs, OpRef, SecretResolver, SecretSpec, SourceOrder,
};
use pk_cli_selfupdate::{SelfUpdateArgs, Updater};
use pk_cli_utility::{Paged, RangeArgs, Statement, UtilitySummary};
use serde::{Deserialize, Serialize};

const BIN: &str = "example-cli";
const REPO: &str = "piekstra/cli-common";
const CONFIG_KEYS: &str = "username, account, op_ref, secret_sources, data_root, remote";

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
    /// Account overview: amount due and due date (utility-summary/v1).
    Summary,
    /// Same DTO as `summary` — the profile's second entry point.
    Balance,
    /// Bills/statements (utility/v1 profile).
    #[command(subcommand)]
    Bills(BillsCmd),
    /// Published documents (documents/v1 profile).
    #[command(subcommand)]
    Documents(DocumentsCmd),
    /// Devices — a plain (non-profile) noun: list, resolve a reference, mutate.
    #[command(subcommand)]
    Devices(DevicesCmd),
    /// Small state files in the owner's Drive (mount or rclone remote).
    State(StateArgs),
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
    Login(LoginCmd),
    /// Report credential/session state (auth-status/v1).
    Status,
    /// Clear the session; --forget also removes the stored credential.
    Logout(LogoutArgs),
    /// Raw keychain write for rotation / headless setup.
    SetCredential(SetCredentialArgs),
}

/// The standard login flags plus `--op`. `OpArgs` is flattened beside
/// `LoginArgs` rather than inside it, so CLIs that build `LoginArgs` by hand
/// keep compiling.
#[derive(clap::Args, Debug)]
struct LoginCmd {
    #[command(flatten)]
    login: LoginArgs,
    #[command(flatten)]
    op: OpArgs,
}

#[derive(Subcommand, Debug)]
enum BillsCmd {
    /// List statements, newest first (statement-list/v1).
    #[command(visible_alias = "ls")]
    List(RangeArgs),
}

#[derive(Subcommand, Debug)]
enum DocumentsCmd {
    /// List published documents, newest first (document-list/v1).
    #[command(visible_alias = "ls")]
    List(RangeArgs),
    // A real CLI adds `download <ID> -o <PATH>` (document-download/v1) here;
    // the demo has no files to stream, so it shows the list shape only.
}

#[derive(Subcommand, Debug)]
enum DevicesCmd {
    /// List devices (device-list/v1 — a plain list, no paging).
    #[command(visible_alias = "ls")]
    List,
    /// Show one device by name, id, or a unique part of its name (device/v1).
    Get { device: String },
    /// Rename a device. Asks first unless --force; exit 6 when it cannot ask.
    Rename {
        device: String,
        name: String,
        /// Skip the confirmation prompt (required when non-interactive).
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::Args, Debug)]
struct StateArgs {
    /// Read straight from the remote, bypassing the read cache (writes
    /// still update it). Same as `EXAMPLE_CLI_NO_CACHE=1`.
    #[arg(long, global = true)]
    no_cache: bool,
    #[command(subcommand)]
    cmd: StateCmd,
}

#[derive(Subcommand, Debug)]
enum StateCmd {
    /// Print a state file (state-file/v1). Exit 4 when it does not exist.
    Get { rel: String },
    /// Write a state file from stdin.
    Put { rel: String },
    /// List the files under a folder, recursively (state-entry-list/v1).
    #[command(visible_alias = "ls")]
    List {
        #[arg(default_value = "")]
        rel: String,
    },
    /// Wipe the read cache (`--clear`); hidden: the background refresh.
    Sync(StateSyncArgs),
}

/// `state sync`. The revalidate arguments are what
/// `pk_cli_drive::cache::SpawnRevalidator` passes; they are hidden because
/// only the cache runs them.
#[derive(clap::Args, Debug)]
struct StateSyncArgs {
    /// Wipe the read cache for the configured remote.
    #[arg(long)]
    clear: bool,
    #[arg(long, hide = true, value_name = "REL", conflicts_with_all = ["clear", "revalidate_listing"])]
    revalidate_file: Option<String>,
    #[arg(long, hide = true, value_name = "REL", conflicts_with = "clear")]
    revalidate_listing: Option<String>,
    #[arg(long, hide = true, value_name = "SPEC")]
    remote_spec: Option<String>,
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

/// A demo record for the `devices` noun. A real CLI reads these from its
/// provider; the mechanisms below do not care where they came from.
#[derive(Debug, Clone, Serialize)]
struct Device {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    room: Option<String>,
}

fn demo_devices() -> Vec<Device> {
    let dev = |id: &str, name: &str, room: Option<&str>| Device {
        id: id.into(),
        name: name.into(),
        room: room.map(str::to_string),
    };
    vec![
        dev("H6076_AA11BB22", "Office Lamp", Some("Office")),
        dev("KP115_CC33DD44", "Desk Plug", Some("Office")),
        dev("H6159_EE55FF66", "Kitchen Strip", None),
    ]
}

/// `<REF>` may be a name, an id (any case), or a unique part of a name;
/// a tie is exit 4 naming the candidates, never a silent first pick.
fn find_device<'a>(devices: &'a [Device], q: &str) -> Result<&'a Device, CliError> {
    resolve::pick(devices, q, |d| vec![d.id.clone()], |d| &d.name, "device")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    /// Where 1Password keeps the password (`op://vault/item/field`).
    #[serde(skip_serializing_if = "Option::is_none")]
    op_ref: Option<OpRef>,
    /// Source order for runtime reads, e.g. `op,keychain` (default
    /// `env,keychain,op`).
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_sources: Option<SourceOrder>,
    /// A local folder for `state` — a Drive-for-desktop mount, or any dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    data_root: Option<String>,
    /// An rclone spec for `state` (`<remote>:<folder>`); preferred over
    /// `data_root` when both are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
}

/// The store `state` works on: the rclone remote behind the read cache when
/// one is configured, else the mount.
fn state_backend(cfg: &Config) -> Result<Backend, CliError> {
    if let Some(spec) = &cfg.remote {
        let remote = Remote::new(spec).temp_prefix(BIN);
        let Some(dir) = cache::cache_dir_for(BIN, spec) else {
            return Ok(Backend::Remote(remote));
        };
        let policy = cache::CachePolicy::from_env(BIN);
        let mut cached = CachedRemote::new(remote, dir, policy);
        if !policy.bypass_reads {
            if let Some(r) = cache::background_revalidator(BIN, &["state", "sync"], spec) {
                cached = cached.with_revalidator(r);
            }
        }
        return Ok(Backend::Cached(cached));
    }
    match &cfg.data_root {
        Some(root) => Ok(Backend::Mount(root.into())),
        None => Err(CliError::Usage(format!(
            "no state store configured — `{BIN} config set data_root <dir>` or \
             `{BIN} config set remote <remote>:<folder>`"
        ))),
    }
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
        Command::Summary | Command::Balance => {
            // A real CLI resolves its credential before the provider call.
            let cfg: Config = store.load()?;
            let user = cfg.username.clone().unwrap_or_else(|| "demo".into());
            let spec = SecretSpec::new(&user)
                .env("EXAMPLE_CLI_PASSWORD")
                .op(cfg.op_ref.clone());
            let resolved = SecretResolver::new(&creds)
                .order(cfg.secret_sources.clone().unwrap_or_default())
                .resolve(&spec)?
                .ok_or_else(|| {
                    CliError::Auth(format!("no credential stored; run `{BIN} auth login`"))
                })?;
            if cli.common.verbose {
                eprintln!("credential from {}", resolved.source);
            }
            let mut dto = UtilitySummary::new(pk_cli_core::Money::usd("42.00"));
            dto.due_date = Some("2026-08-01".into());
            pk_cli_utility::emit(&dto, cli.common.json);
            Ok(())
        }
        Command::Bills(BillsCmd::List(range)) => {
            range.validate()?;
            let statements = vec![
                Statement {
                    id: "2026-07".into(),
                    date: Some("2026-07-15".into()),
                    amount: pk_cli_core::Money::usd("42.00"),
                    due_date: Some("2026-08-01".into()),
                    paid: Some(false),
                },
                Statement {
                    id: "2026-06".into(),
                    date: Some("2026-06-15".into()),
                    amount: pk_cli_core::Money::usd("39.75"),
                    due_date: Some("2026-07-01".into()),
                    paid: Some(true),
                },
            ];
            let n = range.limit.unwrap_or(u32::MAX) as usize;
            Paged::new("statement", statements.into_iter().take(n).collect()).emit(cli.common.json);
            Ok(())
        }
        Command::Documents(DocumentsCmd::List(range)) => {
            range.validate()?;
            let mut stmt = Document::new("2026-07", "July 2026 Statement");
            stmt.date = Some("2026-07-15".into());
            stmt.category = Some("statement".into());
            stmt.file = Some("2026-07-statement.pdf".into());
            let mut tax = Document::new("2025-1098", "2025 Form 1098");
            tax.date = Some("2026-01-31".into());
            tax.category = Some("tax".into());
            tax.file = Some("2025-1098.pdf".into());
            let docs = vec![stmt, tax];
            let n = range.limit.unwrap_or(u32::MAX) as usize;
            Paged::new("document", docs.into_iter().take(n).collect()).emit(cli.common.json);
            Ok(())
        }
        Command::Devices(cmd) => devices(cli, cmd),
        Command::State(args) => state(cli, args, &store),
        Command::SelfUpdate(args) => Updater {
            repo: REPO.into(),
            binary: BIN.into(),
            target: env!("BUILD_TARGET").into(),
            current: env!("CARGO_PKG_VERSION").into(),
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
                &[
                    "summary",
                    "balance",
                    "bills",
                    "documents",
                    "devices",
                    "state",
                ],
            )
            .with_profiles(&[pk_cli_utility::PROFILE, pk_cli_documents::PROFILE]);
            output::json(&serde_json::to_value(&info).expect("CliInfo serializes to JSON"));
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
        AuthCmd::Login(LoginCmd { login: args, op }) => {
            if creds.get(&user)?.is_some() && !args.overwrite {
                return Err(CliError::Usage(
                    "a credential is already stored; pass --overwrite to replace it".into(),
                ));
            }
            let prompt = if args.non_interactive {
                None
            } else {
                Some("Password")
            };
            // No explicit source: a configured 1Password reference comes
            // before the prompt.
            let explicit = args.source.stdin || args.source.from_env.is_some();
            let op = match (&op.op, &cfg.op_ref) {
                (None, Some(r)) if !explicit => OpArgs::reference(r.clone()),
                _ => op.clone(),
            };
            let secret = args.source.read_with_op(&op, &OnePassword::new(), prompt)?;
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

fn devices(cli: &Cli, cmd: &DevicesCmd) -> Result<(), CliError> {
    let json = cli.common.json;
    let value = |d: &Device| {
        serde_json::to_value(d).map_err(|e| CliError::Other(format!("serializing device: {e}")))
    };
    match cmd {
        DevicesCmd::List => {
            let rows = demo_devices()
                .iter()
                .map(value)
                .collect::<Result<Vec<_>, _>>()?;
            output::emit_list(json, "device", rows, &["id", "name", "room"]);
            Ok(())
        }
        DevicesCmd::Get { device } => {
            let all = demo_devices();
            output::emit_one(json, "device", value(find_device(&all, device)?)?);
            Ok(())
        }
        DevicesCmd::Rename {
            device,
            name,
            force,
        } => {
            // Gate first: a driver that forgot --force gets exit 6 before any
            // keychain or network work (SPEC §1.3).
            confirm::require_confirmable(*force, cli.common.interactive(), "renaming a device")?;
            let all = demo_devices(); // a real CLI: session, then the provider read
            let d = find_device(&all, device)?;
            confirm::confirm(*force, &format!("Rename \"{}\" to \"{name}\"?", d.name))?;
            // A real CLI sends the write here, then reads the device back and
            // emits what it read — never the write's status code.
            let mut renamed = d.clone();
            renamed.name = name.clone();
            output::emit_one(json, "device", value(&renamed)?);
            Ok(())
        }
    }
}

fn state(cli: &Cli, args: &StateArgs, store: &ConfigStore) -> Result<(), CliError> {
    let json = cli.common.json;
    if args.no_cache {
        // Exported so a child this run spawns inherits the choice.
        std::env::set_var(format!("{}_NO_CACHE", cache::env_prefix(BIN)), "1");
    }
    let cfg: Config = store.load()?;
    if let StateCmd::Sync(sync) = &args.cmd {
        return state_sync(json, &cfg, sync);
    }
    let backend = state_backend(&cfg)?;
    match &args.cmd {
        StateCmd::Get { rel } => {
            let content = backend.read_file(rel)?.ok_or_else(|| {
                CliError::NotFound(format!("{}: no such file", backend.display_path(rel)))
            })?;
            let payload = serde_json::json!({
                "rel": rel,
                "location": backend.display_path(rel),
                "content": content,
            });
            output::emit(json, "state-file", payload, |p| {
                print!("{}", p["content"].as_str().unwrap_or_default());
            });
            Ok(())
        }
        StateCmd::Put { rel } => {
            let mut content = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut content)
                .map_err(|e| CliError::Other(format!("reading stdin: {e}")))?;
            backend.write_file(rel, &content)?;
            // Read back, never trust the write's status (SPEC §1.3).
            let stored = backend.read_file(rel)?.unwrap_or_default();
            let payload = serde_json::json!({
                "rel": rel,
                "location": backend.display_path(rel),
                "bytes": stored.len(),
            });
            output::emit(json, "state-write", payload, |p| {
                eprintln!(
                    "wrote {} bytes to {}",
                    p["bytes"],
                    p["location"].as_str().unwrap_or("")
                );
            });
            Ok(())
        }
        StateCmd::List { rel } => {
            let rows = backend
                .list_entries(rel)?
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or_default())
                .collect();
            output::emit_list(json, "state-entry", rows, &["rel", "size", "modified"]);
            Ok(())
        }
        StateCmd::Sync(_) => unreachable!("handled above"),
    }
}

/// `state sync`: the background refresh a stale cached read spawns, or
/// `--clear`.
fn state_sync(json: bool, cfg: &Config, args: &StateSyncArgs) -> Result<(), CliError> {
    let spec = args
        .remote_spec
        .as_ref()
        .or(cfg.remote.as_ref())
        .ok_or_else(|| {
            CliError::Usage(
                "`state sync` manages the read cache of a remote, and none is configured".into(),
            )
        })?;
    let target = match (&args.revalidate_file, &args.revalidate_listing) {
        (Some(rel), _) => Some((Kind::File, rel)),
        (None, Some(rel)) => Some((Kind::Listing, rel)),
        (None, None) => None,
    };
    if let Some((kind, rel)) = target {
        let (dir, outcome) = cache::run_revalidation(BIN, spec, kind, rel)?;
        let payload = serde_json::json!({
            "cache_dir": dir.display().to_string(),
            "revalidated": { "kind": kind.as_str(), "rel": rel, "outcome": outcome.as_str() },
        });
        output::emit(json, "state-sync", payload, |_| {
            eprintln!("{} {rel}: {}", kind.as_str(), outcome.as_str());
        });
        return Ok(());
    }
    if !args.clear {
        return Err(CliError::Usage(
            "nothing to do — pass --clear to wipe the read cache".into(),
        ));
    }
    let dir = cache::cache_dir_for(BIN, spec).ok_or_else(|| {
        CliError::Other("cannot resolve a cache directory (is $HOME set?)".into())
    })?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| CliError::Other(format!("clearing cache {}: {e}", dir.display())))?;
    }
    let payload = serde_json::json!({ "cache_dir": dir.display().to_string(), "cleared": true });
    output::emit(json, "state-sync", payload, |_| {
        eprintln!("cache cleared: {}", dir.display());
    });
    Ok(())
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
                "op_ref" => cfg.op_ref = Some(value.parse().map_err(CliError::Usage)?),
                "secret_sources" => {
                    cfg.secret_sources = Some(value.parse().map_err(CliError::Usage)?)
                }
                "data_root" => cfg.data_root = Some(value.clone()),
                "remote" => cfg.remote = Some(value.clone()),
                other => {
                    return Err(CliError::Usage(format!(
                        "unknown config key `{other}` (known: {CONFIG_KEYS})"
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
                "op_ref" => cfg.op_ref = None,
                "secret_sources" => cfg.secret_sources = None,
                "data_root" => cfg.data_root = None,
                "remote" => cfg.remote = None,
                other => {
                    return Err(CliError::Usage(format!(
                        "unknown config key `{other}` (known: {CONFIG_KEYS})"
                    )))
                }
            }
            store.save(&cfg)
        }
    }
}
