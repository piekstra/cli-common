//! Money as string-decimal + currency (SPEC v1 §1.4) — never floats.

use std::fmt;

use serde::{Deserialize, Serialize};

/// `{"amount": "123.45", "currency": "USD"}`. Amount is a decimal string with
/// two fraction digits; arithmetic is intentionally out of scope (CLIs report,
/// they don't do accounting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount: String,
    pub currency: String,
}

impl Money {
    pub fn usd(amount: impl Into<String>) -> Self {
        Money {
            amount: amount.into(),
            currency: "USD".into(),
        }
    }

    /// Build from **minor units** (US cents, pence, …).
    ///
    /// Plenty of provider APIs report money as an integer number of minor
    /// units, and some mix the two scales across endpoints — the same
    /// transaction arriving as `25000` from one and `250.00` from another.
    /// Doing the conversion by hand invites a silent 100× error, so it lives
    /// here with the rest of the money handling. Integer arithmetic
    /// throughout: no float ever touches the value.
    ///
    /// ```
    /// # use pk_cli_core::Money;
    /// assert_eq!(Money::from_cents(25_000).amount, "250.00");
    /// assert_eq!(Money::from_cents(-5).amount, "-0.05");
    /// ```
    pub fn from_cents(cents: i64) -> Self {
        Money::usd(format!(
            "{}{}.{:02}",
            if cents < 0 { "-" } else { "" },
            (cents / 100).abs(),
            (cents % 100).abs()
        ))
    }

    /// Parse a provider-formatted amount like `$1,234.50`, `1234.5`, or
    /// `(12.34)` (accounting negative) into a normalized two-decimal string.
    pub fn parse_usd(raw: &str) -> Option<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        let negative = s.starts_with('(') && s.ends_with(')') || s.starts_with('-');
        let cleaned: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if cleaned.is_empty() {
            return None;
        }
        let value: f64 = cleaned.parse().ok()?;
        let cents = (value * 100.0).round() as i64;
        let cents = if negative { -cents } else { cents };
        Some(Money::usd(format!(
            "{}{}.{:02}",
            if cents < 0 { "-" } else { "" },
            (cents / 100).abs(),
            (cents % 100).abs()
        )))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.currency == "USD" {
            write!(f, "${}", self.amount)
        } else {
            write!(f, "{} {}", self.amount, self.currency)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_formats() {
        assert_eq!(Money::parse_usd("$1,234.50").unwrap().amount, "1234.50");
        assert_eq!(Money::parse_usd("1234.5").unwrap().amount, "1234.50");
        assert_eq!(Money::parse_usd("(12.34)").unwrap().amount, "-12.34");
        assert_eq!(Money::parse_usd("-3").unwrap().amount, "-3.00");
        assert!(Money::parse_usd("").is_none());
        assert!(Money::parse_usd("n/a").is_none());
    }

    #[test]
    fn builds_from_minor_units() {
        assert_eq!(Money::from_cents(25_000).amount, "250.00");
        assert_eq!(Money::from_cents(250).amount, "2.50");
        assert_eq!(Money::from_cents(0).amount, "0.00");
        assert_eq!(Money::from_cents(5).amount, "0.05");
        assert_eq!(Money::from_cents(-25_000).amount, "-250.00");
        // The sign must survive a magnitude below one unit, where `cents / 100`
        // truncates to zero and would otherwise lose it.
        assert_eq!(Money::from_cents(-5).amount, "-0.05");
        assert_eq!(Money::from_cents(i64::MAX).currency, "USD");
    }

    /// The two constructors must agree, since a provider may report the same
    /// amount either way and a CLI can end up calling both.
    #[test]
    fn minor_units_agree_with_parsed_decimals() {
        for (cents, decimal) in [
            (25_000, "250.00"),
            (250, "2.50"),
            (-1_234, "-12.34"),
            (99, "0.99"),
        ] {
            assert_eq!(
                Money::from_cents(cents),
                Money::parse_usd(decimal).expect("parses"),
                "{cents} cents should equal {decimal}"
            );
        }
    }

    #[test]
    fn serializes_as_object() {
        let m = Money::usd("9.99");
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"amount":"9.99","currency":"USD"}"#
        );
        assert_eq!(m.to_string(), "$9.99");
    }
}
