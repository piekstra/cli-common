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
//!
//! The one named cookie a [`Capture::cookie`](crate::Capture::cookie) reads
//! is chosen here too ([`pick_cookie`]), from the same list and by the same
//! scope rules, for one URL.

use pk_cli_secrets::Secret;
use serde_json::Value;

use crate::host::{is_under, normalize_host, parse_url, HostRule, UrlParts};

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

#[cfg(feature = "test-util")]
impl JarCookie {
    /// A jar cookie as a capture hands it over, for a CLI's unit tests (the
    /// `test-util` feature, a dev-dependency only; see the crate docs).
    /// `value` is wrapped in a [`Secret`], as a read one is, and `domain` is
    /// normalized as a read one is: lowercase, without the leading dot a
    /// domain cookie is listed with, or a trailing dot. `host_only` and
    /// `path` are kept as given.
    ///
    /// # Panics
    ///
    /// If `domain` is not a host name, which no cookie the crate reads has.
    pub fn for_test(
        name: impl Into<String>,
        value: impl Into<String>,
        domain: &str,
        host_only: bool,
        path: impl Into<String>,
    ) -> Self {
        let Some(normalized) = normalize_host(domain.trim_start_matches('.')) else {
            panic!("JarCookie::for_test: the domain `{domain}` is not a host name");
        };
        JarCookie {
            name: name.into(),
            value: Secret::new(value),
            domain: normalized,
            host_only,
            path: path.into(),
        }
    }
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
fn cookie_scope(cookie: &Value) -> Option<(String, bool)> {
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
fn partition_site(cookie: &Value) -> Option<Option<String>> {
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

/// The value of cookie `name` that the browser would send with a request
/// to `url`, from a `Storage.getCookies` list: domain-matched (a host-only
/// cookie only on its own host, a domain cookie on the domain and its
/// subdomains), path-matched, `Secure` only over https, and a partitioned
/// cookie only in its own top-level site. Of several matches, the longest
/// path wins, then a host-only cookie, as the browser orders them. An empty
/// value counts as unset.
pub(crate) fn pick_cookie(cookies: &Value, url: &UrlParts, name: &str) -> Option<String> {
    cookies
        .as_array()?
        .iter()
        .filter(|c| c.get("name").and_then(Value::as_str) == Some(name))
        .filter_map(|c| {
            let value = c.get("value")?.as_str().filter(|v| !v.is_empty())?;
            let (domain, host_only) = cookie_scope(c)?;
            let domain_ok = if host_only {
                url.host == domain
            } else {
                is_under(&url.host, &domain)
            };
            let path = c.get("path").and_then(Value::as_str).unwrap_or("/");
            let secure = c.get("secure").and_then(Value::as_bool).unwrap_or(false);
            let partition_ok = match partition_site(c) {
                None => false,
                Some(None) => true,
                Some(Some(site)) => is_under(&url.host, &site),
            };
            (domain_ok
                && path_matches(&url.path, path)
                && (!secure || url.scheme == "https")
                && partition_ok)
                .then_some((path.len(), host_only, value))
        })
        .max_by_key(|(path_len, host_only, _)| (*path_len, *host_only))
        .map(|(_, _, value)| value.to_string())
}

/// RFC 6265 §5.1.4 path-match.
pub(crate) fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request[cookie.len()..].starts_with('/')))
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

    /// A built cookie is the one the jar reads from the same listing.
    #[cfg(feature = "test-util")]
    #[test]
    fn a_built_cookie_is_the_one_a_read_listing_gives() {
        let all = json!([
            {"name": "sid", "value": "dummy-sid", "domain": ".Example.COM", "path": "/api"},
            {"name": "own", "value": "dummy-own", "domain": "www.example.com.", "path": "/"}
        ]);
        let read = jar_cookies(&all, &HostRule::domain("example.com").unwrap());
        let built = [
            JarCookie::for_test("sid", "dummy-sid", ".Example.COM", false, "/api"),
            JarCookie::for_test("own", "dummy-own", "www.example.com.", true, "/"),
        ];
        assert_eq!(read.len(), built.len());
        for (r, b) in read.iter().zip(&built) {
            assert_eq!(
                (
                    r.name(),
                    r.value().expose(),
                    r.domain(),
                    r.host_only(),
                    r.path()
                ),
                (
                    b.name(),
                    b.value().expose(),
                    b.domain(),
                    b.host_only(),
                    b.path()
                )
            );
        }
        assert_eq!(
            cookie_header(&read).expose(),
            cookie_header(&built).expose()
        );
    }

    #[test]
    fn a_list_that_is_not_an_array_is_an_empty_jar() {
        let rule = HostRule::domain("example.com").unwrap();
        assert!(jar_cookies(&Value::Null, &rule).is_empty());
        assert!(jar_cookies(&json!({"cookies": []}), &rule).is_empty());
    }

    #[test]
    fn cookies_are_chosen_as_the_browser_would_send_them() {
        let url = |u: &str| parse_url(u).unwrap();
        let jar = json!([
            {"name": "host_only", "value": "h", "domain": "app.example.com", "path": "/"},
            {"name": "domain", "value": "d", "domain": ".example.com", "path": "/"},
            {"name": "scoped", "value": "root", "domain": ".example.com", "path": "/"},
            {"name": "scoped", "value": "api", "domain": ".example.com", "path": "/api"},
            {"name": "tie", "value": "domain-wide", "domain": ".example.com", "path": "/"},
            {"name": "tie", "value": "host-only", "domain": "app.example.com", "path": "/"},
            {"name": "empty", "value": "", "domain": ".example.com", "path": "/"},
            {"name": "confusable", "value": "c", "domain": ".example.com.evil.example", "path": "/"},
            {"name": "suffix", "value": "s", "domain": ".ample.com", "path": "/"},
            {"name": "part_own", "value": "p1", "domain": ".example.com", "path": "/",
             "partitionKey": {"topLevelSite": "https://example.com", "hasCrossSiteAncestor": false}},
            {"name": "part_cross", "value": "p2", "domain": ".example.com", "path": "/",
             "partitionKey": {"topLevelSite": "https://example.com", "hasCrossSiteAncestor": true}},
            {"name": "part_other", "value": "p3", "domain": ".example.com", "path": "/",
             "partitionKey": "https://other.example"},
            {"name": "part_null", "value": "p4", "domain": ".example.com", "path": "/",
             "partitionKey": null},
            {"name": "no_path", "value": "np", "domain": ".example.com"},
            {"name": "bad_domain", "value": "b", "domain": "exa mple.com", "path": "/"}
        ]);
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("https://app.example.com/", "host_only", Some("h")),
            ("https://www.example.com/", "host_only", None),
            ("https://sub.app.example.com/", "host_only", None),
            ("https://example.com/", "domain", Some("d")),
            ("https://deep.app.example.com/x", "domain", Some("d")),
            ("https://example.org/", "domain", None),
            ("https://notexample.com/", "domain", None),
            // The longest matching path wins; a path only matches on a boundary.
            ("https://app.example.com/api/v1", "scoped", Some("api")),
            ("https://app.example.com/api", "scoped", Some("api")),
            ("https://app.example.com/apiary", "scoped", Some("root")),
            ("https://app.example.com/", "scoped", Some("root")),
            // Same path: the host-only cookie is the more specific.
            ("https://app.example.com/", "tie", Some("host-only")),
            ("https://www.example.com/", "tie", Some("domain-wide")),
            ("https://app.example.com/", "empty", None),
            ("https://app.example.com/", "confusable", None),
            ("https://app.example.com/", "suffix", None),
            ("https://app.example.com/", "part_own", Some("p1")),
            ("https://app.example.com/", "part_cross", None),
            ("https://app.example.com/", "part_other", None),
            ("https://app.example.com/", "part_null", Some("p4")),
            ("https://app.example.com/x", "no_path", Some("np")),
            ("https://app.example.com/", "bad_domain", None),
            ("https://app.example.com/", "missing", None),
        ];
        for (u, name, want) in cases {
            assert_eq!(
                pick_cookie(&jar, &url(u), name).as_deref(),
                *want,
                "{u} {name}"
            );
        }
        // A Secure cookie is never chosen for a plain-http URL.
        let secure = json!([{"name": "s", "value": "v", "domain": "a.example", "path": "/", "secure": true}]);
        assert_eq!(pick_cookie(&secure, &url("http://a.example/"), "s"), None);
        assert_eq!(
            pick_cookie(&secure, &url("https://a.example/"), "s").as_deref(),
            Some("v")
        );
        assert_eq!(
            pick_cookie(&Value::Null, &url("https://a.example/"), "s"),
            None
        );
    }

    #[test]
    fn path_matching_follows_rfc_6265() {
        let cases: &[(&str, &str, bool)] = &[
            ("/", "/", true),
            ("/a", "/", true),
            ("/a/b", "/a", true),
            ("/a/b", "/a/", true),
            ("/a", "/a", true),
            ("/ab", "/a", false),
            ("/", "/a", false),
            ("/b", "/a", false),
        ];
        for (request, cookie, want) in cases {
            assert_eq!(path_matches(request, cookie), *want, "{request} {cookie}");
        }
    }
}
