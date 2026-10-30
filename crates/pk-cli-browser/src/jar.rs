//! A domain's whole cookie jar, for a site where no single cookie is known
//! to carry the session.
//!
//! The jar is read from the same browser-wide `Storage.getCookies` list as a
//! named cookie, and filtered by host alone, label for label: a cookie is in
//! the jar of `example.com` when the browser would send it to `example.com`
//! or to some host under it. That is a host-only cookie on one of those
//! hosts, a domain cookie on one of them, or a domain cookie on a parent
//! (`.example.com` is in the jar of `app.example.com`). A partitioned cookie
//! counts only when it is keyed to one of those sites in a first-party
//! context. `notexample.com`, `example.com.evil.example` and a punycode
//! look-alike never match.
//!
//! Path is not filtered. A CLI replays the jar to API paths the crate does
//! not know, so every path's cookies are kept, and the header lists the
//! longer paths first, as the browser orders them.

use pk_cli_secrets::Secret;
use serde_json::Value;

use crate::host::{normalize_host, parse_url, HostRule};

/// One cookie of a captured jar. The value is a [`Secret`]; the rest is
/// what the browser reports about the cookie's scope.
#[derive(Debug)]
pub struct JarCookie {
    name: String,
    value: Secret,
    domain: String,
    host_only: bool,
    path: String,
}

impl JarCookie {
    /// The cookie's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The cookie's value.
    pub fn value(&self) -> &Secret {
        &self.value
    }

    /// The host or domain it is scoped to, lowercase, without the leading
    /// dot a domain cookie is listed with.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Whether it is sent to [`JarCookie::domain`] only (no `Domain`
    /// attribute), rather than to that domain and every host under it.
    pub fn host_only(&self) -> bool {
        self.host_only
    }

    /// The path it is scoped to (`/` for most session cookies).
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Whether `host` is `domain` or a host under it, label for label.
pub(crate) fn is_under(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

/// Whether a cookie scoped to `domain` reaches the host `jar` or a host
/// under it: it sits on one of those hosts, or it is a domain cookie on a
/// parent of `jar`. A parent that is a bare suffix (`com`) never holds a
/// cookie in a real browser, and is refused here too.
fn reaches(domain: &str, host_only: bool, jar: &str) -> bool {
    is_under(domain, jar)
        || (!host_only && is_under(jar, domain) && HostRule::domain(domain).is_ok())
}

/// A listed cookie's scope: its normalized domain, and whether it is
/// host-only (listed without a leading dot). `None` for a domain that is not
/// a valid host.
pub(crate) fn cookie_scope(cookie: &Value) -> Option<(String, bool)> {
    let raw = cookie.get("domain")?.as_str()?;
    let host_only = !raw.starts_with('.');
    let domain = normalize_host(raw.trim_start_matches('.'))?;
    Some((domain, host_only))
}

/// Where a listed cookie may be sent from, by its partition: `Some(None)`
/// for an unpartitioned cookie, `Some(Some(site))` for one keyed to the
/// first-party top-level site `site`, and `None` for one keyed to a
/// cross-site context or to a site that cannot be read, which a CLI's own
/// first-party request would never carry.
pub(crate) fn partition_site(cookie: &Value) -> Option<Option<String>> {
    match cookie.get("partitionKey") {
        None | Some(Value::Null) => Some(None),
        Some(key) => {
            let cross_site = key
                .get("hasCrossSiteAncestor")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if cross_site {
                return None;
            }
            let site = key
                .as_str()
                .or_else(|| key.get("topLevelSite").and_then(Value::as_str))
                .and_then(parse_url)?;
            Some(Some(site.host))
        }
    }
}

/// Every cookie in a `Storage.getCookies` list that the browser would send
/// to `rule`'s domain or a host under it (see the module docs), in the
/// order the browser lists them. A cookie without a string name and value
/// is skipped; an empty value is kept, as the browser sends it.
pub(crate) fn jar_cookies(cookies: &Value, rule: &HostRule) -> Vec<JarCookie> {
    let jar = rule.host();
    let Some(all) = cookies.as_array() else {
        return Vec::new();
    };
    all.iter()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?;
            let value = c.get("value")?.as_str()?;
            let (domain, host_only) = cookie_scope(c)?;
            let partition_ok = match partition_site(c)? {
                None => true,
                Some(site) => reaches(&site, false, jar),
            };
            (reaches(&domain, host_only, jar) && partition_ok).then(|| JarCookie {
                name: name.to_string(),
                value: Secret::new(value),
                domain,
                host_only,
                path: c
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("/")
                    .to_string(),
            })
        })
        .collect()
}

/// The jar as a `Cookie` request-header value: `name=value` pairs joined by
/// `; `, longer paths first and otherwise in jar order, as the browser
/// orders them. A cookie with an empty name is sent as its bare value, as
/// the browser does. Two cookies of one name (on different domains or
/// paths) are both listed, as the browser would send both to a URL both
/// match.
pub(crate) fn cookie_header(jar: &[JarCookie]) -> Secret {
    let mut ordered: Vec<&JarCookie> = jar.iter().collect();
    ordered.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
    let mut header = String::new();
    for c in ordered {
        if !header.is_empty() {
            header.push_str("; ");
        }
        if !c.name.is_empty() {
            header.push_str(&c.name);
            header.push('=');
        }
        header.push_str(c.value.expose());
    }
    Secret::new(header)
}

/// The jar's cookie names, sorted and deduplicated, for a diagnostic.
pub(crate) fn jar_names(jar: &[JarCookie]) -> Vec<String> {
    let mut names: Vec<String> = jar.iter().map(|c| c.name.clone()).collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cookie(name: &str, domain: &str) -> Value {
        json!({"name": name, "value": format!("dummy-{name}"), "domain": domain, "path": "/"})
    }

    fn names(jar: &[JarCookie]) -> Vec<&str> {
        jar.iter().map(JarCookie::name).collect()
    }

    #[test]
    fn the_jar_holds_what_the_browser_would_send_to_the_domain_or_under_it() {
        let all = json!([
            cookie("apex_host_only", "example.com"),
            cookie("apex_domain", ".example.com"),
            cookie("www", "www.example.com"),
            cookie("deep", "a.b.example.com"),
            cookie("deep_domain", ".b.example.com"),
            cookie("upper", "WWW.Example.COM"),
            cookie("trailing_dot", "www.example.com."),
            // Confusables and strangers.
            cookie("not_prefix", "notexample.com"),
            cookie("not_prefix_domain", ".notexample.com"),
            cookie("suffix_attack", "example.com.evil.example"),
            cookie("suffix_attack_domain", ".example.com.evil.example"),
            cookie("dash", "example.com-evil.example"),
            cookie("truncated", "xample.com"),
            cookie("other_tld", "example.co"),
            cookie("punycode", "xn--exmple-cua.com"),
            cookie("cyrillic", "\u{0435}xample.com"),
            cookie("foreign", ".evil.example"),
            cookie("public_suffix", ".com"),
            cookie("bad_domain", "exa mple.com"),
            cookie("empty_domain", ""),
            json!({"name": "no_domain", "value": "x", "path": "/"}),
            json!({"name": "no_value", "domain": "example.com", "path": "/"}),
            json!({"value": "no_name", "domain": "example.com", "path": "/"}),
        ]);
        let rule = HostRule::domain("example.com").unwrap();
        assert_eq!(
            names(&jar_cookies(&all, &rule)),
            [
                "apex_host_only",
                "apex_domain",
                "www",
                "deep",
                "deep_domain",
                "upper",
                "trailing_dot"
            ]
        );
    }

    #[test]
    fn a_subdomain_jar_takes_parent_domain_cookies_but_not_siblings() {
        let all = json!([
            cookie("parent_domain", ".example.com"),
            cookie("parent_host_only", "example.com"),
            cookie("own", "app.example.com"),
            cookie("own_domain", ".app.example.com"),
            cookie("child", "x.app.example.com"),
            cookie("sibling", "www.example.com"),
            cookie("sibling_domain", ".www.example.com"),
            cookie("look_alike", "app.example.com.evil.example"),
            cookie("glued", "evilapp.example.com"),
        ]);
        let rule = HostRule::domain("app.example.com").unwrap();
        assert_eq!(
            names(&jar_cookies(&all, &rule)),
            ["parent_domain", "own", "own_domain", "child"]
        );
    }

    #[test]
    fn partitioned_cookies_count_only_in_their_own_first_party_site() {
        let part = |name: &str, key: Value| {
            json!({"name": name, "value": "v", "domain": ".example.com", "path": "/",
                   "partitionKey": key})
        };
        let all = json!([
            part(
                "own",
                json!({"topLevelSite": "https://example.com", "hasCrossSiteAncestor": false})
            ),
            part("own_string_key", json!("https://example.com")),
            part(
                "cross_site",
                json!({"topLevelSite": "https://example.com", "hasCrossSiteAncestor": true})
            ),
            part(
                "other_site",
                json!({"topLevelSite": "https://other.example", "hasCrossSiteAncestor": false})
            ),
            part("confusable_site", json!("https://example.com.evil.example")),
            part("unreadable_site", json!({"topLevelSite": "not a url"})),
            part("bare_suffix_site", json!("https://com")),
            part("null_key", Value::Null),
        ]);
        let rule = HostRule::domain("example.com").unwrap();
        assert_eq!(
            names(&jar_cookies(&all, &rule)),
            ["own", "own_string_key", "null_key"]
        );
    }

    #[test]
    fn every_path_is_kept_and_the_header_lists_longer_paths_first() {
        let all = json!([
            {"name": "a", "value": "1", "domain": ".example.com", "path": "/"},
            {"name": "b", "value": "2", "domain": "www.example.com", "path": "/api/v1"},
            {"name": "c", "value": "", "domain": "www.example.com", "path": "/"},
            {"name": "a", "value": "3", "domain": "www.example.com", "path": "/api"},
            {"name": "", "value": "bare", "domain": "www.example.com", "path": "/"},
            {"name": "d", "value": "4", "domain": "www.example.com"}
        ]);
        let jar = jar_cookies(&all, &HostRule::domain("example.com").unwrap());
        assert_eq!(jar.len(), 6, "every path and the empty value are kept");
        assert_eq!(jar[1].path(), "/api/v1");
        assert_eq!(jar[5].path(), "/", "a cookie listed without a path is at /");
        assert_eq!(jar[0].domain(), "example.com");
        assert!(!jar[0].host_only());
        assert!(jar[1].host_only());
        assert_eq!(cookie_header(&jar).expose(), "b=2; a=3; a=1; c=; bare; d=4");
        assert_eq!(jar_names(&jar), ["", "a", "b", "c", "d"]);
        assert_eq!(cookie_header(&[]).expose(), "");
    }

    #[test]
    fn values_never_print() {
        let all = json!([{"name": "sid", "value": "dummy-secret", "domain": "example.com"}]);
        let jar = jar_cookies(&all, &HostRule::domain("example.com").unwrap());
        let debug = format!("{jar:?} {:?}", cookie_header(&jar));
        assert!(!debug.contains("dummy-secret"), "{debug}");
        assert_eq!(jar[0].value().expose(), "dummy-secret");
    }

    #[test]
    fn a_list_that_is_not_an_array_is_an_empty_jar() {
        let rule = HostRule::domain("example.com").unwrap();
        assert!(jar_cookies(&Value::Null, &rule).is_empty());
        assert!(jar_cookies(&json!({"cookies": []}), &rule).is_empty());
    }

    #[test]
    fn hosts_are_under_a_domain_label_for_label() {
        let cases: &[(&str, &str, bool)] = &[
            ("example.com", "example.com", true),
            ("www.example.com", "example.com", true),
            ("a.b.example.com", "example.com", true),
            ("notexample.com", "example.com", false),
            ("example.com", "www.example.com", false),
            ("example.com.evil.example", "example.com", false),
            ("xample.com", "example.com", false),
        ];
        for (host, domain, want) in cases {
            assert_eq!(is_under(host, domain), *want, "{host} under {domain}");
        }
    }
}
