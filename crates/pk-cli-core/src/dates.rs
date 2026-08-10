//! Minimal date helpers so family CLIs don't pull in a calendar crate. SPEC
//! v1 accepts ISO `YYYY-MM-DD` on every flag; provider formats (`MM-DD-YYYY`
//! and friends) are an internal conversion concern handled here.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::CliError;

/// A civil `(year, month, day)` date.
pub type Civil = (i64, u32, u32);

/// Days since the Unix epoch in UTC.
fn epoch_days() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    secs.div_euclid(86_400)
}

/// Convert days-since-epoch to a civil date.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> Civil {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn today() -> Civil {
    civil_from_days(epoch_days())
}

/// The civil date a Unix timestamp falls on, in UTC.
pub fn civil_from_unix(secs: i64) -> Civil {
    civil_from_days(secs.div_euclid(86_400))
}

/// A Unix timestamp as RFC 3339 UTC (`2026-08-07T21:04:05Z`).
///
/// The format `auth-status/v1` specifies for `expires_at`, so a CLI whose
/// session carries a known lifetime — a bearer token with an `exp` claim, say
/// — can report it without pulling in a calendar crate.
pub fn fmt_rfc3339(secs: i64) -> String {
    let (y, m, d) = civil_from_unix(secs);
    let time = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

pub fn yesterday() -> Civil {
    civil_from_days(epoch_days() - 1)
}

/// ISO `YYYY-MM-DD` — the wire and flag format (SPEC v1 §1.3/§1.4).
pub fn fmt_iso((y, m, d): Civil) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

/// `MM-DD-YYYY` — legacy provider format (e.g. FPL endpoints).
pub fn fmt_mm_dd_yyyy((y, m, d): Civil) -> String {
    format!("{m:02}-{d:02}-{y:04}")
}

/// `MM/DD/YYYY` — legacy provider format (e.g. Xfinity endpoints).
pub fn fmt_mm_slash_dd_yyyy((y, m, d): Civil) -> String {
    format!("{m:02}/{d:02}/{y:04}")
}

/// `.NET`'s `DateTime.MinValue` as a civil date — the sentinel ASP.NET back
/// ends serialize for "no value".
pub const DOTNET_MIN: Civil = (1, 1, 1);

/// Parse an ASP.NET / ServiceStack `/Date(<millis>[±hhmm])/` timestamp.
///
/// Returns `None` only when the value isn't that format. `DateTime.MinValue`
/// parses successfully, to [`DOTNET_MIN`]; whether that counts as "absent" is
/// left to the caller, because back ends also use their own placeholder dates
/// (1900-01-01 is common) that only the caller knows about.
///
/// The trailing offset is deliberately ignored: it is the *server's* timezone
/// rendering of an instant the milliseconds already express in UTC, so
/// applying it shifts evening timestamps back a day.
pub fn parse_dotnet(raw: &str) -> Option<Civil> {
    let inner = raw.trim().strip_prefix("/Date(")?.strip_suffix(")/")?;
    // Find a trailing ±hhmm offset without tripping on the leading sign of a
    // negative epoch (any date before 1970, which includes the sentinel).
    // `get` keeps an empty or non-ASCII payload from panicking on the slice.
    let millis = match inner.get(1..)?.find(['+', '-']) {
        Some(i) => &inner[..i + 1],
        None => inner,
    };
    let millis: i64 = millis.parse().ok()?;
    Some(civil_from_days(millis.div_euclid(86_400_000)))
}

/// Whether a civil date is .NET's `DateTime.MinValue` sentinel.
pub fn is_dotnet_min(c: Civil) -> bool {
    c == DOTNET_MIN
}

/// Parse a `MM/DD/YYYY` provider value — the inverse of
/// [`fmt_mm_slash_dd_yyyy`], for reading values back out of rendered pages.
///
/// Lenient about single-digit month and day, which templating engines emit
/// inconsistently; strict about a 4-digit year, so a two-digit-year value is
/// rejected rather than silently misread as year 26.
pub fn parse_mm_slash_dd_yyyy(s: &str) -> Option<Civil> {
    let parts: Vec<&str> = s.trim().split('/').collect();
    if parts.len() != 3 || parts[2].len() != 4 {
        return None;
    }
    if !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let m: u32 = parts[0].parse().ok()?;
    let d: u32 = parts[1].parse().ok()?;
    let y: i64 = parts[2].parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// Parse an ISO `YYYY-MM-DD` flag value, with basic range validation.
pub fn parse_iso(s: &str) -> Result<Civil, CliError> {
    let bad = || CliError::Usage(format!("expected an ISO date (YYYY-MM-DD), got `{s}`"));
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    let y: i64 = parts[0].parse().map_err(|_| bad())?;
    let m: u32 = parts[1].parse().map_err(|_| bad())?;
    let d: u32 = parts[2].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || parts[0].len() != 4 {
        return Err(bad());
    }
    Ok((y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_iso((2024, 3, 5)), "2024-03-05");
        assert_eq!(fmt_mm_dd_yyyy((2024, 3, 5)), "03-05-2024");
        assert_eq!(fmt_mm_slash_dd_yyyy((2024, 3, 5)), "03/05/2024");
    }

    #[test]
    fn rfc3339_round_numbers() {
        assert_eq!(fmt_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(fmt_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // Last second of a day, then the first of the next.
        assert_eq!(fmt_rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(fmt_rfc3339(86_400), "1970-01-02T00:00:00Z");
    }

    /// Pre-epoch timestamps must not wrap into a negative clock time —
    /// `rem_euclid` rather than `%` is what keeps this right.
    #[test]
    fn rfc3339_handles_pre_epoch() {
        assert_eq!(fmt_rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(civil_from_unix(-1), (1969, 12, 31));
    }

    #[test]
    fn parse_iso_validates() {
        assert_eq!(parse_iso("2024-03-05").unwrap(), (2024, 3, 5));
        assert!(parse_iso("03-05-2024").is_err());
        assert!(parse_iso("2024-13-01").is_err());
        assert!(parse_iso("nope").is_err());
    }

    #[test]
    fn dotnet_timestamps_parse() {
        assert_eq!(
            parse_dotnet("/Date(1785567600000-0700)/"),
            Some((2026, 8, 1))
        );
        assert_eq!(parse_dotnet("/Date(0+0000)/"), Some((1970, 1, 1)));
        // The offset is optional.
        assert_eq!(parse_dotnet("/Date(0)/"), Some((1970, 1, 1)));
    }

    /// The offset must not be applied — it re-expresses an instant the millis
    /// already carry, so honouring it would shift evening dates back a day.
    #[test]
    fn dotnet_offset_does_not_shift_the_date() {
        // 2026-08-01 07:00 UTC, rendered by a UTC-7 server as the prior evening.
        let utc_evening = "/Date(1785567600000-0700)/";
        assert_eq!(parse_dotnet(utc_evening), Some((2026, 8, 1)));
        // Same instant, a different server timezone: same civil date out.
        assert_eq!(
            parse_dotnet("/Date(1785567600000+0530)/"),
            Some((2026, 8, 1))
        );
    }

    #[test]
    fn dotnet_min_value_is_recognized_not_hidden() {
        // Parsing succeeds; classifying it as "absent" is the caller's call.
        let min = parse_dotnet("/Date(-62135596800000-0800)/").expect("parses");
        assert_eq!(min, DOTNET_MIN);
        assert!(is_dotnet_min(min));
        assert!(!is_dotnet_min((2026, 8, 1)));
    }

    #[test]
    fn malformed_dotnet_values_are_none() {
        for bad in [
            "",
            "/Date()/",
            "1785567600000",
            "/Date(abc)/",
            "/Date(",
            "Date(0)/",
        ] {
            assert_eq!(parse_dotnet(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn mm_slash_dd_yyyy_round_trips() {
        assert_eq!(parse_mm_slash_dd_yyyy("08/01/2026"), Some((2026, 8, 1)));
        // Templating engines emit single digits inconsistently.
        assert_eq!(parse_mm_slash_dd_yyyy("8/1/2026"), Some((2026, 8, 1)));
        assert_eq!(
            fmt_mm_slash_dd_yyyy(parse_mm_slash_dd_yyyy("08/01/2026").unwrap()),
            "08/01/2026"
        );
    }

    #[test]
    fn mm_slash_dd_yyyy_rejects_junk_and_short_years() {
        // A 2-digit year must not be read as year 26.
        for bad in [
            "08/01/26",
            "",
            "n/a",
            "2026-08-01",
            "13/01/2026",
            "08/32/2026",
        ] {
            assert_eq!(parse_mm_slash_dd_yyyy(bad), None, "{bad} should not parse");
        }
    }
}
