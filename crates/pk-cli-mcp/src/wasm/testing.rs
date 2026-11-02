//! Run a Worker toolset natively, through the same ABI functions the
//! exports call, with the test playing the host (features `wasm` and
//! `test-util`).
//!
//! ```ignore
//! let out = pk_cli_mcp::wasm::testing::call(
//!     worker::tools,
//!     json!({"name": "svc_items", "arguments": {}}),
//!     json!({"api_key": "dummy"}),
//!     |asked| match asked {
//!         Asked::Fetch(req) => Reply::Response(HttpResponse::new(200, fixture())),
//!         Asked::SignEs256 { .. } => Reply::Unsupported,
//!     },
//! );
//! assert_eq!(out["result"]["isError"], false);
//! ```

use pk_cli_core::transport::{HttpRequest, HttpResponse};
use serde_json::Value;

use super::host::{self, Started};

/// The lines the module logged (a panic's message, at
/// [`Level::Error`](super::Level::Error)) since the last call.
pub use super::host::take_logs;
use super::runtime::{self, STATUS_FAILED, STATUS_UNSUPPORTED};
use crate::toolset::Toolset;

/// What the module asked of the host. `Debug` shows no key material.
#[derive(Clone, PartialEq, Eq)]
pub enum Asked {
    /// A `fetch`, decoded back into the request the module built.
    Fetch(HttpRequest),
    /// An ES256 signature over `data` with the PKCS#8 DER `key`.
    SignEs256 { key: Vec<u8>, data: Vec<u8> },
}

impl std::fmt::Debug for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Asked::Fetch(request) => f.debug_tuple("Fetch").field(request).finish(),
            Asked::SignEs256 { key, data } => f
                .debug_struct("SignEs256")
                .field("key_len", &key.len())
                .field("data_len", &data.len())
                .finish(),
        }
    }
}

/// The host's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A `fetch`'s HTTP response, headers included.
    Response(HttpResponse),
    /// A signature, `r‖s`.
    Signature(Vec<u8>),
    /// The operation failed (status `-1`) with this message.
    Failed(String),
    /// The host does not offer the operation (status `-2`).
    Unsupported,
}

/// What `pk_describe` hands the host for the toolset `factory` builds.
pub fn describe<T: Toolset, F: Fn(&Value) -> T>(factory: F) -> Value {
    runtime::describe(&factory(&Value::Null))
}

/// One `tools/call` through the ABI: `params` is the call's params object,
/// `config` the per-call JSON, and `host` answers every operation the call
/// starts. Returns what `pk_poll` hands the host: `{"result": …}` or
/// `{"error": …}`.
///
/// Panics if the call waits with nothing in flight: a future that waits on
/// anything but the host would hang a real Worker the same way.
pub fn call<T, F>(
    factory: F,
    params: Value,
    config: Value,
    mut host: impl FnMut(Asked) -> Reply,
) -> Value
where
    T: Toolset + 'static,
    F: Fn(&Value) -> T,
{
    let task = runtime::spawn_call(
        params.to_string().as_bytes(),
        config.to_string().as_bytes(),
        factory,
    );
    loop {
        if let Some(answer) = runtime::poll(task) {
            return answer;
        }
        let started = host::take_started();
        assert!(
            !started.is_empty(),
            "the call is waiting, but on nothing the host was asked for"
        );
        for s in started {
            let (op, asked) = match s {
                Started::Fetch { op, request } => (op, Asked::Fetch(decode(&request))),
                Started::SignEs256 { op, key, data } => (op, Asked::SignEs256 { key, data }),
            };
            let (status, data) = match host(asked) {
                Reply::Response(r) => (
                    i32::from(r.status),
                    super::transport::encode_response(&r.headers, r.body.as_bytes()),
                ),
                Reply::Signature(sig) => (0, sig),
                Reply::Failed(m) => (STATUS_FAILED, m.into_bytes()),
                Reply::Unsupported => (STATUS_UNSUPPORTED, Vec::new()),
            };
            runtime::complete(op, status, data);
        }
    }
}

fn decode(request: &[u8]) -> HttpRequest {
    let v: Value = serde_json::from_slice(request).expect("the module sends JSON");
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let mut out = HttpRequest::new(text(&v["method"]), text(&v["url"]));
    for pair in v["headers"].as_array().into_iter().flatten() {
        out = out.header(text(&pair[0]), text(&pair[1]));
    }
    if let Some(body) = v["body"].as_str() {
        out = out.body(body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ToolError;
    use crate::result::ToolOutput;
    use crate::tool::ToolDef;
    use crate::toolset::Server;
    use pk_cli_core::transport::{PlatformSigner, Transport};
    use serde_json::json;

    struct Twice;

    impl Toolset for Twice {
        fn server(&self) -> Server {
            Server::new("twice", "0.0.0", "Twice", "twice")
        }

        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef::read(
                "twice_get",
                "Get twice",
                "Gets. Then again. Then stops.",
            )]
        }

        async fn call(&self, _name: &str, _args: Value) -> Result<ToolOutput, ToolError> {
            let t = super::super::HostTransport;
            let a = t.send(HttpRequest::get("https://h.test/a")).await?;
            let b = t.send(post_json("https://h.test/b", "{}")).await?;
            let signed = t.sign_es256(b"k", b"m").await.transpose()?.is_some();
            ToolOutput::new(&json!({
                "a": a.body,
                "etag": a.header_value("ETag"),
                "b": b.status,
                "signed": signed,
            }))
        }
    }

    fn post_json(url: &str, body: &str) -> HttpRequest {
        HttpRequest::new("POST", url)
            .header("Content-Type", "application/json")
            .body(body)
    }

    #[test]
    fn the_test_host_answers_each_operation_in_turn() {
        let mut seen = Vec::new();
        let out = call(
            |_: &Value| Twice,
            json!({"name": "twice_get"}),
            json!({}),
            |asked| {
                seen.push(asked.clone());
                match asked {
                    Asked::Fetch(r) if r.url.ends_with("/a") => {
                        Reply::Response(HttpResponse::new(200, "first").header("etag", "\"1\""))
                    }
                    Asked::Fetch(_) => Reply::Response(HttpResponse::new(204, "")),
                    Asked::SignEs256 { .. } => Reply::Unsupported,
                }
            },
        );
        assert_eq!(
            out["result"]["structuredContent"],
            json!({"a": "first", "etag": "\"1\"", "b": 204, "signed": false})
        );
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1], Asked::Fetch(post_json("https://h.test/b", "{}")));
        assert_eq!(describe(|_: &Value| Twice)["tools"][0]["name"], "twice_get");
    }
}
