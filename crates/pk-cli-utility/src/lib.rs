//! The utility-domain profile (`utility/v1`, SPEC v1.1 §1.8).
//!
//! Account-portal CLIs (`fpl`, `tojfl`, `lrfl`, `xfin`, …) share the same
//! domain — a balance that comes due, statements, payments, metered usage —
//! but historically spelled it differently, so drivers like `utiman` carried
//! per-provider manifest hacks (`balance-fields = ["balance.cents"]`,
//! `scale = "cents"`, `items-path = "payments"`). This crate owns the shared
//! shapes: a CLI that emits them needs no domain configuration in a driver
//! at all.
//!
//! Rules (see DESIGN.md §1.8 for the command spellings):
//! - `summary` and `balance` both emit [`UtilitySummary`] — one DTO, two
//!   entry points.
//! - Every list command emits a [`Paged`] envelope with the records under
//!   `items` (drivers stop guessing at `items-path`).
//! - Money is [`Money`] (string-decimal, never floats, never bare cents);
//!   dates are ISO `YYYY-MM-DD`; range flags come from [`RangeArgs`].
//!
//! A CLI advertises the profile via `info`:
//! `CliInfo::new(...).with_profiles(&[pk_cli_utility::PROFILE])`.

use pk_cli_core::{output, Money};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The list envelope ([`Paged`]) and range flags ([`RangeArgs`]) every profile
/// `list` command uses now live in `pk-cli-core` — they are profile-agnostic.
/// Re-exported here so existing `pk_cli_utility::{Paged, RangeArgs}` imports
/// keep working unchanged.
pub use pk_cli_core::{Paged, RangeArgs};

/// Profile identifier for `cli-info/v1` `profiles`.
pub const PROFILE: &str = "utility/v1";

/// Emit any DTO per the output contract: pretty JSON in json mode, the
/// standard key/value block or table otherwise.
pub fn emit<T: Serialize>(dto: &T, json_mode: bool) {
    let v = serde_json::to_value(dto).unwrap_or(Value::Null);
    if json_mode {
        output::json(&v);
    } else {
        output::render(&v);
    }
}

/// The canonical `summary` / `balance` DTO (`utility-summary/v1`): what a
/// driver needs to render an account card — amount due and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtilitySummary {
    pub schema: String,
    /// Current amount due (zero when paid up; negative = credit).
    pub balance: Money,
    /// ISO `YYYY-MM-DD`, when the provider states one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autopay: Option<bool>,
}

impl UtilitySummary {
    pub fn new(balance: Money) -> Self {
        UtilitySummary {
            schema: "utility-summary/v1".into(),
            balance,
            due_date: None,
            account: None,
            autopay: None,
        }
    }
}

/// One bill/statement (`statement/v1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statement {
    /// Provider identifier — used by `bills get <ID>`.
    pub id: String,
    /// Issue date, ISO `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    pub amount: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid: Option<bool>,
}

/// One posted payment (`payment/v1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payment {
    /// ISO `YYYY-MM-DD`.
    pub date: String,
    pub amount: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<String>,
}

/// Metered consumption for one period (`usage-period/v1`). Quantity is a
/// plain number with an explicit unit — quantities are not money.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsagePeriod {
    /// Period label: an ISO date, `YYYY-MM`, or a provider cycle name.
    pub period: String,
    pub quantity: f64,
    /// e.g. `kWh`, `gallons`, `GB`.
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Money>,
}

/// One ledger entry (`transaction/v1`): charges, payments, credits, deposits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// ISO `YYYY-MM-DD`.
    pub date: String,
    /// Signed: charges positive, payments/credits negative.
    pub amount: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// e.g. `charge` | `payment` | `credit` | `deposit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_shape() {
        let mut s = UtilitySummary::new(Money::usd("128.44"));
        s.due_date = Some("2026-08-01".into());
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["schema"], "utility-summary/v1");
        assert_eq!(v["balance"]["amount"], "128.44");
        assert_eq!(v["due_date"], "2026-08-01");
        assert!(v.get("account").is_none());
        assert!(v.get("autopay").is_none());
    }

    #[test]
    fn paged_envelope_shape() {
        let page = Paged::new(
            "statement",
            vec![Statement {
                id: "2026-06".into(),
                date: Some("2026-06-15".into()),
                amount: Money::usd("97.10"),
                due_date: Some("2026-07-05".into()),
                paid: Some(true),
            }],
        );
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["schema"], "statement-list/v1");
        assert_eq!(v["items"][0]["id"], "2026-06");
        assert!(v.get("next_cursor").is_none());
        assert!(v.get("total").is_none());
    }

    #[test]
    fn usage_quantity_is_numeric_with_unit() {
        let u = UsagePeriod {
            period: "2026-06".into(),
            quantity: 843.5,
            unit: "kWh".into(),
            cost: None,
        };
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["quantity"], 843.5);
        assert_eq!(v["unit"], "kWh");
    }
}
