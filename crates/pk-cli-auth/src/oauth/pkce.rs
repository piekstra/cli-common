//! PKCE (RFC 7636) and the random values a sign-in carries.

use std::fmt;

use pk_cli_core::CliError;
use pk_cli_secrets::Secret;
use sha2::{Digest, Sha256};

/// The only `code_challenge_method` this module sends. `plain` offers nothing
/// a code interceptor cannot read off the request, so it is never offered.
pub const S256: &str = "S256";

/// A PKCE verifier and its S256 challenge. The verifier stays in this process
/// until the code exchange; only the challenge travels in the authorization
/// request.
pub struct Pkce {
    verifier: Secret,
    challenge: String,
}

impl Pkce {
    /// A fresh verifier: 32 bytes from the operating system's random source,
    /// base64url without padding (43 characters, RFC 7636 §4.1).
    pub fn generate() -> Result<Self, CliError> {
        Self::from_verifier(random_token(32)?)
    }

    /// A pair over a verifier the caller already has (a test vector, or a
    /// verifier parked between two processes). Refuses one RFC 7636 §4.1 does
    /// not allow: 43 to 128 characters of `A-Z a-z 0-9 - . _ ~`.
    pub fn from_verifier(verifier: impl Into<String>) -> Result<Self, CliError> {
        let verifier = verifier.into();
        if !(43..=128).contains(&verifier.len()) || !verifier.bytes().all(is_unreserved) {
            return Err(CliError::Usage(
                "a PKCE verifier is 43 to 128 characters of A-Z a-z 0-9 - . _ ~ (RFC 7636 §4.1)"
                    .into(),
            ));
        }
        let challenge = s256_challenge(&verifier);
        Ok(Pkce {
            verifier: Secret::new(verifier),
            challenge,
        })
    }

    /// The verifier, for the code exchange only.
    pub fn verifier(&self) -> &Secret {
        &self.verifier
    }

    /// `base64url(sha256(verifier))`, for the authorization request.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &self.verifier)
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// `base64url(sha256(verifier))`, unpadded: the S256 code challenge.
pub fn s256_challenge(verifier: &str) -> String {
    base64(&Sha256::digest(verifier.as_bytes()), Alphabet::Url)
}

/// `bytes` from the operating system's random source as base64url without
/// padding: a PKCE verifier, a `state`, a nonce.
pub fn random_token(bytes: usize) -> Result<String, CliError> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)
        .map_err(|e| CliError::Other(format!("the system random source failed: {e}")))?;
    let token = base64(&buf, Alphabet::Url);
    buf.fill(0);
    Ok(token)
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

#[derive(Clone, Copy)]
pub(crate) enum Alphabet {
    /// RFC 4648 §5, unpadded.
    Url,
    /// RFC 4648 §4, padded: HTTP Basic credentials.
    Standard,
}

/// Base64 in either alphabet. Local because this and `token`'s claims decoder
/// are the only base64 the crate needs, and neither is worth a dependency.
pub(crate) fn base64(bytes: &[u8], alphabet: Alphabet) -> String {
    let table: &[u8; 64] = match alphabet {
        Alphabet::Url => b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
        Alphabet::Standard => b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
    };
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]),
            2 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8),
            _ => u32::from(chunk[0]) << 16,
        };
        let symbols = chunk.len() + 1;
        for i in 0..4 {
            if i < symbols {
                out.push(table[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else if matches!(alphabet, Alphabet::Standard) {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B.
    #[test]
    fn the_challenge_matches_the_rfc_vector() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap();
        assert_eq!(
            pkce.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_generated_verifier_is_valid_and_fresh() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_eq!(a.verifier().expose().len(), 43);
        assert!(a.verifier().expose().bytes().all(is_unreserved));
        assert_eq!(a.challenge(), s256_challenge(a.verifier().expose()));
        assert_ne!(a.verifier().expose(), b.verifier().expose());
    }

    #[test]
    fn a_verifier_outside_the_rfc_is_refused() {
        for bad in ["short", &"a".repeat(129), &format!("{}+", "a".repeat(43))] {
            assert_eq!(
                Pkce::from_verifier(bad).unwrap_err().exit_code(),
                2,
                "{bad}"
            );
        }
    }

    #[test]
    fn debug_shows_no_verifier() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap();
        assert!(!format!("{pkce:?}").contains("dBjftJeZ"));
    }

    /// RFC 4648 §10.
    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (raw, standard, url) in [
            ("", "", ""),
            ("f", "Zg==", "Zg"),
            ("fo", "Zm8=", "Zm8"),
            ("foo", "Zm9v", "Zm9v"),
            ("foob", "Zm9vYg==", "Zm9vYg"),
            ("fooba", "Zm9vYmE=", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes(), Alphabet::Standard), standard);
            assert_eq!(base64(raw.as_bytes(), Alphabet::Url), url);
        }
        assert_eq!(base64(&[0xfb, 0xff], Alphabet::Url), "-_8");
        assert_eq!(base64(&[0xfb, 0xff], Alphabet::Standard), "+/8=");
    }
}
