//! Which pages a capture may read from.
//!
//! A capture reads a live session, so the page it reads must be the
//! provider's own. Matching is on the parsed host of an `https` URL on the
//! default port, compared label by label against an ASCII rule: never a
//! substring of the URL, so `evil.example/?x=example.com`,
//! `example.com.evil.example`, `notexample.com` and
//! `example.com@evil.example` all fail. A look-alike host in another script
//! reaches Chrome's target list as punycode (`xn--…`), so it can never equal
//! an ASCII rule either.

use std::fmt;

use pk_cli_core::CliError;

/// A host a capture may be read from: exactly one host, or a registrable
/// domain together with every host under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    host: String,
    subdomains: bool,
}

impl HostRule {
    /// Exactly `host` (`dashboard.example.com`), and no other host:
    /// neither its parent nor its subdomains.
    pub fn exact(host: &str) -> Result<Self, CliError> {
        let host = normalize_host(host)
            .ok_or_else(|| CliError::Other(format!("invalid capture host `{host}`")))?;
        Ok(HostRule {
            host,
            subdomains: false,
        })
    }

    /// `domain` and every host under it (`example.com` matches
    /// `example.com`, `www.example.com` and `a.b.example.com`). Name the
    /// registrable domain the provider owns: a public suffix such as `com`
    /// is refused, though a multi-label suffix (`co.uk`) cannot be told
    /// apart without the Public Suffix List, so that is on the caller.
    pub fn domain(domain: &str) -> Result<Self, CliError> {
        let host = normalize_host(domain)
            .ok_or_else(|| CliError::Other(format!("invalid capture domain `{domain}`")))?;
        let labels: Vec<&str> = host.split('.').collect();
        let numeric_tld = labels
            .last()
            .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()));
        if labels.len() < 2 || numeric_tld {
            return Err(CliError::Other(format!(
                "capture domain `{domain}` is not a registrable domain"
            )));
        }
        Ok(HostRule {
            host,
            subdomains: true,
        })
    }

    /// The rule's host or domain, normalized (lowercase, no trailing dot).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether hosts under [`HostRule::host`] match too.
    pub fn includes_subdomains(&self) -> bool {
        self.subdomains
    }

    /// Whether `host` (as a browser reports it) is one this rule allows.
    pub fn matches_host(&self, host: &str) -> bool {
        let Some(host) = normalize_host(host) else {
            return false;
        };
        host == self.host
            || (self.subdomains
                && host.len() > self.host.len()
                && host.ends_with(&self.host)
                && host.as_bytes()[host.len() - self.host.len() - 1] == b'.')
    }

    /// Whether `url` is an `https` page on the default port whose host this
    /// rule allows.
    pub fn matches_url(&self, url: &str) -> bool {
        parse_url(url).is_some_and(|u| u.is_https_default_port() && self.matches_host(&u.host))
    }

    /// Whether the two rules could match a common host, so two captures
    /// using them would read, and close, each other's tabs.
    pub(crate) fn overlaps(&self, other: &HostRule) -> bool {
        self.matches_host(&other.host) || other.matches_host(&self.host)
    }
}

impl fmt::Display for HostRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.subdomains {
            write!(f, "{} (or a subdomain)", self.host)
        } else {
            f.write_str(&self.host)
        }
    }
}

/// Lowercase ASCII DNS name, one trailing dot dropped; `None` for anything
/// else: empty, over-long, non-ASCII, a bracketed IPv6 literal, a label that
/// is empty, over 63 bytes, starts or ends with `-`, or carries any byte but
/// `[a-z0-9-]` (so no `%`, `@`, `:`, `_`, space or `/`).
pub(crate) fn normalize_host(host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 || !host.is_ascii() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let labels_ok = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    labels_ok.then_some(host)
}

/// The parts of an absolute URL a capture decides on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UrlParts {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// The path without query or fragment; `/` when the URL has none.
    pub path: String,
}

impl UrlParts {
    pub fn is_https_default_port(&self) -> bool {
        self.scheme == "https" && matches!(self.port, None | Some(443))
    }
}

/// Parse `scheme://host[:port][/path][?query][#fragment]`. `None` for a URL
/// without an authority (`about:blank`, `data:`, `javascript:`), with
/// credentials in it (`https://a@b/`: legitimate app pages never carry
/// them, and refusing them leaves no doubt over which side of the `@` is
/// the host), with an IPv6 literal, or whose host [`normalize_host`]
/// refuses.
pub(crate) fn parse_url(url: &str) -> Option<UrlParts> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme_ok = scheme
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    if !scheme_ok {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') || authority.starts_with('[') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (host, Some(port.parse::<u16>().ok()?))
        }
        None => (authority, None),
    };
    let host = normalize_host(host)?;
    let path = if tail.starts_with('/') {
        tail.split(['?', '#']).next().unwrap_or("/")
    } else {
        "/"
    };
    Some(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        host,
        port,
        path: path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_normalize_or_are_refused() {
        let cases: &[(&str, Option<&str>)] = &[
            ("example.com", Some("example.com")),
            ("Example.COM", Some("example.com")),
            ("example.com.", Some("example.com")),
            ("a-b.example.com", Some("a-b.example.com")),
            ("xn--exmple-cua.com", Some("xn--exmple-cua.com")),
            ("127.0.0.1", Some("127.0.0.1")),
            ("", None),
            (".", None),
            ("example..com", None),
            (".example.com", None),
            ("example.com..", None),
            ("-example.com", None),
            ("example-.com", None),
            ("exa_mple.com", None),
            ("exa mple.com", None),
            ("example%2ecom", None),
            ("example.com:443", None),
            ("user@example.com", None),
            ("ex/ample.com", None),
            // Non-ASCII look-alikes: Cyrillic `е`, and the ideographic full stop.
            ("\u{0435}xample.com", None),
            ("example\u{3002}com", None),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_host(input).as_deref(), *want, "{input:?}");
        }
        assert!(normalize_host(&format!("{}.com", "a".repeat(64))).is_none());
        assert!(normalize_host(&format!("{}.com", "a".repeat(63))).is_some());
    }

    #[test]
    fn rules_are_validated_on_construction() {
        assert!(HostRule::exact("dashboard.example.com").is_ok());
        assert!(HostRule::exact("localhost").is_ok());
        assert!(HostRule::exact("").is_err());
        assert!(HostRule::exact("https://example.com").is_err());
        assert!(HostRule::exact("example.com/path").is_err());
        assert!(HostRule::exact("*.example.com").is_err());
        assert!(HostRule::domain("example.com").is_ok());
        assert!(HostRule::domain("example.co.uk").is_ok());
        // A bare public suffix, a single label, an IPv4 address.
        assert!(HostRule::domain("com").is_err());
        assert!(HostRule::domain("localhost").is_err());
        assert!(HostRule::domain("10.0.0.1").is_err());
        assert!(HostRule::domain(".example.com").is_err());
        let rule = HostRule::exact("Dashboard.Example.com.").unwrap();
        assert_eq!(rule.host(), "dashboard.example.com");
        assert!(!rule.includes_subdomains());
        assert!(HostRule::domain("example.com")
            .unwrap()
            .includes_subdomains());
    }

    #[test]
    fn an_exact_rule_matches_only_its_host() {
        let rule = HostRule::exact("dashboard.example.com").unwrap();
        let cases: &[(&str, bool)] = &[
            ("dashboard.example.com", true),
            ("DASHBOARD.example.com", true),
            ("dashboard.example.com.", true),
            ("example.com", false),
            ("www.dashboard.example.com", false),
            ("other.example.com", false),
            ("dashboard.example.com.evil.example", false),
            ("dashboard-example.com", false),
            ("evildashboard.example.com", false),
            ("dashboard.example.co", false),
            ("xn--dashbord-x0a.example.com", false),
            ("", false),
        ];
        for (host, want) in cases {
            assert_eq!(rule.matches_host(host), *want, "{host:?}");
        }
    }

    #[test]
    fn a_domain_rule_matches_the_domain_and_its_subdomains_only() {
        let rule = HostRule::domain("example.com").unwrap();
        let cases: &[(&str, bool)] = &[
            ("example.com", true),
            ("www.example.com", true),
            ("a.b.example.com", true),
            ("WWW.Example.Com.", true),
            ("notexample.com", false),
            ("evilexample.com", false),
            ("example.com.evil.example", false),
            ("example.com-evil.example", false),
            ("example.co", false),
            ("xample.com", false),
            ("com", false),
            ("xn--exmple-cua.com", false),
            ("\u{0435}xample.com", false),
            (".example.com", false),
            ("www..example.com", false),
        ];
        for (host, want) in cases {
            assert_eq!(rule.matches_host(host), *want, "{host:?}");
        }
    }

    #[test]
    fn urls_match_on_the_parsed_https_host_never_a_substring() {
        let domain = HostRule::domain("example.com").unwrap();
        let exact = HostRule::exact("app.example.com").unwrap();
        let cases: &[(&str, bool, bool)] = &[
            // url, domain rule, exact rule
            ("https://example.com/", true, false),
            ("https://app.example.com/home?x=1#y", true, true),
            ("https://app.example.com:443/", true, true),
            ("https://app.example.com", true, true),
            ("https://APP.EXAMPLE.COM/", true, true),
            // Scheme downgrade and other ports are other origins.
            ("http://app.example.com/", false, false),
            ("https://app.example.com:8443/", false, false),
            ("wss://app.example.com/", false, false),
            // The host is elsewhere; example.com only appears in the URL.
            (
                "https://evil.example/?next=https://app.example.com",
                false,
                false,
            ),
            ("https://evil.example/app.example.com", false, false),
            ("https://evil.example#app.example.com", false, false),
            ("https://app.example.com.evil.example/", false, false),
            ("https://notexample.com/", false, false),
            // Credentials: refused outright, whichever side looks right.
            ("https://app.example.com@evil.example/", false, false),
            ("https://evil.example@app.example.com/", false, false),
            ("https://app.example.com:443@evil.example/", false, false),
            // Encoded or non-ASCII hosts.
            ("https://app.example%2Ecom/", false, false),
            ("https://app.\u{0435}xample.com/", false, false),
            // Not web pages at all.
            ("about:blank", false, false),
            ("chrome://newtab/", false, false),
            ("file:///app.example.com", false, false),
            ("javascript:alert('app.example.com')", false, false),
            ("https://[::1]/", false, false),
            ("not a url", false, false),
            ("", false, false),
        ];
        for (url, want_domain, want_exact) in cases {
            assert_eq!(domain.matches_url(url), *want_domain, "domain {url:?}");
            assert_eq!(exact.matches_url(url), *want_exact, "exact {url:?}");
        }
    }

    #[test]
    fn url_parts_split_scheme_host_port_and_path() {
        let u = parse_url("HTTPS://App.Example.com:8443/a/b?c=d#e").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "app.example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(parse_url("https://example.com").unwrap().path, "/");
        assert_eq!(parse_url("https://example.com?x").unwrap().path, "/");
        assert_eq!(parse_url("https://example.com#x").unwrap().path, "/");
        assert!(parse_url("https://example.com:/").is_none());
        assert!(parse_url("https://example.com:99999/").is_none());
        assert!(parse_url("https://example.com:4a/").is_none());
        assert!(parse_url("1https://example.com/").is_none());
        assert!(parse_url("://example.com/").is_none());
    }

    #[test]
    fn overlapping_rules_are_detected() {
        let d = |s| HostRule::domain(s).unwrap();
        let e = |s| HostRule::exact(s).unwrap();
        assert!(d("example.com").overlaps(&e("app.example.com")));
        assert!(e("app.example.com").overlaps(&d("example.com")));
        assert!(d("example.com").overlaps(&d("sub.example.com")));
        assert!(e("app.example.com").overlaps(&e("APP.example.com")));
        assert!(!e("app.example.com").overlaps(&e("www.example.com")));
        assert!(!d("example.com").overlaps(&d("example.org")));
        assert!(!e("example.com").overlaps(&e("app.example.com")));
    }

    #[test]
    fn rules_display_for_messages() {
        assert_eq!(
            HostRule::exact("app.example.com").unwrap().to_string(),
            "app.example.com"
        );
        assert_eq!(
            HostRule::domain("example.com").unwrap().to_string(),
            "example.com (or a subdomain)"
        );
    }
}
