//! A session's cookies as one `Cookie` header, kept current from the
//! `Set-Cookie` lines its answers carry.

/// `header` with the values `set_cookies` gives cookies it already carries,
/// or `None` when none changes. A cookie the header does not carry is not
/// added: an Experience Cloud site sets several on every answer, and none of
/// those carries the session. A cookie being removed (an empty value, a
/// `Max-Age` of zero or less, or an expiry in 1970) is not merged either: a
/// removed session is a signed-out one, which the next read reports.
pub fn merge_set_cookies(header: &str, set_cookies: &[&str]) -> Option<String> {
    let mut pairs: Vec<(String, String)> = header
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    let mut changed = false;
    for line in set_cookies {
        let mut parts = line.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.trim().split_once('=')) else {
            continue;
        };
        let removal = value.is_empty()
            || parts.any(|attr| {
                let a = attr.trim().to_ascii_lowercase();
                a == "max-age=0"
                    || a.starts_with("max-age=-")
                    || a.contains("expires=thu, 01 jan 1970")
            });
        if removal {
            continue;
        }
        for (n, v) in pairs.iter_mut() {
            if n == name && v != value {
                *v = value.to_string();
                changed = true;
            }
        }
    }
    changed.then(|| {
        pairs
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

/// Whether `header` can go on a request as it is: visible ASCII and spaces
/// only, so no line break or control character reaches the wire.
pub fn is_sendable(header: &str) -> bool {
    !header.is_empty() && header.bytes().all(|b| (0x20..0x7f).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotated_cookie_is_merged_and_new_or_removed_ones_are_not() {
        let header = "oid=1; sid=old; BrowserId=b";
        assert_eq!(
            merge_set_cookies(header, &["sid=new; Path=/; Secure; HttpOnly"]).as_deref(),
            Some("oid=1; sid=new; BrowserId=b")
        );
        assert_eq!(merge_set_cookies(header, &["renderCtx=x; Path=/"]), None);
        assert_eq!(merge_set_cookies(header, &["sid=old; Path=/"]), None);
        assert_eq!(merge_set_cookies(header, &["sid=; Max-Age=0"]), None);
        assert_eq!(merge_set_cookies(header, &["sid=gone; Max-Age=-1"]), None);
        assert_eq!(
            merge_set_cookies(header, &["sid=gone; Expires=Thu, 01 Jan 1970 00:00:00 GMT"]),
            None
        );
        assert_eq!(merge_set_cookies(header, &["garbage"]), None);
    }

    #[test]
    fn only_a_plain_header_is_sendable() {
        assert!(is_sendable("sid=a; oid=b"));
        assert!(!is_sendable(""));
        assert!(!is_sendable("sid=a\r\nX-Injected: 1"));
        assert!(!is_sendable("sid=\u{e9}"));
    }
}
