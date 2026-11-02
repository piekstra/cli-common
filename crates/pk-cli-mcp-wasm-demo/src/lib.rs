//! A toolset built for the deploy-your-own Worker's ABI, so the reference
//! host (`crates/pk-cli-mcp/js/pk-host.ts`) can be tested against a real
//! module: `make wasm-host-check` builds this crate for
//! `wasm32-unknown-unknown` and runs `pk-host.test.ts` over it.
//!
//! Each tool exercises one thing a host provides: `fetch`, WebCrypto
//! ES256, two operations in flight at once, the clock and the random
//! source, and a panic (which must trap and be survived).

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;

use pk_cli_core::transport::{HttpRequest, Transport};
use pk_cli_core::CliError;
use pk_cli_mcp::wasm::{fill_random, now_ms, HostTransport};
use pk_cli_mcp::{parse_args, Server, ToolDef, ToolError, ToolOutput, Toolset};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

pk_cli_mcp::export_wasm!(demo);

/// The per-call config: `base` (the URL the fetches go to) and
/// `es256_key_hex` (a PKCS#8 DER P-256 key, hex).
pub fn demo(config: &Value) -> Demo {
    let text = |k: &str| config.get(k).and_then(Value::as_str).map(String::from);
    Demo {
        base: text("base"),
        key_hex: text("es256_key_hex"),
    }
}

pub struct Demo {
    base: Option<String>,
    key_hex: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FetchArgs {
    /// The path under the configured base, e.g. "items".
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SignArgs {
    /// The text to sign.
    pub message: String,
}

impl Toolset for Demo {
    fn server(&self) -> Server {
        Server::new("demo", env!("CARGO_PKG_VERSION"), "Demo", "demo")
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef::read(
                "demo_fetch",
                "Fetch",
                "Fetches a path under the base. Use it to test fetch. Returns status and body.",
            )
            .input::<FetchArgs>(),
            ToolDef::read(
                "demo_sign",
                "Sign",
                "Signs a message with ES256. Use it to test WebCrypto. Returns the signature hex.",
            )
            .input::<SignArgs>(),
            ToolDef::read(
                "demo_both",
                "Both",
                "Fetches two paths at once. Use it to test concurrency. Returns both statuses.",
            ),
            ToolDef::read(
                "demo_random",
                "Random",
                "Reads the clock and random source. Use it to test them. Returns both.",
            ),
            ToolDef::read(
                "demo_panic",
                "Panic",
                "Panics. Use it to test a trap. Returns nothing at all.",
            ),
        ]
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        match name {
            "demo_fetch" => {
                let a: FetchArgs = parse_args(args)?;
                let resp = HostTransport.send(self.request(&a.path)?).await?;
                ToolOutput::new(&json!({"status": resp.status, "body": resp.body}))
            }
            "demo_sign" => {
                let a: SignArgs = parse_args(args)?;
                let key = self
                    .key_hex
                    .as_deref()
                    .map(unhex)
                    .ok_or_else(|| CliError::Auth("no es256_key_hex configured".into()))?;
                let sig = HostTransport
                    .sign_es256(&key, a.message.as_bytes())
                    .await
                    .ok_or_else(|| ToolError::other("the host has no ES256 signer"))??;
                ToolOutput::new(&json!({"signature_hex": hex(&sig)}))
            }
            "demo_both" => {
                let (a, b) = join(
                    HostTransport.send(self.request("one")?),
                    HostTransport.send(self.request("two")?),
                )
                .await;
                ToolOutput::new(&json!({"one": a?.status, "two": b?.status}))
            }
            "demo_random" => {
                let mut bytes = [0u8; 16];
                fill_random(&mut bytes);
                ToolOutput::new(&json!({"random_hex": hex(&bytes), "now_ms": now_ms()}))
            }
            "demo_panic" => panic!("demo_panic was called"),
            other => Err(ToolError::other(format!("no handler for {other}"))),
        }
    }
}

impl Demo {
    fn request(&self, path: &str) -> Result<HttpRequest, CliError> {
        let base = self
            .base
            .as_deref()
            .ok_or_else(|| CliError::Auth("no base configured".into()))?;
        Ok(HttpRequest::get(format!("{base}/{path}"))
            .query(&[("q", "a b".into())])
            .header("X-Demo", "1"))
    }
}

/// Both futures to completion, polled together, so both host operations are
/// in flight at once.
async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ra, mut rb) = (None, None);
    poll_fn(|cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        match (ra.take(), rb.take()) {
            (Some(x), Some(y)) => Poll::Ready((x, y)),
            (x, y) => {
                (ra, rb) = (x, y);
                Poll::Pending
            }
        }
    })
    .await
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pk_cli_core::transport::HttpResponse;
    use pk_cli_mcp::wasm::testing::{call, Asked, Reply};

    #[test]
    fn both_fetches_are_in_flight_before_either_is_answered() {
        let mut asked = Vec::new();
        let out = call(
            demo,
            json!({"name": "demo_both"}),
            json!({"base": "https://demo.test"}),
            |a| {
                asked.push(a);
                Reply::Response(HttpResponse {
                    status: 200 + asked.len() as u16,
                    body: String::new(),
                })
            },
        );
        assert_eq!(
            out["result"]["structuredContent"],
            json!({"one": 201, "two": 202})
        );
        let urls: Vec<String> = asked
            .iter()
            .map(|a| match a {
                Asked::Fetch(r) => r.url.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            urls,
            [
                "https://demo.test/one?q=a%20b",
                "https://demo.test/two?q=a%20b"
            ]
        );
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex(&hex(&[0, 15, 255])), vec![0, 15, 255]);
    }
}
