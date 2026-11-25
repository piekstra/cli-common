//! The running binary's code signature, as macOS sees it: what decides
//! whether a keychain item's lists already let this build in.

use serde::{Deserialize, Serialize};

/// How a binary is signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SigningKind {
    /// By a certificate.
    Certificate,
    /// Ad hoc, as `cargo build` leaves it: a new identity every build.
    AdHoc,
    /// Not at all.
    Unsigned,
}

/// The running binary's code signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Signing {
    pub kind: SigningKind,
    /// The signing certificate's name, such as `pk-cli-codesign`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// The identifier in the signature, such as the binary's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// The hash of this exact build, which a keychain item's partition list
    /// holds (`cdhash:`) for a binary with no team ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdhash: Option<String>,
}

impl std::fmt::Display for Signing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.kind, &self.identity) {
            (_, Some(identity)) => f.write_str(identity)?,
            (SigningKind::AdHoc, None) => f.write_str("ad hoc")?,
            (SigningKind::Unsigned, None) => f.write_str("unsigned")?,
            (SigningKind::Certificate, None) => f.write_str("a certificate")?,
        }
        let details: Vec<String> = [
            ("identifier", &self.identifier),
            ("team", &self.team_id),
            ("cdhash", &self.cdhash),
        ]
        .iter()
        .filter_map(|(name, value)| value.as_ref().map(|v| format!("{name} {v}")))
        .collect();
        if details.is_empty() {
            return Ok(());
        }
        write!(f, " ({})", details.join(", "))
    }
}

/// The running binary's signature. `None` off macOS, or when macOS does not
/// say. It reads the binary's own signature, no keychain item.
pub fn running_binary() -> Option<Signing> {
    #[cfg(target_os = "macos")]
    {
        macos::running_binary()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use security_framework::certificate::SecCertificate;
    use security_framework::os::macos::code_signing::{Flags, SecCode};

    use super::{Signing, SigningKind};

    /// `kSecCSSigningInformation`: the certificates too.
    const SIGNING_INFORMATION: u32 = 1 << 1;
    /// `kSecCodeSignatureAdhoc`.
    const ADHOC: i64 = 1 << 1;

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecCodeInfoIdentifier: CFStringRef;
        static kSecCodeInfoCertificates: CFStringRef;
        static kSecCodeInfoTeamIdentifier: CFStringRef;
        static kSecCodeInfoUnique: CFStringRef;
        static kSecCodeInfoFlags: CFStringRef;

        /// Takes a running code object where it names a static one
        /// (Security/SecCode.h).
        fn SecCodeCopySigningInformation(
            code: CFTypeRef,
            flags: u32,
            information: *mut CFDictionaryRef,
        ) -> i32;
    }

    pub(super) fn running_binary() -> Option<Signing> {
        let code = SecCode::for_self(Flags::NONE).ok()?;
        let mut raw: CFDictionaryRef = std::ptr::null();
        // SAFETY: `code` outlives the call, and a status of 0 means `raw`
        // holds a dictionary this caller owns.
        let info: CFDictionary<CFString, CFType> = unsafe {
            let status =
                SecCodeCopySigningInformation(code.as_CFTypeRef(), SIGNING_INFORMATION, &mut raw);
            if status != 0 || raw.is_null() {
                return None;
            }
            CFDictionary::wrap_under_create_rule(raw)
        };
        // SAFETY: each key is a constant of the framework linked above.
        let value = |key: CFStringRef| {
            let key = unsafe { CFString::wrap_under_get_rule(key) };
            info.find(&key).map(|found| found.clone())
        };
        let text = |key: CFStringRef| {
            value(key)
                .and_then(|v| v.downcast::<CFString>())
                .map(|v| v.to_string())
        };
        // SAFETY: as above, reading the constants.
        let (identifier, team_id, unique, flags, certificates) = unsafe {
            (
                text(kSecCodeInfoIdentifier),
                text(kSecCodeInfoTeamIdentifier),
                value(kSecCodeInfoUnique),
                value(kSecCodeInfoFlags),
                value(kSecCodeInfoCertificates),
            )
        };
        let Some(identifier) = identifier else {
            return Some(Signing {
                kind: SigningKind::Unsigned,
                identity: None,
                identifier: None,
                team_id: None,
                cdhash: None,
            });
        };
        let adhoc = flags
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|v| v.to_i64())
            .is_some_and(|flags| flags & ADHOC != 0);
        let cdhash = unique
            .and_then(|v| v.downcast::<CFData>())
            .map(|v| v.bytes().iter().map(|b| format!("{b:02x}")).collect());
        let identity = certificates.and_then(|v| leaf_name(&v));
        Some(Signing {
            kind: if adhoc {
                SigningKind::AdHoc
            } else {
                SigningKind::Certificate
            },
            identity,
            identifier: Some(identifier),
            team_id,
            cdhash,
        })
    }

    /// The name of the first certificate of a chain, the signer's.
    fn leaf_name(chain: &CFType) -> Option<String> {
        if !chain.instance_of::<CFArray>() {
            return None;
        }
        // SAFETY: the type was checked, and Security documents the array's
        // values as SecCertificateRef.
        let chain: CFArray<SecCertificate> =
            unsafe { CFArray::wrap_under_get_rule(chain.as_CFTypeRef() as CFArrayRef) };
        chain.get(0).map(|leaf| leaf.subject_summary())
    }

    /// The test binary, which `cargo test` leaves ad-hoc signed by the
    /// linker. No keychain is read.
    #[cfg(all(test, target_arch = "aarch64"))]
    #[test]
    fn the_test_binary_is_signed_ad_hoc() {
        let signing = running_binary().expect("macOS describes a running binary");
        assert_eq!(signing.kind, SigningKind::AdHoc, "{signing:?}");
        assert_eq!(signing.identity, None);
        assert!(signing.identifier.is_some());
        let cdhash = signing.cdhash.expect("a signed binary has a cdhash");
        assert_eq!(cdhash.len(), 40, "{cdhash}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signing(kind: SigningKind) -> Signing {
        Signing {
            kind,
            identity: None,
            identifier: None,
            team_id: None,
            cdhash: None,
        }
    }

    #[test]
    fn a_signature_reads_as_one_line() {
        let mut s = signing(SigningKind::Certificate);
        s.identity = Some("example-codesign".into());
        s.identifier = Some("svc".into());
        s.cdhash = Some("ab12".into());
        assert_eq!(
            s.to_string(),
            "example-codesign (identifier svc, cdhash ab12)"
        );
        s.team_id = Some("TEAM123456".into());
        s.identifier = None;
        assert_eq!(
            s.to_string(),
            "example-codesign (team TEAM123456, cdhash ab12)"
        );
        let mut adhoc = signing(SigningKind::AdHoc);
        adhoc.identifier = Some("svc-0f".into());
        assert_eq!(adhoc.to_string(), "ad hoc (identifier svc-0f)");
        assert_eq!(signing(SigningKind::Unsigned).to_string(), "unsigned");
    }

    #[test]
    fn what_a_signature_lacks_is_left_out() {
        let v = serde_json::to_value(signing(SigningKind::AdHoc)).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "ad-hoc" }));
        let back: Signing = serde_json::from_value(v).unwrap();
        assert_eq!(back, signing(SigningKind::AdHoc));
    }
}
