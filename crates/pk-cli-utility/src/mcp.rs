//! The `utility/v1` profile's MCP tools (feature `mcp`; DESIGN.md §1.8, §1.9).
//!
//! The portal CLIs of this profile answer the same questions, so the tools
//! are written once: `<prefix>_balance`, `<prefix>_bills`, `<prefix>_payments`
//! and `<prefix>_usage`, read-only, over the profile's own DTOs. A CLI writes
//! a [`UtilitySource`] over the reads and mappers its commands already use and
//! hands it to [`UtilityTools`]:
//!
//! ```ignore
//! let server = Server::new("fpl", env!("CARGO_PKG_VERSION"), "FPL", "fpl");
//! pk_cli_mcp::run(args, UtilityTools::new(server, LiveSource::new(cfg)).metering("electricity"))
//! ```
//!
//! Nothing here touches a credential, a file or the network: listing the tools
//! calls no [`UtilitySource`] method, and every call reaches the provider only
//! through one. The source is the seam DESIGN.md §2.3 describes (async, no
//! `Send` bound), so this module builds for `wasm32-unknown-unknown` too.

use pk_cli_core::{dates, resolve, CliError, Money};
use pk_cli_mcp::{
    parse_args, ListArgs, Order, Page, PageRequest, PositionKey, PositionRequest, ResponseFormat,
    Server, ToolDef, ToolError, ToolOutput, Toolset,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Payment, Statement, UsagePeriod, UtilitySummary};

/// One account on the login, for resolving the tools' `account` argument and
/// for telling the model which others exist.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UtilityAccount {
    /// The provider's account number.
    pub id: String,
    /// What a person calls it: the service address, or the name on the bill.
    #[serde(default)]
    pub name: Option<String>,
}

/// Where the tools get a provider's data: blocking HTTP natively, `fetch` in a
/// Worker, fixtures in tests.
///
/// `account` is a resolved id from [`Self::accounts`], or, when that list is
/// empty, the argument after a check that it could be an account number
/// (letters, digits, `-`, `_`, `.`; no `..`; at most 64 characters), which a
/// source may put in a request path. `None` means the provider's default
/// account (the one its commands read with no flag). Lists come newest first.
#[allow(async_fn_in_trait)] // deliberate: no `Send` bound, as in `Toolset`
pub trait UtilitySource {
    /// Every account on the login. Empty when the provider has no list to
    /// resolve against (no login, say); the argument is then taken as an id.
    async fn accounts(&self) -> Result<Vec<UtilityAccount>, CliError>;
    /// What is due now.
    async fn summary(&self, account: Option<&str>) -> Result<UtilitySummary, CliError>;
    /// The bills the provider keeps. The toolset filters and pages them.
    async fn statements(&self, account: Option<&str>) -> Result<Vec<Statement>, CliError>;
    /// Posted payments. `since` is the earliest date asked for, for a provider
    /// whose query takes one; the toolset filters and pages either way.
    async fn payments(
        &self,
        account: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<Payment>, CliError>;
    /// Metered consumption by period. Quantities and units as the provider
    /// reports them.
    async fn usage(&self, account: Option<&str>) -> Result<Vec<UsagePeriod>, CliError>;
}

/// The toolset a `utility/v1` CLI serves from `<bin> mcp`.
pub struct UtilityTools<S> {
    server: Server,
    source: S,
    metering: Option<String>,
    payments: bool,
    accounts: bool,
}

impl<S: UtilitySource> UtilityTools<S> {
    /// All four tools, with an `account` argument. Tool names take their
    /// prefix from `server.tool_prefix`.
    pub fn new(server: Server, source: S) -> Self {
        Self {
            server,
            source,
            metering: Some("usage".into()),
            payments: true,
            accounts: true,
        }
    }

    /// What `<prefix>_usage` measures, for its description: `electricity`,
    /// `water`, `internet data`.
    pub fn metering(mut self, what: impl Into<String>) -> Self {
        self.metering = Some(what.into());
        self
    }

    /// The provider reports no metered usage: serve no `<prefix>_usage`.
    pub fn without_usage(mut self) -> Self {
        self.metering = None;
        self
    }

    /// The provider reports no posted payments: serve no `<prefix>_payments`.
    pub fn without_payments(mut self) -> Self {
        self.payments = false;
        self
    }

    /// One account per login: no tool takes an `account` argument, and
    /// [`UtilitySource::accounts`] is never called.
    pub fn single_account(mut self) -> Self {
        self.accounts = false;
        self
    }

    fn name(&self, what: &str) -> String {
        format!("{}_{what}", self.server.tool_prefix)
    }

    /// What a page's hint names for narrowing.
    fn narrow(&self) -> &'static str {
        if self.accounts {
            "since/until or account"
        } else {
            "since/until"
        }
    }

    /// The `account` argument as the id the source takes.
    fn pick_account(&self, accounts: &[UtilityAccount], query: &str) -> Result<String, ToolError> {
        if accounts.is_empty() {
            if !could_be_an_account_number(query) {
                return Err(ToolError::usage(
                    "account must be the account number here: this login has no list of \
                     addresses to match against",
                ));
            }
            return Ok(query.to_string());
        }
        resolve::pick(
            accounts,
            query,
            |a| vec![a.id.clone()],
            |a| a.name.as_deref().unwrap_or(""),
            "account",
        )
        .map(|a| a.id.clone())
        .map_err(|e| {
            // The ladder's message lists each candidate's id; concise output
            // holds account numbers back, and this path has no format to ask
            // for them with.
            let error = match e.base() {
                CliError::NotFound(message) => ToolError::not_found(
                    accounts
                        .iter()
                        .fold(message.clone(), |m, a| m.replace(&a.id, &masked(&a.id))),
                ),
                _ => e.into(),
            };
            error.with_fix(format!(
                "Call {} with no account to see the accounts on this login (response_format \
                 detailed adds their numbers, which always match one), then call again",
                self.name("balance")
            ))
        })
    }

    async fn account(&self, query: Option<&str>) -> Result<Option<String>, ToolError> {
        let Some(query) = present(query) else {
            return Ok(None);
        };
        let accounts = self.source.accounts().await?;
        self.pick_account(&accounts, query).map(Some)
    }

    async fn balance(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: BalanceArgs = parse_args(args)?;
        let detailed = args.response_format.unwrap_or_default().is_detailed();
        let query = present(args.account.as_deref());
        let mut accounts_unavailable = false;
        let accounts = match (self.accounts, query) {
            (false, _) => Vec::new(),
            (true, Some(_)) => self.source.accounts().await?,
            // Nothing needs the list to resolve, so failing to read it must not
            // cost the balance. The result says so, or a login with several
            // accounts would read as a login with one.
            (true, None) => self.source.accounts().await.unwrap_or_else(|_| {
                accounts_unavailable = true;
                Vec::new()
            }),
        };
        let account = query.map(|q| self.pick_account(&accounts, q)).transpose()?;
        let summary = self.source.summary(account.as_deref()).await?;
        let (balance, currency) = amount(&summary.balance);
        let id = summary.account.or(account);
        let name = id
            .as_deref()
            .and_then(|id| accounts.iter().find(|a| a.id == id))
            .and_then(|a| a.name.clone());
        ToolOutput::new(&BalanceOut {
            balance,
            currency,
            due_date: summary.due_date,
            autopay: summary.autopay,
            account: name.or_else(|| id.as_deref().map(masked)),
            account_id: id.filter(|_| detailed),
            accounts_unavailable,
            accounts: if accounts.len() > 1 {
                // An account whose name does not single it out can only be
                // asked for by its number.
                let names = |name: &Option<String>| {
                    name.is_some() && accounts.iter().filter(|a| a.name == *name).count() == 1
                };
                accounts
                    .iter()
                    .map(|a| AccountItem {
                        id: (detailed || !names(&a.name)).then(|| a.id.clone()),
                        name: a.name.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    async fn bills(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ListToolArgs = parse_args(args)?;
        let page = PositionRequest::new(&args.list, &args.filter())?;
        let (since, until) = args.bounds()?;
        let account = self.account(args.account.as_deref()).await?;
        let detailed = args.list.format().is_detailed();
        let rows = self
            .source
            .statements(account.as_deref())
            .await?
            .into_iter()
            .filter(|s| {
                s.date
                    .as_deref()
                    .or(s.due_date.as_deref())
                    .map_or(true, |d| in_range(d, since.as_deref(), until.as_deref()))
            })
            .map(|s| {
                let key = PositionKey::new(
                    s.date
                        .as_deref()
                        .or(s.due_date.as_deref())
                        .unwrap_or_default(),
                    &s.id,
                );
                let (amount, currency) = amount(&s.amount);
                let item = BillItem {
                    date: s.date,
                    amount,
                    currency,
                    due_date: s.due_date,
                    paid: s.paid,
                    id: detailed.then_some(s.id),
                };
                (key, item)
            })
            .collect();
        ToolOutput::new(&page.page(rows, Order::Descending, self.narrow())?)
    }

    async fn payments(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ListToolArgs = parse_args(args)?;
        let page = PositionRequest::new(&args.list, &args.filter())?;
        let (since, until) = args.bounds()?;
        let account = self.account(args.account.as_deref()).await?;
        let detailed = args.list.format().is_detailed();
        let rows = self
            .source
            .payments(account.as_deref(), since.as_deref())
            .await?
            .into_iter()
            .filter(|p| in_range(&p.date, since.as_deref(), until.as_deref()))
            .map(|p| {
                // A confirmation number when the provider gives one. Its status can change
                // while a payment posts, so the status is no part of this.
                let id = p.confirmation.as_deref().unwrap_or(&p.amount.amount);
                let key = PositionKey::new(&*p.date, id);
                let (amount, currency) = amount(&p.amount);
                let item = PaymentItem {
                    date: p.date,
                    amount,
                    currency,
                    status: p.status,
                    method: p.method.filter(|_| detailed),
                    confirmation: p.confirmation.filter(|_| detailed),
                };
                (key, item)
            })
            .collect();
        ToolOutput::new(&page.page(rows, Order::Descending, self.narrow())?)
    }

    async fn usage(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ListToolArgs = parse_args(args)?;
        let page = PageRequest::new(&args.list, &args.filter())?;
        let (since, until) = args.bounds()?;
        let account = self.account(args.account.as_deref()).await?;
        let rows = self
            .source
            .usage(account.as_deref())
            .await?
            .into_iter()
            .filter(|u| in_range(&u.period, since.as_deref(), until.as_deref()))
            .map(|u| {
                let (cost, currency) = match u.cost.as_ref().map(amount) {
                    Some((cost, currency)) => (Some(cost), currency),
                    None => (None, None),
                };
                UsageItem {
                    period: u.period,
                    quantity: u.quantity,
                    unit: u.unit,
                    cost,
                    currency,
                }
            })
            .collect();
        // By count of rows: a period's label can be a provider's cycle name, which says
        // nothing about where it sorts, and a new period arrives once a billing cycle.
        ToolOutput::new(&page.page(rows, self.narrow())?)
    }
}

impl<S: UtilitySource> Toolset for UtilityTools<S> {
    fn server(&self) -> Server {
        self.server.clone()
    }

    fn tools(&self) -> Vec<ToolDef> {
        let service = &self.server.service;
        let others = if self.accounts {
            ", and the login's other accounts when it has several (accounts_unavailable: true \
             when they could not be read)"
        } else {
            ""
        };
        let narrow = self.narrow();
        let money = "US dollars unless currency is set";
        let mut tools = vec![
            ToolDef::read(
                self.name("balance"),
                format!("{service} balance"),
                format!(
                    "Shows what the user owes {service} now and when it is due. Use it for what \
                     they owe, when the bill is due, or whether autopay is on. Returns balance \
                     (a decimal string, negative for a credit; {money}), due_date, autopay and \
                     account{others}. Detailed adds account numbers."
                ),
            )
            .input::<BalanceArgs>()
            .output::<BalanceOut>(),
            ToolDef::read(
                self.name("bills"),
                format!("{service} bills"),
                format!(
                    "Lists the user's {service} bills, newest first. Use it for what past bills \
                     came to and whether they were paid. Items carry date, amount ({money}), \
                     due_date and paid (detailed adds id). Narrow with {narrow}."
                ),
            )
            .input::<ListToolArgs>()
            .output::<Page<BillItem>>(),
        ];
        if self.payments {
            tools.push(
                ToolDef::read(
                    self.name("payments"),
                    format!("{service} payments"),
                    format!(
                        "Lists payments posted to the user's {service} account, newest first. \
                         Use it to check whether a payment went through or what was paid over a \
                         period. Items carry date, amount ({money}) and status (detailed adds \
                         method and confirmation). Narrow with {narrow}."
                    ),
                )
                .input::<ListToolArgs>()
                .output::<Page<PaymentItem>>(),
            );
        }
        if let Some(metering) = &self.metering {
            tools.push(
                ToolDef::read(
                    self.name("usage"),
                    format!("{service} usage"),
                    format!(
                        "Lists the user's {metering} by billing period, newest first. Use it for \
                         how much they used and how periods compare. Items carry period, \
                         quantity, unit, and cost ({money}) where the provider states one. \
                         Narrow with {narrow}."
                    ),
                )
                .input::<ListToolArgs>()
                .output::<Page<UsageItem>>(),
            );
        }
        if !self.accounts {
            for tool in &mut tools {
                if let Some(props) = tool
                    .input_schema
                    .get_mut("properties")
                    .and_then(Value::as_object_mut)
                {
                    // The dispatcher refuses an argument the schema lacks.
                    props.remove("account");
                }
            }
        }
        tools
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        // The dispatcher only passes names `tools` returned.
        match name
            .strip_prefix(self.server.tool_prefix.as_str())
            .and_then(|n| n.strip_prefix('_'))
        {
            Some("balance") => self.balance(args).await,
            Some("bills") => self.bills(args).await,
            Some("payments") => self.payments(args).await,
            Some("usage") => self.usage(args).await,
            _ => Err(ToolError::other(format!("unknown tool `{name}`"))),
        }
    }
}

/// `<prefix>_balance`'s arguments.
#[derive(Debug, Deserialize, JsonSchema)]
struct BalanceArgs {
    /// The account: its number, or its address or a unique part of it (only
    /// the number when the login lists no addresses). Omit for the default
    /// account.
    #[serde(default)]
    account: Option<String>,
    /// `concise` (default) names accounts by address. `detailed` adds their
    /// numbers.
    #[serde(default)]
    response_format: Option<ResponseFormat>,
}

/// The list tools' arguments.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListToolArgs {
    /// The account: its number, or its address or a unique part of it (only
    /// the number when the login lists no addresses). Omit for the default
    /// account.
    #[serde(default)]
    account: Option<String>,
    /// Earliest date, YYYY-MM-DD.
    #[serde(default)]
    since: Option<String>,
    /// Latest date, YYYY-MM-DD.
    #[serde(default)]
    until: Option<String>,
    #[serde(flatten)]
    list: ListArgs,
}

/// The filters a cursor is bound to.
#[derive(Serialize)]
struct ListFilter<'a> {
    account: Option<&'a str>,
    since: Option<&'a str>,
    until: Option<&'a str>,
}

impl ListToolArgs {
    fn filter(&self) -> ListFilter<'_> {
        ListFilter {
            account: present(self.account.as_deref()),
            since: present(self.since.as_deref()),
            until: present(self.until.as_deref()),
        }
    }

    /// `since` and `until` in canonical ISO form, so they compare as text.
    fn bounds(&self) -> Result<(Option<String>, Option<String>), ToolError> {
        let bound = |name: &str, v: Option<&str>| {
            present(v)
                .map(|d| {
                    dates::parse_iso(d).map(dates::fmt_iso).map_err(|_| {
                        ToolError::usage(format!("{name} must be YYYY-MM-DD; got `{d}`"))
                    })
                })
                .transpose()
        };
        let since = bound("since", self.since.as_deref())?;
        let until = bound("until", self.until.as_deref())?;
        if let (Some(s), Some(u)) = (&since, &until) {
            if s > u {
                return Err(ToolError::usage(format!("since {s} is after until {u}")));
            }
        }
        Ok((since, until))
    }
}

/// `<prefix>_balance`'s result.
#[derive(Debug, Serialize, JsonSchema)]
struct BalanceOut {
    balance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    autopay: Option<bool>,
    /// The account's address, or the end of its number when it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    accounts_unavailable: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    accounts: Vec<AccountItem>,
}

/// One account on the login, in `<prefix>_balance`'s result.
#[derive(Debug, Serialize, JsonSchema)]
struct AccountItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

/// One bill, in `<prefix>_bills`'s result.
#[derive(Debug, Serialize, JsonSchema)]
struct BillItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    paid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

/// One payment, in `<prefix>_payments`'s result.
#[derive(Debug, Serialize, JsonSchema)]
struct PaymentItem {
    date: String,
    amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmation: Option<String>,
}

/// One period, in `<prefix>_usage`'s result.
#[derive(Debug, Serialize, JsonSchema)]
struct UsageItem {
    period: String,
    quantity: f64,
    unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency: Option<String>,
}

/// A profile `Money` as a tool's decimal string, and its currency when that
/// is not US dollars, so the amount always parses as a number.
fn amount(money: &Money) -> (String, Option<String>) {
    let currency = Some(money.currency.clone()).filter(|c| c != "USD");
    (money.amount.clone(), currency)
}

/// An account number as a person recognizes it, without sending all of it: the
/// last four characters, or nothing of one too short to mask.
fn masked(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    match chars.len().checked_sub(4) {
        Some(start) if start > 0 => {
            format!("ending {}", chars[start..].iter().collect::<String>())
        }
        _ => "on file".to_string(),
    }
}

/// Whether an argument with no list to check it against could be an account
/// number. It comes from the model, and a source may put it in a request path.
fn could_be_an_account_number(value: &str) -> bool {
    value.len() <= 64
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Whether a date falls in the bounds, both canonical ISO. A `YYYY-MM` period
/// is in range when its month overlaps them. A date this cannot read (a
/// provider's cycle name) stays in: a person can read it, and a filter that
/// silently hid a bill would answer a different question than the one asked.
fn in_range(date: &str, since: Option<&str>, until: Option<&str>) -> bool {
    let date = date.trim();
    let len = if date.len() == 10 && dates::parse_iso(date).is_ok() {
        10
    } else if date.len() == 7 && dates::parse_iso(&format!("{date}-01")).is_ok() {
        7
    } else {
        return true;
    };
    since.map_or(true, |s| date >= &s[..len]) && until.map_or(true, |u| date <= &u[..len])
}
