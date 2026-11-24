//! The Aura protocol, as an Experience Cloud site's pages speak it: the
//! request form, the answer and its failures, and the context the site's
//! home page carries. Pure functions; [`crate::Client`] does the HTTP.
//!
//! What the protocol does on a bad call, recorded against a live site:
//!
//! - an unknown method: HTTP 200 and an empty `actions` list, so an action
//!   with no entry in the answer is an error, never "no data";
//! - a stale framework id (`fwuid`): the action still succeeds, and the answer
//!   adds a `COOSE` ("client out of sync") warning entry and its own `fwuid`;
//! - an error wrapped as `*/{…}/*ERROR*/`, with an `event` naming
//!   `aura:invalidSession` when the token is missing or wrong;
//! - a call made without a live session (no `sid` cookie, or an expired one)
//!   runs as the site's guest user, and the action fails with "You do not
//!   have access to the Apex class named '…'": that is a lost session too.

use pk_cli_core::CliError;
use serde_json::{json, Map, Value};

use crate::Site;

/// The framework build and app a request names. The site's own page carries
/// them; an answer carries the current `fwuid` back.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub fwuid: String,
    pub app: String,
    /// The app's loaded markup versions (`APPLICATION@markup://…` → hash).
    pub loaded: Map<String, Value>,
}

impl Context {
    /// `aura.context` as the site's own requests spell it.
    pub fn to_json(&self) -> String {
        json!({
            "mode": "PROD",
            "fwuid": self.fwuid,
            "app": self.app,
            "loaded": self.loaded,
            "dn": [],
            "globals": {},
            "uad": true,
        })
        .to_string()
    }
}

/// One server action. A batch is posted as one request; when a redeployed
/// site answers it without a result for some of its actions, only those
/// are posted again ([`crate::Client::call`]), so an action that ran is
/// never sent twice.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub descriptor: String,
    pub params: Value,
    /// For the query string and log lines only: the site names each action
    /// there (`other.<Class>.<method>=1`), which makes a request readable.
    pub label: String,
}

impl Action {
    /// An Aura-enabled Apex method called through a component's controller:
    /// `apex://<Class>/ACTION$<method>`.
    pub fn apex(class: &str, method: &str, params: Value) -> Action {
        Action {
            descriptor: format!("apex://{class}/ACTION${method}"),
            params,
            label: format!("{class}.{method}"),
        }
    }

    /// An Apex method a Lightning web component calls (`@AuraEnabled` through
    /// `ApexActionController.execute`).
    pub fn apex_execute(
        namespace: &str,
        class: &str,
        method: &str,
        params: Value,
        cacheable: bool,
    ) -> Action {
        Action {
            descriptor: "aura://ApexActionController/ACTION$execute".into(),
            params: json!({
                "namespace": namespace,
                "classname": class,
                "method": method,
                "params": params,
                "cacheable": cacheable,
                "isContinuation": false,
            }),
            label: format!("ApexAction.execute.{class}.{method}"),
        }
    }

    /// Any other descriptor, as the site sends it.
    pub fn raw(descriptor: &str, params: Value) -> Action {
        let label = descriptor
            .rsplit_once("ACTION$")
            .map(|(_, m)| m.to_string())
            .unwrap_or_else(|| "action".into());
        Action {
            descriptor: descriptor.into(),
            params,
            label,
        }
    }
}

/// The id the `n`th action of a request carries (`1;a`, `2;a`, …).
pub(crate) fn action_id(n: usize) -> String {
    format!("{};a", n + 1)
}

/// The `message` field for `actions`, ids by position.
pub(crate) fn message(actions: &[Action]) -> String {
    let list: Vec<Value> = actions
        .iter()
        .enumerate()
        .map(|(n, a)| {
            json!({
                "id": action_id(n),
                "descriptor": a.descriptor,
                "callingDescriptor": "UNKNOWN",
                "params": a.params,
            })
        })
        .collect();
    json!({ "actions": list }).to_string()
}

/// The query string the site's own requests carry: a counter and one flag
/// per action. The server ignores it; it names the call in logs.
pub(crate) fn query(counter: u32, actions: &[Action]) -> String {
    let mut q = format!("r={counter}");
    for a in actions {
        let flag: String = a
            .label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        q.push_str(&format!("&other.{flag}=1"));
    }
    q
}

/// The form a request posts, in order: `message`, `aura.context`,
/// `aura.pageURI`, `aura.token`.
pub(crate) fn form(
    actions: &[Action],
    ctx: &Context,
    page_uri: &str,
    token: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("message", message(actions)),
        ("aura.context", ctx.to_json()),
        ("aura.pageURI", page_uri.to_string()),
        ("aura.token", token.to_string()),
    ]
}

/// What went wrong with a call, for [`AuraError::into_cli`] to map to an exit
/// code.
#[derive(Debug, Clone, PartialEq)]
pub enum AuraError {
    /// The session is gone: an `aura:invalidSession` event, a call that ran
    /// as the guest user, or an answer that is the sign-in page.
    InvalidSession,
    /// The answer is not the protocol's JSON (an edge page, an outage page).
    NotAura(String),
    /// The action has no entry in the answer: the method is unknown to the
    /// server (renamed or removed).
    Missing { label: String },
    /// The server ran the action and it failed.
    Failed { label: String, message: String },
    /// A framework-level error event that is not a lost session.
    Event(String),
}

impl AuraError {
    /// Exit 3 for a lost session, naming the site's sign-in command; exit 5
    /// for everything else.
    pub fn into_cli(self, site: &Site) -> CliError {
        let name = site.name();
        match self {
            AuraError::InvalidSession => site.lost_session(),
            AuraError::NotAura(what) => {
                CliError::Upstream(format!("{name} did not answer with Aura JSON: {what}"))
            }
            AuraError::Missing { label } => CliError::Upstream(format!(
                "{name} answered without a result for {label}: the method may have been renamed or removed"
            )),
            AuraError::Failed { label, message } => {
                CliError::Upstream(format!("{label} failed on {name}: {message}"))
            }
            AuraError::Event(what) => CliError::Upstream(format!("{name} reported {what}")),
        }
    }
}

/// A parsed answer.
#[derive(Debug, Clone)]
pub struct Answer {
    actions: Vec<Value>,
    /// The `fwuid` the server is on now, when the answer carries one.
    pub fwuid: Option<String>,
    /// The server said the request's `fwuid` is stale (`COOSE`).
    pub out_of_sync: bool,
}

/// Strips the wrappers Aura puts around a body: `while(1);` before JSON a
/// browser must not run, and `*/ … /*ERROR*/` around an error.
fn unwrap_body(body: &str) -> &str {
    let mut s = body.trim();
    if let Some(rest) = s.strip_prefix("while(1);") {
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_prefix("*/") {
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_suffix("/*ERROR*/") {
        s = rest.trim_end();
    }
    s
}

/// Whether a body is the site's sign-in page or the redirect to it, the
/// answer a page or an action gets once the session is gone. A site with a
/// URL prefix signs in at `/<prefix>/s/login`, which this matches too.
pub fn is_sign_in_page(body: &str) -> bool {
    let head: String = body.chars().take(4096).collect();
    let lower = head.to_ascii_lowercase();
    (lower.contains("<html") || lower.contains("<!doctype"))
        && (lower.contains("/s/login")
            || lower.contains("secur/frontdoor")
            || lower.contains("siteforce:loginapp"))
}

/// Parses an answer to a request.
pub fn parse(body: &str) -> Result<Answer, AuraError> {
    let text = unwrap_body(body);
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) if is_sign_in_page(body) => return Err(AuraError::InvalidSession),
        Err(_) => {
            let start: String = body.trim().chars().take(60).collect();
            return Err(AuraError::NotAura(format!(
                "{} bytes starting {start:?}",
                body.len()
            )));
        }
    };
    if let Some(event) = v.get("event") {
        let descriptor = event
            .get("descriptor")
            .and_then(Value::as_str)
            .unwrap_or("");
        if descriptor.contains("invalidSession") {
            return Err(AuraError::InvalidSession);
        }
        if v.get("exceptionEvent")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || !descriptor.is_empty()
        {
            let message = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(descriptor);
            return Err(AuraError::Event(message.to_string()));
        }
    }
    let actions = v
        .get("actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let out_of_sync = actions
        .iter()
        .any(|a| a.get("id").and_then(Value::as_str) == Some("COOSE"));
    let fwuid = v
        .pointer("/context/fwuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Answer {
        actions,
        fwuid,
        out_of_sync,
    })
}

impl Answer {
    /// Whether the answer carries a result for the `n`th action.
    pub fn has(&self, n: usize) -> bool {
        let id = action_id(n);
        self.actions
            .iter()
            .any(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
    }

    /// Whether any action of the answer failed as a call without a live
    /// session does: it ran as the site's guest user, or the session is
    /// invalid. Such an answer's cookies are not the session's.
    pub fn is_lost_session(&self) -> bool {
        self.actions.iter().any(|entry| {
            entry.get("state").and_then(Value::as_str) == Some("ERROR")
                && entry_error(entry).is_some_and(|m| is_lost_message(&m))
        })
    }

    /// Adds the results of `retried`, a request of the actions at
    /// `positions` of this answer's request, under their places here; and
    /// the framework id that answer names.
    pub(crate) fn merge_retry(&mut self, retried: Answer, positions: &[usize]) {
        for (k, &original) in positions.iter().enumerate() {
            let id = action_id(k);
            if let Some(mut entry) = retried
                .actions
                .iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .cloned()
            {
                entry["id"] = Value::String(action_id(original));
                self.actions.push(entry);
            }
        }
        if retried.fwuid.is_some() {
            self.fwuid = retried.fwuid;
        }
        self.out_of_sync = retried.out_of_sync;
    }

    /// The return value of the `n`th action of the request, labelled `label`
    /// for errors. [`crate::Site::take`] gives the exit-code error instead.
    pub fn take(&self, n: usize, label: &str) -> Result<Value, AuraError> {
        let id = action_id(n);
        let entry = self
            .actions
            .iter()
            .find(|a| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
            .ok_or_else(|| AuraError::Missing {
                label: label.into(),
            })?;
        match entry.get("state").and_then(Value::as_str) {
            Some("SUCCESS") => Ok(entry.get("returnValue").cloned().unwrap_or(Value::Null)),
            state => {
                let message = entry_error(entry)
                    .unwrap_or_else(|| format!("state {}", state.unwrap_or("absent")));
                if is_lost_message(&message) {
                    return Err(AuraError::InvalidSession);
                }
                Err(AuraError::Failed {
                    label: label.into(),
                    message,
                })
            }
        }
    }
}

impl Answer {
    /// The result of the `n`th action when it is an [`Action::apex_execute`]
    /// call: `ApexActionController` wraps a method's result as
    /// `{"returnValue": …, "cacheable": …}`, and a method often returns its
    /// result as a JSON string ([`json_value`]).
    pub fn take_apex(&self, n: usize, label: &str) -> Result<Value, AuraError> {
        let wrapped = self.take(n, label)?;
        Ok(json_value(
            wrapped.get("returnValue").cloned().unwrap_or(Value::Null),
        ))
    }
}

/// The first error message of an action's entry.
fn entry_error(entry: &Value) -> Option<String> {
    entry
        .get("error")
        .and_then(Value::as_array)
        .and_then(|errs| errs.first())
        .and_then(error_message)
}

/// An action error that means the call had no live session.
fn is_lost_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("invalid session") || lower.contains("do not have access to the apex class")
}

/// The message of one entry of an action's `error` list. An Apex exception
/// carries `message`; an `AuraHandledException` carries it under `event`.
fn error_message(err: &Value) -> Option<String> {
    err.get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            err.pointer("/event/attributes/values/message")
                .and_then(Value::as_str)
        })
        .or_else(|| err.pointer("/pageErrors/0/message").and_then(Value::as_str))
        .map(str::to_string)
}

/// The context the signed-in home page's scripts are built for: the first
/// `…/s/sfsites/l/<context>/{bootstrap,inline,resources}.js` URL whose
/// URL-encoded JSON names an `fwuid`, an `app` and `loaded`. `None` when the
/// page carries none (it is not the signed-in app).
pub fn bootstrap_context(html: &str) -> Option<Context> {
    const MARK: &str = "/s/sfsites/l/";
    let mut rest = html;
    while let Some(at) = rest.find(MARK) {
        let after = &rest[at + MARK.len()..];
        rest = after;
        let Some(slash) = after.find('/') else {
            continue;
        };
        let (segment, tail) = after.split_at(slash);
        if !["/bootstrap.js", "/inline.js", "/resources.js"]
            .iter()
            .any(|s| tail.starts_with(s))
        {
            continue;
        }
        let Some(decoded) = percent_decode(segment) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&decoded) else {
            continue;
        };
        let (Some(fwuid), Some(app), Some(loaded)) = (
            v.get("fwuid").and_then(Value::as_str),
            v.get("app").and_then(Value::as_str),
            v.get("loaded").and_then(Value::as_object),
        ) else {
            continue;
        };
        if fwuid.is_empty() || app.is_empty() {
            continue;
        }
        return Some(Context {
            fwuid: fwuid.to_string(),
            app: app.to_string(),
            loaded: loaded.clone(),
        });
    }
    None
}

/// `%XX` decoding of a URL path segment. `None` on a malformed escape or
/// bytes that are not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// An Apex controller often returns its result as a JSON string; this reads
/// either that or a value as is.
pub fn json_value(v: Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn a_successful_action_yields_its_return_value() {
        let answer = parse(&fixture("ok.json")).unwrap();
        assert_eq!(answer.take(0, "probe").unwrap()["returnValue"]["rows"], 2);
        assert!(!answer.out_of_sync);
        assert_eq!(answer.fwuid.as_deref(), Some("RFVNTVktRldVSUQtTkVX"));
    }

    #[test]
    fn an_apex_execute_result_is_unwrapped_and_read_as_json() {
        let answer = parse(&fixture("ok.json")).unwrap();
        assert_eq!(answer.take_apex(0, "probe").unwrap(), json!({"rows": 2}));
        let as_string = r#"{"actions":[{"id":"1;a","state":"SUCCESS","returnValue":{"returnValue":"[1,2]","cacheable":false}}]}"#;
        assert_eq!(
            parse(as_string).unwrap().take_apex(0, "x").unwrap(),
            json!([1, 2])
        );
        assert_eq!(
            parse(&fixture("unknown-method.json"))
                .unwrap()
                .take_apex(0, "x")
                .unwrap_err(),
            AuraError::Missing { label: "x".into() }
        );
    }

    #[test]
    fn a_stale_framework_id_still_answers_and_says_so() {
        let answer = parse(&fixture("stale-fwuid.json")).unwrap();
        assert_eq!(answer.take(0, "probe").unwrap(), Value::Bool(true));
        assert!(answer.out_of_sync);
        // The warning is not an action of the request.
        assert!(answer.has(0));
        assert!(!answer.has(1));
    }

    #[test]
    fn an_unknown_method_is_an_error_not_an_empty_result() {
        let answer = parse(&fixture("unknown-method.json")).unwrap();
        assert_eq!(
            answer.take(0, "Class.noSuchMethod").unwrap_err(),
            AuraError::Missing {
                label: "Class.noSuchMethod".into()
            }
        );
    }

    #[test]
    fn wrapped_error_events_are_read() {
        assert_eq!(
            parse(&fixture("invalid-csrf.txt")).unwrap_err(),
            AuraError::InvalidSession
        );
        let other = r#"*/{"message":"An internal server error has occurred","event":{"descriptor":"markup://aura:systemError"},"exceptionEvent":true}/*ERROR*/"#;
        assert_eq!(
            parse(other).unwrap_err(),
            AuraError::Event("An internal server error has occurred".into())
        );
        let guarded = r#"while(1);{"actions":[{"id":"1;a","state":"SUCCESS","returnValue":1}]}"#;
        assert_eq!(parse(guarded).unwrap().take(0, "x").unwrap(), 1);
    }

    #[test]
    fn a_sign_in_page_is_a_lost_session_and_other_html_is_not_aura() {
        let page = fixture("signed-out.html");
        assert!(is_sign_in_page(&page));
        assert_eq!(parse(&page).unwrap_err(), AuraError::InvalidSession);
        assert!(!is_sign_in_page("<html>Service Unavailable</html>"));
        assert!(matches!(
            parse("<html>Service Unavailable</html>"),
            Err(AuraError::NotAura(_))
        ));
    }

    #[test]
    fn a_failed_action_carries_the_server_message() {
        let body = r#"{"actions":[{"id":"1;a","state":"ERROR","returnValue":null,"error":[{"message":"Attempt to de-reference a null object","exceptionType":"System.NullPointerException"}]}]}"#;
        assert_eq!(
            parse(body).unwrap().take(0, "C.m").unwrap_err(),
            AuraError::Failed {
                label: "C.m".into(),
                message: "Attempt to de-reference a null object".into()
            }
        );
        let handled = r#"{"actions":[{"id":"1;a","state":"ERROR","error":[{"event":{"attributes":{"values":{"message":"No such record"}}}}]}]}"#;
        assert_eq!(
            parse(handled).unwrap().take(0, "C.m").unwrap_err(),
            AuraError::Failed {
                label: "C.m".into(),
                message: "No such record".into()
            }
        );
    }

    #[test]
    fn a_guest_call_is_a_lost_session() {
        let answer = parse(&fixture("guest-no-access.json")).unwrap();
        assert_eq!(
            answer.take(0, "C.m").unwrap_err(),
            AuraError::InvalidSession
        );
        assert!(answer.is_lost_session());
        assert!(!parse(&fixture("ok.json")).unwrap().is_lost_session());
        let failed = r#"{"actions":[{"id":"1;a","state":"ERROR","error":[{"message":"Attempt to de-reference a null object"}]}]}"#;
        assert!(!parse(failed).unwrap().is_lost_session());
    }

    #[test]
    fn a_retried_answer_fills_its_actions_places() {
        let mut first = parse(
            r#"{"actions":[{"id":"1;a","state":"SUCCESS","returnValue":"a"},{"id":"COOSE","state":"warning"}],"context":{"fwuid":"NEW"}}"#,
        )
        .unwrap();
        let retried = parse(
            r#"{"actions":[{"id":"1;a","state":"SUCCESS","returnValue":"c"}],"context":{"fwuid":"NEWER"}}"#,
        )
        .unwrap();
        // The first request had three actions; the third had no result and
        // was posted again on its own.
        assert!(!first.has(2));
        first.merge_retry(retried, &[2]);
        assert_eq!(first.take(0, "a").unwrap(), "a");
        assert_eq!(first.take(2, "c").unwrap(), "c");
        assert!(!first.has(1));
        assert_eq!(first.fwuid.as_deref(), Some("NEWER"));
        assert!(!first.out_of_sync);
    }

    #[test]
    fn the_form_names_each_action_by_position() {
        let ctx = Context {
            fwuid: "FW".into(),
            app: "siteforce:communityApp".into(),
            loaded: Map::new(),
        };
        let actions = [
            Action::apex("C", "a", json!({})),
            Action::apex_execute("", "D", "b", json!({"x": 1}), true),
            Action::raw("serviceComponent://ui.x/ACTION$getThing", json!({})),
        ];
        let form = form(&actions, &ctx, "/s/", "TOKEN");
        let names: Vec<&str> = form.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            ["message", "aura.context", "aura.pageURI", "aura.token"]
        );
        let msg: Value = serde_json::from_str(&form[0].1).unwrap();
        assert_eq!(msg["actions"][0]["id"], "1;a");
        assert_eq!(msg["actions"][0]["descriptor"], "apex://C/ACTION$a");
        assert_eq!(msg["actions"][1]["id"], "2;a");
        assert_eq!(msg["actions"][1]["params"]["classname"], "D");
        assert_eq!(msg["actions"][1]["params"]["params"]["x"], 1);
        assert_eq!(actions[2].label, "getThing");
        let ctx_json: Value = serde_json::from_str(&form[1].1).unwrap();
        assert_eq!(ctx_json["fwuid"], "FW");
        assert_eq!(form[3], ("aura.token", "TOKEN".to_string()));
        assert_eq!(
            query(3, &actions[..2]),
            "r=3&other.C.a=1&other.ApexAction.execute.D.b=1"
        );
    }

    #[test]
    fn the_context_is_read_from_the_bootstrap_script_url() {
        let got = bootstrap_context(&fixture("home.html")).unwrap();
        assert_eq!(got.fwuid, "RFVNTVktRldVSUQ");
        assert_eq!(got.app, "siteforce:communityApp");
        assert_eq!(
            got.loaded["APPLICATION@markup://siteforce:communityApp"],
            "dummy_loaded_version"
        );
        assert_eq!(bootstrap_context(&fixture("signed-out.html")), None);
        assert_eq!(bootstrap_context("<html>no scripts</html>"), None);
        // A site with a URL prefix serves the same scripts under it.
        let prefixed = fixture("home.html").replace("\"/s/sfsites/", "\"/members/s/sfsites/");
        assert_eq!(
            bootstrap_context(&prefixed).unwrap().fwuid,
            "RFVNTVktRldVSUQ"
        );
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(percent_decode("%7B%7D").as_deref(), Some("{}"));
    }

    #[test]
    fn a_string_return_value_holding_json_is_read_as_json() {
        assert_eq!(
            json_value(Value::String(r#"{"a":1}"#.into())),
            json!({"a": 1})
        );
        assert_eq!(json_value(Value::String("plain".into())), json!("plain"));
        assert_eq!(json_value(json!([1])), json!([1]));
    }
}
