//! Runtime secret resolution across the family's sources, in a configurable
//! order: an environment variable, the OS keychain, and 1Password.
//!
//! A CLI declares each secret once as a [`SecretSpec`] (its keychain
//! account, and optionally an env var and an `op://` reference) and asks a
//! [`SecretResolver`] for it. The resolver walks a [`SourceOrder`]:
//!
//! - a source the spec does not declare is skipped;
//! - a declared source that holds nothing (env var unset, no keychain item)
//!   falls through to the next;
//! - the first source that answers wins, and later sources are never read,
//!   so a keychain hit costs no `op` call and an env hit costs no keychain
//!   prompt;
//! - a source that **fails** stops the walk with its error. A dismissed
//!   1Password approval must not fall through to a keychain read (a second
//!   prompt) or to a stale keychain copy the user did not choose.
//!
//! `Ok(None)` means every declared source was empty: the caller prompts, or
//! ends with exit 3 naming `<bin> auth login`.
//!
//! The default order is env, then keychain, then 1Password (see DESIGN.md
//! §1.7 for why). A CLI exposes the order as a config key, `secret_sources`,
//! holding the text form (`<bin> config set secret_sources op,keychain`);
//! [`SourceOrder`] parses it and (de)serializes as that string.

use std::fmt;
use std::str::FromStr;

use pk_cli_core::CliError;
use serde::{Deserialize, Serialize};

use crate::op::{OnePassword, OpRef};
use crate::{CredentialStore, ItemStore, Secret};

/// One place a secret can come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A named environment variable (`op run --` and CI inject these).
    Env,
    /// The OS keychain item under `piekstra.<bin>`.
    Keychain,
    /// A 1Password secret reference, read with `op read`.
    Op,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Env => "env",
            SourceKind::Keychain => "keychain",
            SourceKind::Op => "op",
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SourceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "env" => Ok(SourceKind::Env),
            "keychain" => Ok(SourceKind::Keychain),
            "op" | "1password" => Ok(SourceKind::Op),
            other => Err(format!(
                "unknown secret source `{other}` (known: env, keychain, op)"
            )),
        }
    }
}

/// The order sources are tried in. Non-empty, no repeats; a source left out
/// is never read. Text form: comma-separated, `env,keychain,op`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourceOrder(Vec<SourceKind>);

impl SourceOrder {
    pub fn new(sources: &[SourceKind]) -> Result<Self, String> {
        if sources.is_empty() {
            return Err("the secret source order is empty".into());
        }
        for (i, s) in sources.iter().enumerate() {
            if sources[..i].contains(s) {
                return Err(format!("secret source `{s}` is listed twice"));
            }
        }
        Ok(SourceOrder(sources.to_vec()))
    }

    pub fn sources(&self) -> &[SourceKind] {
        &self.0
    }
}

impl Default for SourceOrder {
    /// Env, then keychain, then 1Password.
    fn default() -> Self {
        SourceOrder(vec![SourceKind::Env, SourceKind::Keychain, SourceKind::Op])
    }
}

impl FromStr for SourceOrder {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let kinds = s
            .split(',')
            .filter(|p| !p.trim().is_empty())
            .map(str::parse)
            .collect::<Result<Vec<SourceKind>, _>>()?;
        SourceOrder::new(&kinds)
    }
}

impl TryFrom<String> for SourceOrder {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<SourceOrder> for String {
    fn from(o: SourceOrder) -> String {
        o.to_string()
    }
}

impl fmt::Display for SourceOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<&str> = self.0.iter().map(|s| s.as_str()).collect();
        f.write_str(&parts.join(","))
    }
}

/// Where one secret may live. The keychain account is always declared; the
/// env var and the 1Password reference are opt-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretSpec {
    account: String,
    env: Option<String>,
    op: Option<OpRef>,
}

impl SecretSpec {
    /// A secret stored in the keychain under `account`.
    pub fn new(account: impl Into<String>) -> Self {
        SecretSpec {
            account: account.into(),
            env: None,
            op: None,
        }
    }

    /// Also read from this environment variable (e.g. `FPL_PASSWORD`).
    pub fn env(mut self, var: impl Into<String>) -> Self {
        self.env = Some(var.into());
        self
    }

    /// Also read from 1Password at this reference. `None` leaves it unset,
    /// so a reference from optional config passes straight through.
    pub fn op(mut self, reference: Option<OpRef>) -> Self {
        self.op = reference;
        self
    }

    pub fn account(&self) -> &str {
        &self.account
    }
}

/// A resolved secret and the source that answered.
#[derive(Debug)]
pub struct Resolved {
    pub secret: Secret,
    pub source: SourceKind,
}

/// Resolves [`SecretSpec`]s against the env, a keychain store and 1Password.
pub struct SecretResolver<'a> {
    store: &'a CredentialStore,
    op: OnePassword,
    order: SourceOrder,
}

impl<'a> SecretResolver<'a> {
    /// The default order, `op` from `PATH`.
    pub fn new(store: &'a CredentialStore) -> Self {
        SecretResolver {
            store,
            op: OnePassword::new(),
            order: SourceOrder::default(),
        }
    }

    pub fn order(mut self, order: SourceOrder) -> Self {
        self.order = order;
        self
    }

    /// The `op` to read through (a different program, account or timeout).
    pub fn one_password(mut self, op: OnePassword) -> Self {
        self.op = op;
        self
    }

    /// Walk the order for `spec`. See the module docs for the rules.
    pub fn resolve(&self, spec: &SecretSpec) -> Result<Option<Resolved>, CliError> {
        resolve_in(self.store, &self.op, &self.order, spec)
    }
}

fn resolve_in(
    store: &impl ItemStore,
    op: &OnePassword,
    order: &SourceOrder,
    spec: &SecretSpec,
) -> Result<Option<Resolved>, CliError> {
    for &source in order.sources() {
        let found = match source {
            SourceKind::Env => match &spec.env {
                None => None,
                Some(var) => match std::env::var(var) {
                    Ok(v) if v.is_empty() => {
                        return Err(CliError::Usage(format!("${var} is set but empty")))
                    }
                    Ok(v) => Some(Secret::new(v)),
                    Err(std::env::VarError::NotPresent) => None,
                    Err(std::env::VarError::NotUnicode(_)) => {
                        return Err(CliError::Usage(format!("${var} is not valid UTF-8")))
                    }
                },
            },
            SourceKind::Keychain => store.get(&spec.account)?,
            SourceKind::Op => match &spec.op {
                None => None,
                Some(reference) => Some(op.read(reference)?),
            },
        };
        if let Some(secret) = found {
            return Ok(Some(Resolved { secret, source }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::MemStore;

    #[test]
    fn the_order_parses_validates_and_prints() {
        assert_eq!(
            "env,keychain,op".parse::<SourceOrder>().unwrap(),
            SourceOrder::default()
        );
        let o: SourceOrder = " 1Password , keychain ".parse().unwrap();
        assert_eq!(o.sources(), [SourceKind::Op, SourceKind::Keychain]);
        assert_eq!(o.to_string(), "op,keychain");
        assert!("".parse::<SourceOrder>().is_err());
        assert!("op,op".parse::<SourceOrder>().is_err());
        let err = "env,vault".parse::<SourceOrder>().unwrap_err();
        assert!(err.contains("vault"), "{err}");
    }

    #[test]
    fn the_order_round_trips_through_config_as_a_string() {
        #[derive(Serialize, Deserialize)]
        struct Cfg {
            secret_sources: SourceOrder,
        }
        let cfg: Cfg = serde_json::from_str(r#"{"secret_sources":"op,env"}"#).unwrap();
        assert_eq!(
            cfg.secret_sources.sources(),
            [SourceKind::Op, SourceKind::Env]
        );
        assert_eq!(
            serde_json::to_string(&cfg).unwrap(),
            r#"{"secret_sources":"op,env"}"#
        );
        assert!(serde_json::from_str::<Cfg>(r#"{"secret_sources":"op,op"}"#).is_err());
    }

    #[test]
    fn nothing_declared_or_stored_is_none() {
        let store = MemStore::new("piekstra.x");
        let order = SourceOrder::default();
        let spec = SecretSpec::new("password").env("PK_CLI_RESOLVE_TEST_UNSET");
        let got = resolve_in(&store, &OnePassword::new(), &order, &spec).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn a_set_but_empty_env_var_is_a_usage_error() {
        std::env::set_var("PK_CLI_RESOLVE_TEST_EMPTY", "");
        let store = MemStore::new("piekstra.x").with("password", "from-keychain");
        let spec = SecretSpec::new("password").env("PK_CLI_RESOLVE_TEST_EMPTY");
        let err =
            resolve_in(&store, &OnePassword::new(), &SourceOrder::default(), &spec).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    mod with_fake_op {
        use super::*;
        use crate::op::fake::FakeOp;

        fn spec(env: &str) -> SecretSpec {
            SecretSpec::new("password")
                .env(env)
                .op(Some("op://Example/Login/password".parse().unwrap()))
        }

        fn op(fake: &FakeOp) -> OnePassword {
            OnePassword::new().program(fake.program())
        }

        #[test]
        fn by_default_env_beats_keychain_beats_op() {
            let fake = FakeOp::answering("from-op");
            std::env::set_var("PK_CLI_RESOLVE_TEST_ENV", "from-env");
            let store = MemStore::new("piekstra.x").with("password", "from-keychain");
            let order = SourceOrder::default();

            let got = resolve_in(&store, &op(&fake), &order, &spec("PK_CLI_RESOLVE_TEST_ENV"))
                .unwrap()
                .unwrap();
            assert_eq!(
                (got.secret.expose(), got.source),
                ("from-env", SourceKind::Env)
            );

            let got = resolve_in(
                &store,
                &op(&fake),
                &order,
                &spec("PK_CLI_RESOLVE_TEST_NONE"),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                (got.secret.expose(), got.source),
                ("from-keychain", SourceKind::Keychain)
            );
            assert_eq!(fake.calls(), 0, "a keychain hit costs no op call");

            let empty = MemStore::new("piekstra.x");
            let got = resolve_in(
                &empty,
                &op(&fake),
                &order,
                &spec("PK_CLI_RESOLVE_TEST_NONE"),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                (got.secret.expose(), got.source),
                ("from-op", SourceKind::Op)
            );
            assert_eq!(fake.calls(), 1);
        }

        #[test]
        fn an_op_first_order_reads_op_even_with_a_keychain_copy() {
            let fake = FakeOp::answering("from-op");
            let store = MemStore::new("piekstra.x").with("password", "from-keychain");
            let order: SourceOrder = "op,keychain".parse().unwrap();
            let got = resolve_in(
                &store,
                &op(&fake),
                &order,
                &spec("PK_CLI_RESOLVE_TEST_NONE"),
            )
            .unwrap()
            .unwrap();
            assert_eq!(got.source, SourceKind::Op);
            assert_eq!(got.secret.expose(), "from-op");
        }

        #[test]
        fn an_op_failure_stops_the_walk_instead_of_falling_through() {
            let fake = FakeOp::failing_with("not-signed-in.txt");
            let store = MemStore::new("piekstra.x").with("password", "from-keychain");
            let order: SourceOrder = "op,keychain".parse().unwrap();
            let err = resolve_in(
                &store,
                &op(&fake),
                &order,
                &spec("PK_CLI_RESOLVE_TEST_NONE"),
            )
            .unwrap_err();
            assert_eq!(err.exit_code(), 3);
            assert!(err.to_string().contains("`op signin`"), "{err}");
            assert_eq!(fake.calls(), 1, "probed once, never retried");
        }

        #[test]
        fn a_source_left_out_of_the_order_is_never_read() {
            let fake = FakeOp::answering("from-op");
            let store = MemStore::new("piekstra.x");
            let order: SourceOrder = "env,keychain".parse().unwrap();
            let got = resolve_in(
                &store,
                &op(&fake),
                &order,
                &spec("PK_CLI_RESOLVE_TEST_NONE"),
            )
            .unwrap();
            assert!(got.is_none());
            assert_eq!(fake.calls(), 0);
        }

        #[test]
        fn a_spec_without_a_reference_never_calls_op() {
            let fake = FakeOp::answering("from-op");
            let store = MemStore::new("piekstra.x");
            let spec = SecretSpec::new("password").op(None);
            let got = resolve_in(&store, &op(&fake), &SourceOrder::default(), &spec).unwrap();
            assert!(got.is_none());
            assert_eq!(fake.calls(), 0);
        }
    }
}
