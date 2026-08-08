//! Reading the claims of a cached bearer token.
//!
//! Portals that hand out a JWT put the session's lifetime inside it, which
//! lets a CLI answer two questions without spending a request:
//!
//! - `auth status` can report `expires_at` (and whether the session is still
//!   usable) offline.
//! - An ordinary read can fail with a clear "run `<bin> auth login`" instead of
//!   a bare 401 from the provider.
//!
//! # This does not verify anything
//!
//! No signature check, no issuer check, no audience check. A CLI is the
//! *bearer* of this token, not the party validating it — it cannot hold the
//! signing key, and a token it minted itself would be worthless anyway. The
//! server remains the only authority on whether a token is good. Everything
//! here is a read of what the token claims about itself, used solely to
//! produce a better error message than the provider would.
//!
//! That is also why the safety rail points the way it does: a token this
//! module cannot parse is reported as **not** expired ([`is_expired`]), so an
//! unfamiliar token shape still reaches the server rather than locking a user
//! out of a session that was fine.
//!
//! # Skew is the caller's call
//!
//! [`is_expired`] takes the leeway in seconds rather than picking one. How
//! close to the edge is too close depends on how long the CLI's requests take
//! and how strict the provider is; [`DEFAULT_SKEW_SECS`] is a starting point,
//! not a house rule.

use serde_json::Value;

/// A reasonable starting leeway for [`is_expired`]: treat a token expiring
/// within a minute as already gone, so a command doesn't begin work that will
/// 401 partway through.
pub const DEFAULT_SKEW_SECS: u64 = 60;

/// The decoded claims of a JWT, or `None` if it isn't one.
///
/// Accepts both the URL-safe and standard base64 alphabets, with or without
/// padding — providers differ, and a claims segment is not worth failing over.
pub fn claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64_decode(payload)?).ok()
}

/// A numeric claim, e.g. `exp` or `iat`.
pub fn numeric_claim(token: &str, name: &str) -> Option<u64> {
    claims(token)?.get(name)?.as_u64()
}

/// The `exp` claim: when the token stops being valid, in Unix seconds.
pub fn expiry(token: &str) -> Option<u64> {
    numeric_claim(token, "exp")
}

/// The `exp` claim formatted for `auth-status/v1`'s `expires_at` field.
pub fn expires_at(token: &str) -> Option<String> {
    expiry(token).map(|exp| pk_cli_core::dates::fmt_rfc3339(exp as i64))
}

/// Whether the token is past its `exp`, allowing `skew_secs` of leeway.
///
/// A token with no readable `exp` is reported as **not** expired: this module
/// verifies nothing, so the only safe direction to be wrong in is the one that
/// still lets the server answer. See the module docs.
pub fn is_expired(token: &str, now_unix: u64, skew_secs: u64) -> bool {
    match expiry(token) {
        Some(exp) => exp.saturating_sub(skew_secs) <= now_unix,
        None => false,
    }
}

/// Base64 decoder covering both alphabets, padding optional. Deliberately
/// local: a claims segment is the only thing this crate ever decodes, and it
/// isn't worth a dependency.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const fn value(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'-' | b'+' => Some(62),
            b'_' | b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for b in input.bytes() {
        if b == b'=' {
            break;
        }
        acc = (acc << 6) | value(b)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build an unsigned JWT carrying `claims`. The signature is never looked
    /// at, so its content is irrelevant.
    fn jwt(claims: &Value) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let bytes = serde_json::to_vec(claims).unwrap();
        let mut encoded = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..chunk.len() + 1 {
                encoded.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            }
        }
        format!("header.{encoded}.signature")
    }

    #[test]
    fn reads_claims() {
        let t = jwt(&json!({ "sub": "user-1", "exp": 1_800_000_000u64, "iat": 1_700_000_000u64 }));
        assert_eq!(expiry(&t), Some(1_800_000_000));
        assert_eq!(numeric_claim(&t, "iat"), Some(1_700_000_000));
        assert_eq!(claims(&t).unwrap()["sub"], "user-1");
        assert_eq!(numeric_claim(&t, "nope"), None);
    }

    #[test]
    fn expiry_respects_skew() {
        let t = jwt(&json!({ "exp": 1_000_000u64 }));
        assert!(is_expired(&t, 1_000_001, 0));
        assert!(!is_expired(&t, 999_999, 0));
        // With leeway, a token about to expire counts as gone.
        assert!(is_expired(&t, 999_950, DEFAULT_SKEW_SECS));
        assert!(!is_expired(&t, 999_000, DEFAULT_SKEW_SECS));
    }

    /// The safety rail: anything unreadable defers to the server.
    #[test]
    fn unreadable_tokens_are_never_reported_expired() {
        for bad in [
            "",
            "not-a-jwt",
            "a.b",
            "a.!!!!.c",
            "a.e30.c",                 // valid base64, but `{}` has no exp
            "header..signature",       // empty payload
            "header.bm90LWpzb24=.sig", // valid base64, not JSON
        ] {
            assert!(
                !is_expired(bad, 2_000_000_000, DEFAULT_SKEW_SECS),
                "{bad:?} must defer to the server"
            );
            assert_eq!(expiry(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn accepts_both_base64_alphabets_and_optional_padding() {
        assert_eq!(base64_decode("eyJhIjoxfQ").unwrap(), br#"{"a":1}"#);
        assert_eq!(base64_decode("eyJhIjoxfQ==").unwrap(), br#"{"a":1}"#);
        // `-`/`_` (url-safe) and `+`/`/` (standard) decode to the same bytes.
        assert_eq!(
            base64_decode("--__").unwrap(),
            base64_decode("++//").unwrap()
        );
        assert!(base64_decode("****").is_none());
    }

    #[test]
    fn expires_at_is_rfc3339_for_auth_status() {
        let t = jwt(&json!({ "exp": 1_700_000_000u64 }));
        assert_eq!(expires_at(&t).as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(expires_at("not-a-jwt"), None);
    }
}
