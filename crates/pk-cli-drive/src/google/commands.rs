//! The Drive commands a CLI mounts ([`DriveCommand`]) and the context they
//! run in ([`DriveCli`]): profiles, sign-in, status, logout, `about`.
//!
//! A CLI with no `--profile` of its own mounts [`DriveArgs`]
//! (`<bin> drive --profile <NAME> login`). One whose global `--profile`
//! means something else (medrec's records folders) mounts [`DriveCommand`]
//! and passes the Drive profile it resolved to [`DriveCli::run`].

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use pk_cli_auth::oauth::{self, AuthorizationCodeFlow, OAuthClient, TokenSet};
use pk_cli_auth::refresh_lock::RefreshLock;
use pk_cli_auth::vault::Vault;
use pk_cli_auth::{AuthLogin, AuthMethod, AuthStatus};
use pk_cli_config::profile::{self, ProfileName};
use pk_cli_core::{block_on, confirm, output, CliError, Reason};
use pk_cli_secrets::CredentialStore;
use serde_json::{json, Value};

use super::account::{now, Account, FixedToken};
use super::api::{Api, Retry};
use super::client::{id_hint, DesktopClient};
use super::drive::{Drive, DriveSpec};
use super::endpoints::Endpoints;
use super::profiles::{client_entry, token_entry, DriveProfile, ProfileStore};

/// A Drive scope a profile asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    /// `drive.file`: only the files a client of this Cloud project created
    /// or the person opened with one; every client of the project sees
    /// them. Not a restricted scope.
    File,
    /// `drive.readonly`: read every file (restricted).
    Readonly,
    /// `drive`: read and write every file (restricted).
    Full,
}

impl Scope {
    pub fn url(self) -> &'static str {
        match self {
            Scope::File => "https://www.googleapis.com/auth/drive.file",
            Scope::Readonly => "https://www.googleapis.com/auth/drive.readonly",
            Scope::Full => "https://www.googleapis.com/auth/drive",
        }
    }

    /// `drive.file` for its URL; any other URL as given.
    pub fn short(url: &str) -> &str {
        url.strip_prefix("https://www.googleapis.com/auth/")
            .unwrap_or(url)
    }
}

/// The Drive commands with their own `--profile`, for a CLI that has none.
#[derive(Args, Debug, Clone)]
pub struct DriveArgs {
    /// The Drive profile: one Google account (env `<PREFIX>_DRIVE_PROFILE`).
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,
    #[command(subcommand)]
    pub command: DriveCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DriveCommand {
    /// Sign a Drive profile in to one Google account, in the browser.
    Login(LoginArgs),
    /// Whether a Drive profile is signed in (auth-status/v1).
    Status(StatusArgs),
    /// Remove a Drive profile's sign-in from this machine.
    Logout(LogoutArgs),
    /// The profile's Google account and its storage (drive-about/v1).
    About,
    /// The OAuth client a Drive profile signs in with.
    #[command(subcommand)]
    Client(ClientCommand),
    /// The Drive profiles on this machine.
    #[command(subcommand)]
    Profiles(ProfilesCommand),
    /// Folders at the top of My Drive, for a store.
    #[command(subcommand)]
    Folder(FolderCommand),
}

#[derive(Subcommand, Debug, Clone)]
pub enum FolderCommand {
    /// Make a folder at the top of My Drive, and print its id and the store
    /// spec naming it. Under `drive.file` a store must be a folder this
    /// app's client made. One of that name already there is reused.
    Create {
        /// The folder's name.
        name: String,
    },
    /// The folders at the top of My Drive this sign-in can see (under
    /// `drive.file`, the ones this app's client made).
    List,
}

#[derive(Args, Debug, Clone, Default)]
pub struct LoginArgs {
    /// Import this Desktop app client JSON (from the Google Cloud console)
    /// first.
    #[arg(long, value_name = "PATH", conflicts_with = "client_stdin")]
    pub client_file: Option<PathBuf>,
    /// Import the Desktop app client JSON from stdin first.
    #[arg(long)]
    pub client_stdin: bool,
    /// A Drive scope to ask for; repeat for more. Default: the CLI's.
    #[arg(long = "scope", value_enum, value_name = "SCOPE")]
    pub scopes: Vec<Scope>,
    /// The Google account to sign in as; a sign-in to another is refused.
    /// Default: the profile's account, for a profile signed in before.
    #[arg(long, value_name = "EMAIL")]
    pub account: Option<String>,
    /// Print the sign-in address instead of opening the browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Minutes to wait for the browser.
    #[arg(long, value_name = "MINUTES", default_value_t = 5)]
    pub wait: u64,
}

#[derive(Args, Debug, Clone, Default)]
pub struct StatusArgs {
    /// Report only what is stored: no keychain read, no request to Google.
    #[arg(long)]
    pub local: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct LogoutArgs {
    /// Also revoke the grant at Google. It ends every sign-in of this
    /// account through any OAuth client of this Cloud project (another
    /// CLI's, a web app's), on every machine.
    #[arg(long)]
    pub revoke: bool,
    /// Also remove the profile and its OAuth client.
    #[arg(long)]
    pub forget: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ClientCommand {
    /// Store a Desktop app client's JSON for the profile (in the vault).
    Import {
        /// The JSON file the Cloud console's "Download JSON" gave.
        #[arg(
            long,
            alias = "client-file",
            value_name = "PATH",
            required_unless_present = "stdin",
            conflicts_with = "stdin"
        )]
        file: Option<PathBuf>,
        /// Read the JSON from stdin.
        #[arg(long, alias = "client-stdin")]
        stdin: bool,
        /// Replace a different client already stored; its sign-in is removed.
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProfilesCommand {
    /// The Drive profiles (profile-list/v1). Reads no keychain.
    List,
    /// Remove a Drive profile: its sign-in, its OAuth client and its record.
    /// Named as `profiles add` does, or by the commands' profile flag.
    Remove {
        name: Option<String>,
        /// Do not ask.
        #[arg(long)]
        force: bool,
    },
}

/// What the Drive commands run with: the CLI's name, its vault, where the
/// profiles are recorded, and Google's endpoints.
pub struct DriveCli {
    bin: String,
    version: String,
    command: String,
    profile_flag: String,
    store: ProfileStore,
    vault: Arc<Vault>,
    lock_path: PathBuf,
    endpoints: Endpoints,
    default_scopes: Vec<Scope>,
    browser: Box<dyn Fn(&str) -> bool>,
    retry: Retry,
}

impl DriveCli {
    /// The standard context for `bin`: its vault (the keychain service
    /// `piekstra.<bin>`, or `<PREFIX>_KEYCHAIN_SERVICE` in a test), its
    /// `drive.json`, its refresh lock, and Google's endpoints (or
    /// `<PREFIX>_DRIVE_TEST_ENDPOINT`'s fake server). `command` is how the
    /// CLI mounts these commands (`medrec drive`), for remedies.
    pub fn new(bin: &str, version: &str, command: &str) -> Result<Self, CliError> {
        let service_var = format!("{}_KEYCHAIN_SERVICE", crate::cache::env_prefix(bin));
        let keys = match std::env::var(&service_var) {
            Ok(s) if !s.is_empty() => CredentialStore::new(s),
            _ => CredentialStore::for_binary(bin),
        };
        let vault = Vault::new(keys, pk_cli_auth::vault::default_dir(bin)?);
        Ok(DriveCli {
            bin: bin.into(),
            version: version.into(),
            command: command.into(),
            profile_flag: "--profile".into(),
            store: ProfileStore::for_binary(bin)?,
            vault: Arc::new(vault),
            lock_path: RefreshLock::for_binary(bin)?.path().to_path_buf(),
            endpoints: Endpoints::from_env(bin)?,
            default_scopes: Vec::new(),
            browser: Box::new(oauth::open_browser),
            retry: Retry::DEFAULT,
        })
    }

    /// The flag that names a Drive profile on the mounted commands, for
    /// remedies (default `--profile`). A CLI whose global `--profile` means
    /// something else gives its own (`--drive-profile`).
    pub fn profile_flag(mut self, flag: impl Into<String>) -> Self {
        self.profile_flag = flag.into();
        self
    }

    /// Use `vault` instead (a test's, with its own keys and directory).
    pub fn with_vault(mut self, vault: Vault) -> Self {
        self.vault = Arc::new(vault);
        self
    }

    pub fn with_profiles(mut self, store: ProfileStore) -> Self {
        self.store = store;
        self
    }

    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    pub fn with_lock(mut self, path: impl Into<PathBuf>) -> Self {
        self.lock_path = path.into();
        self
    }

    /// The scopes `login` asks for when given none.
    pub fn default_scopes(mut self, scopes: &[Scope]) -> Self {
        self.default_scopes = scopes.to_vec();
        self
    }

    /// Open the sign-in address with `open` instead of the system browser.
    pub fn with_browser(mut self, open: impl Fn(&str) -> bool + 'static) -> Self {
        self.browser = Box::new(open);
        self
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    pub fn profiles(&self) -> &ProfileStore {
        &self.store
    }

    /// The environment's fallback for the Drive profile:
    /// `<PREFIX>_DRIVE_PROFILE`.
    pub fn profile_env(&self) -> String {
        format!("{}_DRIVE_PROFILE", crate::cache::env_prefix(&self.bin))
    }

    fn login_command(&self, name: &ProfileName) -> String {
        format!("{} login {} {name}", self.command, self.profile_flag)
    }

    /// A configured Drive profile: `given`, else the environment's. Missing
    /// or unknown is exit 2 naming the configured ones and the command that
    /// adds one.
    pub fn profile(&self, given: Option<&str>) -> Result<ProfileName, CliError> {
        let configured = self.store.load()?;
        profile::resolve(given, &self.profile_env(), &configured, &self.bin).map_err(|e| {
            let hint = match given.map(ProfileName::parse) {
                Some(Ok(name)) => self.login_command(&name),
                _ => format!("{} login {} <NAME>", self.command, self.profile_flag),
            };
            CliError::Usage(e.message().to_string())
                .with_remedy(format!("sign a Drive profile in with `{hint}`"))
        })
    }

    /// The token source for a profile.
    pub fn account(&self, profile: &ProfileName) -> Account {
        Account {
            bin: self.bin.clone(),
            version: self.version.clone(),
            command: self.command.clone(),
            profile_flag: self.profile_flag.clone(),
            profile: profile.clone(),
            vault: self.vault.clone(),
            lock: RefreshLock::at(self.lock_path.clone()),
            endpoints: self.endpoints.clone(),
        }
    }

    fn api(&self, profile: &ProfileName) -> Result<Api, CliError> {
        let mut api = Api::new(
            &self.bin,
            &self.version,
            self.endpoints.clone(),
            Box::new(self.account(profile)),
        )?;
        api.retry = self.retry;
        Ok(api)
    }

    /// The store at `spec`. Its profile must be configured: one that is not
    /// is exit 2 naming the sign-in that adds it. Nothing is read yet.
    pub fn drive(&self, spec: &DriveSpec) -> Result<Drive, CliError> {
        if self.store.get(spec.profile())?.is_none() {
            return Err(CliError::Usage(format!(
                "Drive profile `{}` is not configured on this machine",
                spec.profile()
            ))
            .because(Reason::NoCredential)
            .with_remedy(format!(
                "sign it in with `{}`",
                self.login_command(spec.profile())
            )));
        }
        Ok(Drive::new(self.api(spec.profile())?, spec.clone()).temp_prefix(self.bin.clone()))
    }

    /// Run one Drive command as `profile` (the CLI's `--profile` for these
    /// commands, or the one it resolved). `interactive` is
    /// `CommonArgs::interactive`, for the confirmation a removal asks.
    pub fn run(
        &self,
        command: &DriveCommand,
        profile: Option<&str>,
        json_mode: bool,
        interactive: bool,
    ) -> Result<(), CliError> {
        match command {
            DriveCommand::Login(args) => self.login(profile, args, json_mode),
            DriveCommand::Status(args) => self.status(profile, args, json_mode),
            DriveCommand::Logout(args) => self.logout(profile, args, json_mode),
            DriveCommand::About => self.about(profile, json_mode),
            DriveCommand::Client(ClientCommand::Import {
                file,
                stdin,
                overwrite,
            }) => self.import_client(profile, file.as_deref(), *stdin, *overwrite, json_mode),
            DriveCommand::Profiles(ProfilesCommand::List) => self.list(json_mode),
            DriveCommand::Profiles(ProfilesCommand::Remove { name, force }) => {
                let name = match (name.as_deref(), profile) {
                    (Some(a), Some(b)) if a != b => {
                        return Err(CliError::Usage(format!(
                            "two profiles named: `{a}` and {} `{b}`; name one",
                            self.profile_flag
                        )))
                    }
                    (Some(a), _) | (None, Some(a)) => a,
                    (None, None) => {
                        return Err(CliError::Usage(format!(
                            "name the profile to remove: `{} profiles remove <NAME>`",
                            self.command
                        )))
                    }
                };
                self.remove(name, *force, interactive, json_mode)
            }
            DriveCommand::Folder(FolderCommand::Create { name }) => {
                self.folder_create(profile, name, json_mode)
            }
            DriveCommand::Folder(FolderCommand::List) => self.folder_list(profile, json_mode),
        }
    }

    /// A profile name for a command that may create the profile.
    fn new_name(&self, given: Option<&str>) -> Result<ProfileName, CliError> {
        let from_env = std::env::var(self.profile_env())
            .ok()
            .filter(|v| !v.is_empty());
        match given.or(from_env.as_deref()) {
            Some(name) => ProfileName::parse(name),
            None => self.profile(None),
        }
    }

    fn read_client(
        &self,
        file: Option<&std::path::Path>,
        stdin: bool,
    ) -> Result<Option<DesktopClient>, CliError> {
        if let Some(path) = file {
            let text = std::fs::read_to_string(path)
                .map_err(|e| CliError::Usage(format!("reading {}: {e}", path.display())))?;
            return DesktopClient::from_json(&text).map(Some);
        }
        if stdin {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| CliError::Usage(format!("reading the client JSON from stdin: {e}")))?;
            let client = DesktopClient::from_json(&text);
            zeroize::Zeroize::zeroize(&mut text);
            return client.map(Some);
        }
        Ok(None)
    }

    fn login(
        &self,
        given: Option<&str>,
        args: &LoginArgs,
        json_mode: bool,
    ) -> Result<(), CliError> {
        let name = self.new_name(given)?;
        let scopes: Vec<Scope> = if args.scopes.is_empty() {
            self.default_scopes.clone()
        } else {
            args.scopes.clone()
        };
        if scopes.is_empty() {
            return Err(CliError::Usage(
                "name the Drive scope to ask for: `--scope file`, `--scope readonly` or `--scope full`"
                    .into(),
            ));
        }
        let record = self.store.get(&name)?.unwrap_or_default();
        let expected = args.account.clone().or_else(|| record.account.clone());
        let given_client = self.read_client(args.client_file.as_deref(), args.client_stdin)?;
        let account = self.account(&name);
        let client = match given_client {
            Some(c) => c,
            None => account.client()?,
        };
        let lock = RefreshLock::at(self.lock_path.clone());
        lock.before_signin()?;
        let scope_text = scopes.iter().map(|s| s.url()).collect::<Vec<_>>().join(" ");
        let mut flow =
            AuthorizationCodeFlow::new(&self.bin, &self.endpoints.authorize, client.client_id())
                .scope(scope_text)
                .param("access_type", "offline")
                .param("prompt", "consent");
        if let Some(email) = &expected {
            flow = flow.param("login_hint", email);
        }
        let pending = flow.start()?;
        eprintln!(
            "Sign in to Google for Drive profile `{name}`{}. If no browser opens, go to:\n{}",
            expected
                .as_deref()
                .map(|e| format!(" as {e}"))
                .unwrap_or_default(),
            pending.url()
        );
        if !args.no_browser {
            (self.browser)(pending.url());
        }
        let code = pending
            .wait_for_code_with(Duration::from_secs(args.wait.max(1) * 60), |why| {
                eprintln!("{why}. Still waiting; press Ctrl-C to start again.")
            })?;
        let oauth = OAuthClient::new(
            pk_cli_http::BlockingTransport::new()?,
            self.bin.clone(),
            self.version.clone(),
        );
        let answer = block_on(oauth.token(
            &self.endpoints.token,
            &client.auth(),
            &pending.token_request(&code),
        ))?;
        let tokens = TokenSet::from_response(answer, now(), None);
        if tokens.refresh_token.is_none() {
            return Err(CliError::Auth(
                "Google sent no refresh token, so the sign-in could not be kept; nothing was stored"
                    .into(),
            )
            .with_remedy(format!(
                "remove this app's access at https://myaccount.google.com/permissions, then `{}`",
                self.login_command(&name)
            )));
        }
        let granted: Vec<String> = tokens
            .scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let missing: Vec<&str> = scopes
            .iter()
            .map(|s| s.url())
            .filter(|url| !granted.iter().any(|g| g == url))
            .collect();
        if !missing.is_empty() {
            return Err(CliError::Auth(format!(
                "Google granted {} but not {}; nothing was stored",
                if granted.is_empty() {
                    "nothing".to_string()
                } else {
                    granted
                        .iter()
                        .map(|g| Scope::short(g))
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                missing
                    .iter()
                    .map(|m| Scope::short(m))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .because(Reason::Forbidden)
            .with_remedy(format!(
                "sign in again with `{}` and tick every permission the consent screen asks",
                self.login_command(&name)
            )));
        }
        let mut probe = Api::new(
            &self.bin,
            &self.version,
            self.endpoints.clone(),
            Box::new(FixedToken(tokens.access_token.clone())),
        )?;
        probe.retry = self.retry;
        let about = probe.about()?;
        if let Some(want) = &expected {
            if !want.eq_ignore_ascii_case(&about.email) {
                return Err(CliError::Auth(format!(
                    "signed in as {}, but Drive profile `{name}` is {want}'s; nothing was stored",
                    about.email
                ))
                .because(Reason::CredentialRefused)
                .with_remedy(format!(
                    "sign in as {want}, or use another profile name for {}",
                    about.email
                )));
            }
        }
        let new_record = DriveProfile {
            account: Some(about.email.clone()),
            scopes: granted.clone(),
            client_id: Some(client.client_id().to_string()),
            signed_in_at: Some(pk_cli_core::dates::fmt_rfc3339(now() as i64)),
        };
        lock.store_signin(|| {
            self.vault
                .put_json(&client_entry(&name), &client.to_stored())?;
            self.vault.put_json(&token_entry(&name), &tokens)?;
            self.store.put(&name, &new_record)
        })?;
        let mut payload = AuthLogin::stored(true).to_json();
        if let Value::Object(map) = &mut payload {
            map.insert("profile".into(), json!(name.as_str()));
            map.insert("username".into(), json!(about.email));
            map.insert(
                "scopes".into(),
                json!(granted.iter().map(|g| Scope::short(g)).collect::<Vec<_>>()),
            );
        }
        if json_mode {
            output::json(&payload);
        } else {
            println!(
                "Signed in Drive profile `{name}` as {} ({}).",
                about.email,
                granted
                    .iter()
                    .map(|g| Scope::short(g))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(path) = &args.client_file {
            eprintln!(
                "note: the OAuth client is in the vault now; {} can be deleted",
                path.display()
            );
        }
        Ok(())
    }

    fn status(
        &self,
        given: Option<&str>,
        args: &StatusArgs,
        json_mode: bool,
    ) -> Result<(), CliError> {
        let name = self.profile(given)?;
        let record = self.store.get(&name)?.unwrap_or_default();
        let present = self.vault.contains(&token_entry(&name))?;
        let mut status = AuthStatus::new(true, present, AuthMethod::BrowserSession);
        status.username = record.account.clone();
        status.credential_in_keychain = Some(present);
        let mut problem = None;
        if present && !args.local {
            match self.api(&name)?.about() {
                Ok(about)
                    if record
                        .account
                        .as_deref()
                        .is_none_or(|a| a.eq_ignore_ascii_case(&about.email)) =>
                {
                    status.session_valid = Some(true);
                }
                Ok(_) => {
                    status.session_valid = Some(false);
                    status.authenticated = false;
                    // The profile's address is the status's username; the
                    // other account's is not shown.
                    problem = Some(
                        "the stored sign-in is another Google account's, not this profile's"
                            .to_string(),
                    );
                }
                Err(e) if e.is_auth() => {
                    status.session_valid = Some(false);
                    status.authenticated = false;
                    problem = Some(e.message().to_string());
                }
                Err(e) => return Err(e),
            }
        }
        let scopes: Vec<&str> = record.scopes.iter().map(|s| Scope::short(s)).collect();
        let mut payload = status.to_json();
        if let Value::Object(map) = &mut payload {
            map.insert("profile".into(), json!(name.as_str()));
            map.insert("scopes".into(), json!(scopes));
            if let Some(id) = &record.client_id {
                map.insert("client".into(), json!(id_hint(id)));
            }
            if let Some(at) = &record.signed_in_at {
                map.insert("signed_in_at".into(), json!(at));
            }
        }
        if json_mode {
            output::json(&payload);
        } else {
            println!("Profile: {name}");
            status.render();
            if !scopes.is_empty() {
                println!("Scopes: {}", scopes.join(", "));
            }
            if let Some(id) = &record.client_id {
                println!("Client: {}", id_hint(id));
            }
        }
        if let Some(problem) = problem {
            eprintln!(
                "warning: {problem}. Sign in again with `{}`",
                self.login_command(&name)
            );
        } else if !present {
            eprintln!(
                "Drive profile `{name}` is not signed in: `{}`",
                self.login_command(&name)
            );
        }
        Ok(())
    }

    fn logout(
        &self,
        given: Option<&str>,
        args: &LogoutArgs,
        json_mode: bool,
    ) -> Result<(), CliError> {
        let name = self.profile(given)?;
        let account = self.account(&name);
        let mut revoked = false;
        if args.revoke && self.vault.contains(&token_entry(&name))? {
            let stored = account.stored()?;
            if let Some(refresh) = stored.refresh_token() {
                revoked = revoke(&self.endpoints.revoke, refresh.expose())?;
            }
        }
        let lock = RefreshLock::at(self.lock_path.clone());
        let signed_out = lock.forget(|| self.vault.delete(&token_entry(&name)))?;
        if args.forget {
            self.vault.delete(&client_entry(&name))?;
            self.store.remove(&name)?;
        }
        let payload = json!({
            "profile": name.as_str(),
            "signed_out": signed_out,
            "revoked": revoked,
            "forgotten": args.forget,
        });
        output::emit(json_mode, "drive-logout", payload, |_| {
            println!(
                "Drive profile `{name}`: {}{}{}.",
                if signed_out {
                    "signed out"
                } else {
                    "was not signed in"
                },
                if revoked {
                    ", the grant revoked at Google"
                } else {
                    ""
                },
                if args.forget {
                    ", profile and client removed"
                } else {
                    ""
                }
            );
        });
        Ok(())
    }

    fn about(&self, given: Option<&str>, json_mode: bool) -> Result<(), CliError> {
        let name = self.profile(given)?;
        let about = self.api(&name)?.about()?;
        let payload = json!({
            "profile": name.as_str(),
            "account": about.email,
            "limit": about.limit,
            "usage": about.usage,
            "usage_in_drive": about.usage_in_drive,
            "usage_in_trash": about.usage_in_trash,
            "free": about.free(),
        });
        output::emit(json_mode, "drive-about", payload, |v| output::kv(v, 0));
        Ok(())
    }

    fn folder_create(
        &self,
        given: Option<&str>,
        name: &str,
        json_mode: bool,
    ) -> Result<(), CliError> {
        let profile = self.profile(given)?;
        let name = name.trim();
        if name.is_empty() || name.contains('/') {
            return Err(CliError::Usage(
                "a folder name is needed, with no `/` in it".into(),
            ));
        }
        let api = self.api(&profile)?;
        let what = format!("making the folder `{name}` in Drive profile `{profile}`");
        let mut found = api.named(&what, "root", name, Some(true))?;
        let (folder, existing) =
            match found.len() {
                0 => (api.create_folder(&what, "root", name)?, false),
                1 => (found.pop().expect("one"), true),
                n => {
                    return Err(CliError::Usage(format!(
                    "My Drive has {n} folders named `{name}` that this sign-in can see (ids {}); \
                     nothing was made",
                    found.iter().map(|f| f.id.as_str()).collect::<Vec<_>>().join(", ")
                ))
                    .because(Reason::Provider("duplicate_name"))
                    .with_remedy("keep one of them in Google Drive, or choose another name"))
                }
            };
        let spec = DriveSpec::new(profile.clone(), folder.id.clone())?;
        let payload = json!({
            "profile": profile.as_str(),
            "name": folder.name,
            "id": folder.id,
            "spec": spec.to_string(),
            "existing": existing,
        });
        output::emit(json_mode, "drive-folder", payload, |_| {
            println!(
                "{} folder `{}` in Drive profile `{profile}`: {}",
                if existing { "Found the" } else { "Made the" },
                folder.name,
                spec
            );
        });
        Ok(())
    }

    fn folder_list(&self, given: Option<&str>, json_mode: bool) -> Result<(), CliError> {
        let profile = self.profile(given)?;
        let api = self.api(&profile)?;
        let what = format!("listing the folders in Drive profile `{profile}`");
        let q = format!(
            "'root' in parents and trashed = false and mimeType = {}",
            super::files::quote(super::files::FOLDER_MIME)
        );
        let mut folders = api.list(&what, &q)?;
        folders.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let items: Vec<Value> = folders
            .iter()
            .map(|f| {
                json!({
                    "name": f.name,
                    "id": f.id,
                    "spec": format!("{}{profile}/{}", super::drive::SPEC_PREFIX, f.id),
                    "modified": f.modified_time,
                })
            })
            .collect();
        if json_mode {
            output::json(
                &json!({"schema": "drive-folder-list/v1", "profile": profile.as_str(), "folders": items}),
            );
        } else if items.is_empty() {
            eprintln!(
                "(no folders this sign-in can see; make one with `{} folder create {} {profile} <NAME>`)",
                self.command, self.profile_flag
            );
        } else {
            output::table(&items);
        }
        Ok(())
    }

    fn import_client(
        &self,
        given: Option<&str>,
        file: Option<&std::path::Path>,
        stdin: bool,
        overwrite: bool,
        json_mode: bool,
    ) -> Result<(), CliError> {
        let name = self.new_name(given)?;
        let client = self
            .read_client(file, stdin)?
            .ok_or_else(|| CliError::Usage("give `--file <PATH>` or `--stdin`".into()))?;
        let mut record = self.store.get(&name)?.unwrap_or_default();
        let changed = record
            .client_id
            .as_deref()
            .is_some_and(|id| id != client.client_id());
        if changed && !overwrite {
            return Err(CliError::Usage(format!(
                "Drive profile `{name}` already has another OAuth client ({}); nothing was changed",
                record.client_id.as_deref().map(id_hint).unwrap_or_default()
            ))
            .with_remedy(
                "pass --overwrite to replace it; the profile's sign-in is removed with it",
            ));
        }
        if changed {
            record.scopes.clear();
            record.signed_in_at = None;
        }
        // The new client and its record first, the old sign-in last: a
        // failure between leaves a token Google refuses for the new client
        // (exit 3, naming the sign-in), never a profile that lost its
        // sign-in while still naming the old client.
        self.vault
            .put_json(&client_entry(&name), &client.to_stored())?;
        record.client_id = Some(client.client_id().to_string());
        self.store.put(&name, &record)?;
        if changed {
            let lock = RefreshLock::at(self.lock_path.clone());
            lock.forget(|| self.vault.delete(&token_entry(&name)))?;
        }
        let payload = json!({
            "profile": name.as_str(),
            "client": client.id_hint(),
            "project": client.project_id(),
            "stored": true,
        });
        output::emit(json_mode, "drive-client", payload, |_| {
            println!(
                "Stored OAuth client {} for Drive profile `{name}`. Sign in with `{}`.",
                client.id_hint(),
                self.login_command(&name)
            );
        });
        if let Some(path) = file {
            eprintln!(
                "note: the OAuth client is in the vault now; {} can be deleted",
                path.display()
            );
        }
        Ok(())
    }

    fn list(&self, json_mode: bool) -> Result<(), CliError> {
        let profiles = self.store.load()?;
        let mut items = Vec::new();
        for name in profiles.names() {
            let record: DriveProfile = profiles.settings(name)?;
            items.push(json!({
                "name": name.as_str(),
                "account": record.account,
                "signed_in": self.vault.contains(&token_entry(name))?,
                "scopes": record.scopes.iter().map(|s| Scope::short(s)).collect::<Vec<_>>(),
                "client": record.client_id.as_deref().map(id_hint),
            }));
        }
        if json_mode {
            output::json(&json!({"schema": "profile-list/v1", "profiles": items}));
        } else if items.is_empty() {
            eprintln!(
                "(no Drive profiles; add one with `{} login {} <NAME>`)",
                self.command, self.profile_flag
            );
        } else {
            let rows: Vec<Value> = items
                .iter()
                .map(|i| {
                    json!({
                        "name": i["name"],
                        "account": i["account"],
                        "signed_in": i["signed_in"],
                        "scopes": i["scopes"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
                    })
                })
                .collect();
            output::table(&rows);
        }
        Ok(())
    }

    fn remove(
        &self,
        name: &str,
        force: bool,
        interactive: bool,
        json_mode: bool,
    ) -> Result<(), CliError> {
        let name = ProfileName::parse(name)?;
        let configured = self.store.load()?;
        if !configured.contains(&name) {
            let names: Vec<&str> = configured.names().map(ProfileName::as_str).collect();
            return Err(CliError::Usage(format!(
                "no Drive profile `{name}`; configured: {}",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            )));
        }
        confirm::require_confirmable(
            force,
            interactive,
            &format!("removing Drive profile `{name}`"),
        )?;
        confirm::confirm(
            force,
            &format!("Remove Drive profile `{name}`, its sign-in and its OAuth client, from this machine?"),
        )?;
        let lock = RefreshLock::at(self.lock_path.clone());
        lock.forget(|| self.vault.delete(&token_entry(&name)))?;
        self.vault.delete(&client_entry(&name))?;
        self.store.remove(&name)?;
        output::emit(
            json_mode,
            "drive-profile-removed",
            json!({"profile": name.as_str(), "removed": true}),
            |_| println!("Removed Drive profile `{name}`."),
        );
        Ok(())
    }
}

/// Revoke a token at Google: `true` when revoked, `false` when Google no
/// longer knew it (already revoked or expired).
fn revoke(endpoint: &str, token: &str) -> Result<bool, CliError> {
    pk_cli_core::transport::require_secure_url(endpoint)?;
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CliError::Other(format!("failed to build the HTTP client: {e}")))?;
    let resp = http
        .post(endpoint)
        .form(&[("token", token)])
        .send()
        .map_err(|e| {
            CliError::Upstream(format!("revoking at Google failed: {}", e.without_url()))
        })?;
    match resp.status().as_u16() {
        200 => Ok(true),
        400 => Ok(false),
        s => Err(CliError::Upstream(format!("revoking at Google: HTTP {s}"))),
    }
}
