//! The authorization request a browser opens, and the answer it brings back.

use pk_cli_core::transport::percent_encode;
use pk_cli_core::CliError;
use pk_cli_secrets::Secret;

use super::pkce::{Pkce, S256};
use super::secure_endpoint;

/// Parameters the request sets itself. A caller's extra parameter may not
/// replace one: a second `redirect_uri` or `code_challenge_method` is how a
/// request gets bent.
const RESERVED: [&str; 7] = [
    "response_type",
    "client_id",
    "redirect_uri",
    "scope",
    "state",
    "code_challenge",
    "code_challenge_method",
];

/// An authorization-code request (RFC 6749 §4.1.1) with PKCE S256 (RFC 7636)
/// and a `state`. Extra parameters carry what a provider or a profile adds: an
/// RFC 8707 `resource`, SMART's `aud`, a `prompt`.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    endpoint: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    state: String,
    scope: Option<String>,
    params: Vec<(String, String)>,
}

impl AuthorizationRequest {
    pub fn new(
        endpoint: impl Into<String>,
        client_id: impl Into<String>,
        redirect_uri: impl Into<String>,
        pkce: &Pkce,
        state: impl Into<String>,
    ) -> Self {
        AuthorizationRequest {
            endpoint: endpoint.into(),
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
            challenge: pkce.challenge().to_string(),
            state: state.into(),
            scope: None,
            params: Vec::new(),
        }
    }

    /// Space-separated scopes.
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// One more query parameter, sent after the request's own.
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.push((name.into(), value.into()));
        self
    }

    /// The address to open. Refuses an endpoint that is not `https` (or `http`
    /// to this machine, for a test server) as [`CliError::Upstream`], since it
    /// comes from a provider's metadata, and an extra parameter that would
    /// replace one of the request's own as [`CliError::Usage`].
    pub fn url(&self) -> Result<String, CliError> {
        secure_endpoint(&self.endpoint, "authorization")?;
        if self.endpoint.contains('#') {
            return Err(CliError::Upstream(
                "the authorization endpoint carries a fragment, which RFC 6749 §3.1 forbids".into(),
            ));
        }
        if let Some((name, _)) = self
            .params
            .iter()
            .find(|(name, _)| RESERVED.contains(&name.as_str()))
        {
            return Err(CliError::Usage(format!(
                "`{name}` is set by the authorization request itself, not as an extra parameter"
            )));
        }
        let mut pairs: Vec<(&str, &str)> = vec![
            ("response_type", "code"),
            ("client_id", &self.client_id),
            ("redirect_uri", &self.redirect_uri),
        ];
        if let Some(scope) = &self.scope {
            pairs.push(("scope", scope));
        }
        pairs.push(("state", &self.state));
        pairs.push(("code_challenge", &self.challenge));
        pairs.push(("code_challenge_method", S256));
        for (name, value) in &self.params {
            pairs.push((name, value));
        }
        let query = pairs
            .iter()
            .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let sep = if self.endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        Ok(format!("{}{sep}{query}", self.endpoint))
    }
}

/// The authorization code from the redirect's request target
/// (`/callback?code=…&state=…`) or a whole redirect URL.
///
/// - `state` must equal `expected_state` (compared in constant time), or the
///   answer is refused as a possible cross-site request forgery. An answer
///   with no `state` is refused too, an `error` answer included: RFC 6749
///   §4.1.2.1 requires a server to return the `state` it was sent.
/// - A parameter given twice is refused (RFC 6749 §3.1).
/// - With `expected_issuer`, an `iss` in the answer (RFC 9207) must equal it, so
///   a code minted by another server is refused. An answer without `iss` passes:
///   most servers do not send one yet.
/// - An `error` answer is [`CliError::Auth`] quoting the provider's slug and
///   description, cut to printable text.
///
/// Every refusal is [`CliError::Auth`] (exit 3): signing in again is the fix.
/// [`super::LoopbackListener`] goes on waiting past an answer that is not this
/// sign-in's (the first two rules), so a reloaded old tab or a page elsewhere
/// cannot end it.
pub fn code_from_callback(
    target: &str,
    expected_state: &str,
    expected_issuer: Option<&str>,
) -> Result<Secret, CliError> {
    match check_callback(target, expected_state, expected_issuer) {
        Callback::Code(code) => Ok(code),
        Callback::Refused(err) => Err(err),
        Callback::NotThisSignIn(reason) => Err(CliError::Auth(format!("{reason}. Sign in again"))),
    }
}

/// What one request to the callback says about this sign-in.
pub(crate) enum Callback {
    /// This sign-in's code.
    Code(Secret),
    /// This sign-in's answer, and a refusal: the provider's `error`, another
    /// issuer, no code.
    Refused(CliError),
    /// Not provably this sign-in's: no `state`, another one, or a repeated
    /// parameter. The wait goes on; this says why, with no remedy, since the
    /// remedy depends on whether the wait is still going.
    NotThisSignIn(String),
}

pub(crate) fn check_callback(
    target: &str,
    expected_state: &str,
    expected_issuer: Option<&str>,
) -> Callback {
    let query = target
        .split_once('?')
        .map_or("", |(_, query)| query)
        .split('#')
        .next()
        .unwrap_or_default();
    let params = parse_query(query);
    for (i, (name, _)) in params.iter().enumerate() {
        if params[..i].iter().any(|(earlier, _)| earlier == name) {
            return Callback::NotThisSignIn(format!(
                "the sign-in's answer carried `{}` more than once; refusing it",
                printable(name)
            ));
        }
    }
    let one = |name: &str| {
        params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };

    match one("state") {
        Some(state) if constant_time_eq(state.as_bytes(), expected_state.as_bytes()) => {}
        Some(_) => {
            return Callback::NotThisSignIn(
                "the sign-in's answer carried a different `state`; refusing it as a possible \
                 cross-site request forgery"
                    .into(),
            )
        }
        None => {
            // A server that leaves `state` off its error may be answering this
            // sign-in, so its refusal is named, though it cannot end the wait.
            let error = one("error").map(|error| {
                let description = one("error_description")
                    .map(|d| format!(": {}", printable(d)))
                    .unwrap_or_default();
                format!(
                    " (the provider refused a sign-in: {}{description})",
                    printable(error)
                )
            });
            return Callback::NotThisSignIn(format!(
                "the sign-in's answer carried no `state`; refusing it{}",
                error.unwrap_or_default()
            ));
        }
    }
    if let (Some(expected), Some(iss)) = (expected_issuer, one("iss")) {
        if iss != expected {
            return Callback::Refused(CliError::Auth(format!(
                "the sign-in's answer came from issuer {}, not {}; refusing it. Sign in again",
                printable(iss),
                printable(expected)
            )));
        }
    }
    if let Some(error) = one("error") {
        let description = one("error_description")
            .map(|d| format!(": {}", printable(d)))
            .unwrap_or_default();
        return Callback::Refused(CliError::Auth(format!(
            "the provider refused the sign-in ({}{description})",
            printable(error)
        )));
    }
    match one("code") {
        Some(code) if !code.is_empty() => Callback::Code(Secret::new(code)),
        _ => Callback::Refused(CliError::Auth(
            "the sign-in's answer carried no authorization code. Sign in again".into(),
        )),
    }
}

/// Open `url` in the system browser: `open` on macOS, `xdg-open` on Linux.
/// Returns whether the launcher started. Only an `https` address, or `http` to
/// this machine, is handed over, so a provider's metadata cannot point the
/// launcher at a file or another application's scheme. Print the address as
/// well: a failed launch is a convenience lost, and the person can paste it.
pub fn open_browser(url: &str) -> bool {
    if pk_cli_core::transport::require_secure_url(url).is_err() {
        return false;
    }
    #[cfg(target_os = "macos")]
    let launcher = Some("open");
    #[cfg(target_os = "linux")]
    let launcher = Some("xdg-open");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let launcher: Option<&str> = None;

    launcher.is_some_and(|cmd| {
        std::process::Command::new(cmd)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// `application/x-www-form-urlencoded` pairs, decoded: `+` is a space and a
/// malformed escape stays as it is.
pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (form_decode(name), form_decode(value))
        })
        .collect()
}

fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Provider text for a terminal: control characters (an escape sequence could
/// rewrite the screen) become spaces, and it stops at 200 characters.
pub(crate) fn printable(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    if text.chars().count() > 200 {
        out.push('…');
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkce() -> Pkce {
        Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap()
    }

    #[test]
    fn the_url_carries_pkce_state_and_the_extras_encoded() {
        let url = AuthorizationRequest::new(
            "https://auth.example.test/authorize",
            "client-1",
            "http://127.0.0.1:52321/callback",
            &pkce(),
            "STATE",
        )
        .scope("openid patient/*.read")
        .param("aud", "https://fhir.example.test/r4")
        .url()
        .unwrap();
        assert_eq!(
            url,
            "https://auth.example.test/authorize?response_type=code&client_id=client-1\
             &redirect_uri=http%3A%2F%2F127.0.0.1%3A52321%2Fcallback\
             &scope=openid%20patient%2F%2A.read&state=STATE\
             &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &code_challenge_method=S256&aud=https%3A%2F%2Ffhir.example.test%2Fr4"
        );
    }

    #[test]
    fn an_endpoint_with_a_query_keeps_it() {
        let url = AuthorizationRequest::new(
            "https://auth.example.test/authorize?tenant=a",
            "c",
            "http://127.0.0.1:1/callback",
            &pkce(),
            "s",
        )
        .url()
        .unwrap();
        assert!(url.starts_with("https://auth.example.test/authorize?tenant=a&response_type=code"));
    }

    #[test]
    fn an_extra_may_not_replace_a_parameter_of_the_request() {
        for name in RESERVED {
            let err = AuthorizationRequest::new(
                "https://auth.example.test/authorize",
                "c",
                "http://127.0.0.1:1/callback",
                &pkce(),
                "s",
            )
            .param(name, "x")
            .url()
            .unwrap_err();
            assert_eq!(err.exit_code(), 2, "{name}");
        }
    }

    #[test]
    fn an_insecure_or_fragment_endpoint_is_refused_as_upstream() {
        for endpoint in [
            "http://auth.example.test/authorize",
            "https://auth.example.test/authorize#x",
            "file:///etc/passwd",
        ] {
            let err = AuthorizationRequest::new(endpoint, "c", "r", &pkce(), "s")
                .url()
                .unwrap_err();
            assert_eq!(err.exit_code(), 5, "{endpoint}");
        }
    }

    #[test]
    fn the_code_comes_back_when_state_matches() {
        let code = code_from_callback("/callback?code=abc%2B1&state=XYZ", "XYZ", None).unwrap();
        assert_eq!(code.expose(), "abc+1");
        let code = code_from_callback(
            "http://127.0.0.1:9/callback?state=XYZ&code=a+b#frag",
            "XYZ",
            None,
        )
        .unwrap();
        assert_eq!(code.expose(), "a b");
    }

    #[test]
    fn a_wrong_or_missing_state_is_refused() {
        for target in [
            "/callback?code=abc&state=WRONG",
            "/callback?code=abc",
            "/callback?code=abc&state=",
            "/callback?code=abc&state=XYZ&state=XYZ",
            "/callback?code=abc&code=def&state=XYZ",
        ] {
            let err = code_from_callback(target, "XYZ", None).unwrap_err();
            assert_eq!(err.exit_code(), 3, "{target}");
        }
    }

    #[test]
    fn an_error_answer_names_the_slug_and_description_printably() {
        let err = code_from_callback(
            "/callback?error=access_denied&error_description=User+said%20no%1B%5B2J&state=XYZ",
            "XYZ",
            None,
        )
        .unwrap_err();
        let message = err.to_string();
        assert_eq!(err.exit_code(), 3);
        assert!(
            message.contains("access_denied: User said no [2J"),
            "{message}"
        );
        assert!(!message.contains('\u{1b}'), "{message}");
        // Without a state, an error is not provably this sign-in's.
        let err = code_from_callback("/callback?error=server_error", "XYZ", None).unwrap_err();
        assert!(err.to_string().contains("no `state`"), "{err}");
        assert!(
            err.to_string().contains("refused a sign-in: server_error"),
            "{err}"
        );
        assert!(matches!(
            check_callback("/callback?error=server_error", "XYZ", None),
            Callback::NotThisSignIn(_)
        ));
        // With the wrong state, it is refused as forgery rather than quoted.
        let err = code_from_callback("/callback?error=x&state=NO", "XYZ", None).unwrap_err();
        assert!(err.to_string().contains("forgery"));
    }

    #[test]
    fn only_an_answer_with_this_state_is_this_sign_ins() {
        for target in [
            "/callback?code=abc&state=WRONG",
            "/callback?code=abc",
            "/callback?error=access_denied",
            "/callback?code=abc&state=XYZ&state=XYZ",
        ] {
            assert!(
                matches!(
                    check_callback(target, "XYZ", None),
                    Callback::NotThisSignIn(_)
                ),
                "{target}"
            );
        }
        for target in [
            "/callback?error=access_denied&state=XYZ",
            "/callback?state=XYZ",
            "/callback?code=c&state=XYZ&iss=https%3A%2F%2Fother.example.test",
        ] {
            assert!(
                matches!(
                    check_callback(target, "XYZ", Some("https://auth.example.test")),
                    Callback::Refused(_)
                ),
                "{target}"
            );
        }
    }

    #[test]
    fn an_issuer_is_checked_when_both_sides_have_one() {
        let target = "/callback?code=c&state=s&iss=https%3A%2F%2Fother.example.test";
        assert!(code_from_callback(target, "s", None).is_ok());
        assert!(code_from_callback(target, "s", Some("https://other.example.test")).is_ok());
        let err = code_from_callback(target, "s", Some("https://auth.example.test")).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(
            code_from_callback("/callback?code=c&state=s", "s", Some("https://a.test")).is_ok()
        );
    }

    #[test]
    fn a_codeless_answer_is_refused() {
        for target in [
            "/callback?state=s",
            "/callback?state=s&code=",
            "/callback",
            "",
        ] {
            assert_eq!(
                code_from_callback(target, "s", None)
                    .unwrap_err()
                    .exit_code(),
                3,
                "{target}"
            );
        }
    }

    #[test]
    fn form_decoding_handles_plus_escapes_and_malformed_escapes() {
        assert_eq!(form_decode("a+b%20c"), "a b c");
        assert_eq!(form_decode("%zz%4"), "%zz%4");
        assert_eq!(form_decode("%"), "%");
        assert_eq!(form_decode("%C3%A9"), "é");
    }

    #[test]
    fn printable_cuts_control_characters_and_length() {
        assert_eq!(printable("a\nb\u{1b}c"), "a b c");
        let long = "x".repeat(300);
        assert_eq!(printable(&long).chars().count(), 201);
    }

    #[test]
    fn open_browser_refuses_anything_but_a_secure_url() {
        assert!(!open_browser("file:///etc/passwd"));
        assert!(!open_browser("javascript:alert(1)"));
        assert!(!open_browser("http://example.test/"));
    }
}
