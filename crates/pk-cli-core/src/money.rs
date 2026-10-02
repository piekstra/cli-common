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

    /// Human display with thousands separators and the sign ahead of the
    /// currency symbol: `$1,234.56`, `-$1,234.56`, `1,234.56 EUR`.
    ///
    /// For text reports, where `$438000.00` is hard to scan. `Display` keeps
    /// its ungrouped form (`$1234.56`) so existing output and snapshots are
    /// unchanged, and JSON is unaffected either way: the wire amount stays a
    /// plain decimal string (SPEC v1 §1.4).
    ///
    /// ```
    /// # use pk_cli_core::Money;
    /// assert_eq!(Money::usd("438000.00").grouped(), "$438,000.00");
    /// assert_eq!(Money::from_cents(-123_456).grouped(), "-$1,234.56");
    /// ```
    pub fn grouped(&self) -> String {
        let (negative, digits) = split_sign(self.amount.trim());
        let sign = if negative { "-" } else { "" };
        self.render(sign, &group_unsigned(digits))
    }

    /// The one place a currency's symbol and position are decided, shared by
    /// `Display` and [`Money::grouped`].
    fn render(&self, sign: &str, body: &str) -> String {
        if self.currency == "USD" {
            format!("{sign}${body}")
        } else {
            format!("{sign}{body} {}", self.currency)
        }
    }
}

/// Insert thousands separators into a decimal string: `-1234567.5` becomes
/// `-1,234,567.5`.
///
/// For amounts that are not a [`Money`] — a total summed from cents, a value
/// headed for a column that already names its currency. Only the integer part
/// is grouped; the fraction is kept exactly as given. A value that is not a
/// plain decimal (`n/a`, `1e6`, an already-grouped `1,234`) comes back
/// unchanged rather than half-formatted.
///
/// ```
/// # use pk_cli_core::money::group_thousands;
/// assert_eq!(group_thousands("1234567.89"), "1,234,567.89");
/// assert_eq!(group_thousands("-1000"), "-1,000");
/// assert_eq!(group_thousands("n/a"), "n/a");
/// ```
pub fn group_thousands(amount: &str) -> String {
    let (negative, digits) = split_sign(amount.trim());
    if !is_plain_decimal(digits) {
        return amount.to_string();
    }
    let sign = if negative { "-" } else { "" };
    format!("{sign}{}", group_unsigned(digits))
}

/// Strip one leading `-` or `+`, reporting whether the value was negative.
fn split_sign(s: &str) -> (bool, &str) {
    match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    }
}

/// ASCII digits with at most one point and at least one digit: `123`,
/// `123.4`, `.5`, `0.00`.
fn is_plain_decimal(s: &str) -> bool {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    digits(int) && digits(frac) && !(int.is_empty() && frac.is_empty())
}

/// Group the integer part of an unsigned amount. A value that is not a plain
/// decimal is returned as given, so [`Money::grouped`] degrades to the
/// ungrouped amount instead of mangling a provider string it was handed
/// verbatim.
fn group_unsigned(s: &str) -> String {
    if !is_plain_decimal(s) {
        return s.to_string();
    }
    let (int, frac) = match s.find('.') {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let mut out = String::with_capacity(s.len() + int.len() / 3);
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.push_str(frac);
    out
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render("", &self.amount))
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

    #[test]
    fn grouped_inserts_thousands_separators() {
        for (amount, want) in [
            ("0.00", "$0.00"),
            ("9.99", "$9.99"),
            ("999.99", "$999.99"),
            ("1000.00", "$1,000.00"),
            ("438000.00", "$438,000.00"),
            ("1234567.89", "$1,234,567.89"),
            ("100000000", "$100,000,000"),
        ] {
            assert_eq!(Money::usd(amount).grouped(), want, "{amount}");
        }
    }

    /// The sign leads the symbol (`-$5.00`), where `Display` gives `$-5.00`.
    #[test]
    fn grouped_puts_the_sign_before_the_currency() {
        assert_eq!(Money::from_cents(-123_456).grouped(), "-$1,234.56");
        assert_eq!(Money::from_cents(-5).grouped(), "-$0.05");
        assert_eq!(Money::usd("+1000").grouped(), "$1,000");
        let eur = Money {
            amount: "-1234567.50".into(),
            currency: "EUR".into(),
        };
        assert_eq!(eur.grouped(), "-1,234,567.50 EUR");
    }

    /// Grouping is opt-in: `Display` and the wire shape keep their form.
    #[test]
    fn grouping_does_not_change_display_or_json() {
        let m = Money::usd("438000.00");
        assert_eq!(m.to_string(), "$438000.00");
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"amount":"438000.00","currency":"USD"}"#
        );
    }

    #[test]
    fn grouped_leaves_non_decimal_amounts_alone() {
        assert_eq!(Money::usd("n/a").grouped(), "$n/a");
        assert_eq!(Money::usd("1,234.00").grouped(), "$1,234.00");
        assert_eq!(Money::usd("").grouped(), "$");
    }

    #[test]
    fn group_thousands_handles_bare_decimals() {
        assert_eq!(group_thousands("1234567.891"), "1,234,567.891");
        assert_eq!(group_thousands("-1000"), "-1,000");
        assert_eq!(group_thousands("+1000"), "1,000");
        assert_eq!(group_thousands("100"), "100");
        assert_eq!(group_thousands("-100"), "-100");
        assert_eq!(group_thousands(".5"), ".5");
        assert_eq!(group_thousands(" 2500.00 "), "2,500.00");
        // Not a plain decimal: returned exactly as given.
        for raw in ["", "-", ".", "n/a", "1e6", "1,234", "12.3.4", "--5"] {
            assert_eq!(group_thousands(raw), raw, "{raw:?}");
        }
    }

    /// Every cents value round-trips: grouping then stripping the commas gives
    /// back the `from_cents` amount, so the helper only ever inserts commas.
    #[test]
    fn grouping_only_inserts_commas() {
        for cents in [0, 1, 99, 100, 99_999, 100_000, 123_456_789, -1, -100_000] {
            let m = Money::from_cents(cents);
            let stripped: String = m
                .grouped()
                .chars()
                .filter(|c| *c != ',' && *c != '$')
                .collect();
            assert_eq!(stripped, m.amount, "{cents}");
        }
    }
}
