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
    fn parse_iso_validates() {
        assert_eq!(parse_iso("2024-03-05").unwrap(), (2024, 3, 5));
        assert!(parse_iso("03-05-2024").is_err());
        assert!(parse_iso("2024-13-01").is_err());
        assert!(parse_iso("nope").is_err());
    }
}
