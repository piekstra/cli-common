//! The verification code itself: validation, the `--code` flag, and pulling a
//! code out of an email's text.

use std::fmt;

use pk_cli_core::CliError;

/// Shortest and longest code [`OtpCode::parse`] accepts. Wide on purpose:
/// providers send 4- to 10-character codes, and the provider is the authority
/// on whether one is right. This only rejects input that cannot be a code at
/// all, so a typo never costs a redeem attempt.
const MIN_LEN: usize = 4;
const MAX_LEN: usize = 12;

/// A validated one-time code.
///
/// Spaces and hyphens are dropped (`123 456` and `123-456` are how codes are
/// printed and read aloud). What remains must be 4–12 ASCII letters or
/// digits. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct OtpCode(String);

impl OtpCode {
    /// Validate a code typed, piped or read from a mailbox. A malformed code
    /// is a usage error (exit 2), decided before any keychain or network work.
    pub fn parse(raw: &str) -> Result<Self, CliError> {
        let cleaned: String = raw
            .trim()
            .chars()
            .filter(|c| !matches!(c, ' ' | '-'))
            .collect();
        if cleaned.is_empty() {
            return Err(CliError::Usage("the verification code was empty".into()));
        }
        if !(MIN_LEN..=MAX_LEN).contains(&cleaned.len())
            || !cleaned.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Err(CliError::Usage(format!(
                "that does not look like a verification code \
                 (expected {MIN_LEN}–{MAX_LEN} letters or digits)"
            )));
        }
        Ok(OtpCode(cleaned))
    }

    /// The code, for the redeem request only. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OtpCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OtpCode(***)")
    }
}

/// The `--code` flag, flattenable into a CLI's `auth login` args.
///
/// `--code <CODE>` is the family's resume spelling. A one-time code is the
/// one credential-shaped value the family accepts on argv: it is spent on
/// first use, expires in minutes, and is worthless without the parked session,
/// which never leaves the keychain. `--code -` reads it from stdin instead,
/// for a caller that wants it out of `ps` and shell history too. Passwords and
/// sessions never go on argv either way.
#[derive(clap::Args, Default, Clone)]
pub struct CodeArgs {
    /// Verification code for a login waiting on one; `-` reads it from stdin.
    #[arg(long, value_name = "CODE")]
    pub code: Option<String>,
}

impl fmt::Debug for CodeArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeArgs")
            .field("code", &self.code.as_ref().map(|_| "***"))
            .finish()
    }
}

impl CodeArgs {
    /// The validated code, if one was given. Call this first in `auth login`:
    /// a malformed code fails here, before the keychain is read.
    pub fn resolve(&self) -> Result<Option<OtpCode>, CliError> {
        match self.code.as_deref() {
            None => Ok(None),
            Some("-") => {
                let raw = pk_cli_secrets::read_stdin()?;
                OtpCode::parse(raw.expose()).map(Some)
            }
            Some(raw) => OtpCode::parse(raw).map(Some),
        }
    }
}

/// What a provider's codes look like, for reading one out of an email.
///
/// Digits only: every email-OTP provider the family logs in to sends a numeric
/// code, and widening this to letters would match ordinary words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeShape {
    min_digits: usize,
    max_digits: usize,
}

impl CodeShape {
    /// Codes of `min..=max` digits. Panics if `min` is 0 or `min > max`; the
    /// shape is a constant in the calling CLI, so this is a programming error.
    pub fn digits(min: usize, max: usize) -> Self {
        assert!(min > 0 && min <= max, "CodeShape::digits({min}, {max})");
        CodeShape {
            min_digits: min,
            max_digits: max,
        }
    }
}

impl Default for CodeShape {
    /// Six digits, the common case.
    fn default() -> Self {
        CodeShape::digits(6, 6)
    }
}

/// Words that introduce a code in a provider's email ("your verification code
/// is 123456"). Matched case-insensitively.
const KEYWORDS: &[&str] = &["code", "passcode", "otp", "pin", "verification"];

/// How far after a keyword a code may sit and still count as introduced by it.
const KEYWORD_REACH: usize = 40;

/// Pull a code out of an email's subject, snippet or body.
///
/// A digit run of the right length counts only when it stands alone (not part
/// of a longer number or a word like `A123456`). The first one introduced by a
/// keyword wins; failing that, a lone candidate is taken. Two or more
/// unintroduced candidates return `None`: guessing would spend a redeem
/// attempt on, say, a reference number, and attempts are rate-limited. The
/// caller then falls back to the prompt or the parked `--code` resume.
pub fn extract_code(text: &str, shape: CodeShape) -> Option<OtpCode> {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let mut candidates: Vec<(usize, &str)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let len = i - start;
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = i == bytes.len() || !bytes[i].is_ascii_alphanumeric();
        if before_ok
            && after_ok
            && (shape.min_digits..=shape.max_digits).contains(&len)
            // `1.234567` or `$123456.00` is an amount, not a code.
            && !(start > 0 && bytes[start - 1] == b'.' && start > 1 && bytes[start - 2].is_ascii_digit())
            && !(i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit())
        {
            candidates.push((start, &text[start..i]));
        }
    }

    let introduced = candidates.iter().find(|(start, _)| {
        let from = start.saturating_sub(KEYWORD_REACH);
        // `from` may fall inside a multi-byte character; widen to a boundary.
        let from = (0..=from)
            .rev()
            .find(|&b| lower.is_char_boundary(b))
            .unwrap_or(0);
        let window = &lower[from..*start];
        KEYWORDS.iter().any(|k| window.contains(k))
    });
    let pick = match introduced {
        Some((_, c)) => Some(*c),
        None if candidates.len() == 1 => Some(candidates[0].1),
        None => None,
    }?;
    OtpCode::parse(pick).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_cleaned_and_validated() {
        assert_eq!(OtpCode::parse(" 123 456\n").unwrap().expose(), "123456");
        assert_eq!(OtpCode::parse("123-456").unwrap().expose(), "123456");
        assert_eq!(OtpCode::parse("AB12CD").unwrap().expose(), "AB12CD");
        for bad in ["", "   ", "12", "1234567890123", "12345!", "１２３４５６"] {
            let err = OtpCode::parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn a_code_never_debug_prints() {
        let code = OtpCode::parse("123456").unwrap();
        assert_eq!(format!("{code:?}"), "OtpCode(***)");
        let args = CodeArgs {
            code: Some("123456".into()),
        };
        assert!(!format!("{args:?}").contains("123456"));
    }

    #[test]
    fn code_args_validate_without_touching_anything_else() {
        assert_eq!(CodeArgs::default().resolve().unwrap(), None);
        let ok = CodeArgs {
            code: Some("654321".into()),
        };
        assert_eq!(ok.resolve().unwrap().unwrap().expose(), "654321");
        let bad = CodeArgs {
            code: Some("nope!".into()),
        };
        assert_eq!(bad.resolve().unwrap_err().exit_code(), 2);
    }

    #[test]
    fn extraction_prefers_the_code_a_keyword_introduces() {
        let text = "Order 482913 update. Your verification code is 731904. It expires soon.";
        assert_eq!(
            extract_code(text, CodeShape::default()).unwrap().expose(),
            "731904"
        );
    }

    #[test]
    fn a_lone_candidate_is_taken_without_a_keyword() {
        assert_eq!(
            extract_code("Use 553201 to sign in", CodeShape::default())
                .unwrap()
                .expose(),
            "553201"
        );
    }

    #[test]
    fn ambiguous_text_yields_no_code_rather_than_a_guess() {
        assert_eq!(
            extract_code("Ref 123456 and ticket 654321", CodeShape::default()),
            None
        );
    }

    #[test]
    fn digits_inside_longer_numbers_words_and_amounts_are_ignored() {
        let shape = CodeShape::default();
        assert_eq!(extract_code("call 5551234567", shape), None);
        assert_eq!(extract_code("id A123456B", shape), None);
        assert_eq!(extract_code("balance 123456.00", shape), None);
        assert_eq!(extract_code("rate 0.123456", shape), None);
        assert_eq!(extract_code("no digits here", shape), None);
    }

    #[test]
    fn the_shape_bounds_the_length() {
        let text = "Your code: 4821";
        assert_eq!(extract_code(text, CodeShape::default()), None);
        assert_eq!(
            extract_code(text, CodeShape::digits(4, 8))
                .unwrap()
                .expose(),
            "4821"
        );
    }

    #[test]
    fn multibyte_text_before_a_code_does_not_panic() {
        let text = "ñññññññññññññññññññññññññññññññññññññññññññ code — 246810";
        assert_eq!(
            extract_code(text, CodeShape::default()).unwrap().expose(),
            "246810"
        );
    }
}
