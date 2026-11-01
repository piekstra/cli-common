//! coinbase-cli's MCP tools as a WebAssembly module for the Worker shell.
//!
//! The JS side sees exactly the agreed boundary: `tools()` returns the
//! `tools/list` entries as a JSON string, and `call(name, args, config)`
//! resolves to a `CallToolResult` JSON string. Everything Coinbase-specific —
//! signing, paging, parsing, error mapping — is coinbase-cli's own code; this
//! file only supplies the one thing a Worker does differently, `fetch`.

use std::cell::RefCell;

use cbase::client::Coinbase;
use cbase::credential::Credential;
use cbase::http::{HttpRequest, HttpResponse, Transport};
use cbase::jwt::Signer;
use cbase::mcp;
use pk_cli_core::CliError;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

thread_local! {
    /// The last key this isolate parsed, with its WebCrypto import. An
    /// isolate serves many requests; re-parsing the secret on each one costs
    /// a P-256 scalar multiplication (deriving the public key) and an
    /// `importKey`. Keyed on the secret's text, so rotating the secret simply
    /// misses the cache.
    static KEY_CACHE: RefCell<Option<CachedKey>> = const { RefCell::new(None) };
}

struct CachedKey {
    blob: String,
    cred: Credential,
    es256: Option<web_sys::CryptoKey>,
}

fn cached_credential(blob: &str) -> Result<(Credential, Option<web_sys::CryptoKey>), CliError> {
    if let Some(hit) = KEY_CACHE.with_borrow(|c| {
        c.as_ref()
            .filter(|k| k.blob == blob)
            .map(|k| (k.cred.clone(), k.es256.clone()))
    }) {
        return Ok(hit);
    }
    let cred = Credential::parse(blob)?;
    KEY_CACHE.set(Some(CachedKey {
        blob: blob.to_string(),
        cred: cred.clone(),
        es256: None,
    }));
    Ok((cred, None))
}

fn remember_es256(key: &web_sys::CryptoKey) {
    KEY_CACHE.with_borrow_mut(|c| {
        if let Some(k) = c.as_mut() {
            k.es256 = Some(key.clone());
        }
    });
}

/// Sends requests through the Worker's global `fetch` (via reqwest's wasm32
/// backend), and signs ES256 tokens with WebCrypto.
struct FetchTransport {
    client: reqwest::Client,
    webcrypto: bool,
    /// The imported key, reused for every page of one tool call.
    es256_key: RefCell<Option<web_sys::CryptoKey>>,
}

fn subtle() -> Result<web_sys::SubtleCrypto, CliError> {
    js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))
        .ok()
        .and_then(|c| c.dyn_into::<web_sys::Crypto>().ok())
        .map(|c| c.subtle())
        .ok_or_else(|| CliError::Other("WebCrypto is not available".into()))
}

fn js_err(what: &str, e: JsValue) -> CliError {
    CliError::Other(format!(
        "{what}: {}",
        e.as_string().unwrap_or_else(|| format!("{e:?}"))
    ))
}

impl FetchTransport {
    /// ECDSA P-256 / SHA-256 through `crypto.subtle`. WebCrypto's ECDSA
    /// output is IEEE P1363 `r || s`, which is exactly what JWS wants.
    async fn sign_es256(&self, signer: &Signer, input: &[u8]) -> Result<Vec<u8>, CliError> {
        let subtle = subtle()?;
        let cached = self.es256_key.borrow().clone();
        let key = match cached {
            Some(k) => k,
            None => {
                let der = signer
                    .es256_pkcs8_der()
                    .ok_or_else(|| CliError::Other("not an EC key".into()))?;
                let params = web_sys::EcKeyImportParams::new("ECDSA");
                params.set_named_curve("P-256");
                let usages = js_sys::Array::of1(&JsValue::from_str("sign"));
                let promise = subtle
                    .import_key_with_object(
                        "pkcs8",
                        &js_sys::Uint8Array::from(der.as_slice()),
                        &params,
                        false,
                        &usages,
                    )
                    .map_err(|e| js_err("importKey", e))?;
                let key: web_sys::CryptoKey = JsFuture::from(promise)
                    .await
                    .map_err(|e| js_err("importKey", e))?
                    .unchecked_into();
                *self.es256_key.borrow_mut() = Some(key.clone());
                remember_es256(&key);
                key
            }
        };
        let params = web_sys::EcdsaParams::new_with_str("ECDSA", "SHA-256");
        let promise = subtle
            .sign_with_object_and_u8_array(&params, &key, input)
            .map_err(|e| js_err("sign", e))?;
        let sig = JsFuture::from(promise)
            .await
            .map_err(|e| js_err("sign", e))?;
        Ok(js_sys::Uint8Array::new(&sig).to_vec())
    }
}

impl Transport for FetchTransport {
    async fn sign(&self, signer: &Signer, input: &[u8]) -> Option<Result<Vec<u8>, CliError>> {
        match signer {
            Signer::Es256(_) if self.webcrypto => Some(self.sign_es256(signer, input).await),
            // Ed25519 stays in Rust: it is ~10x cheaper than P-256 there, and
            // WebCrypto Ed25519 support varies by runtime.
            _ => None,
        }
    }

    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, CliError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| CliError::Other(format!("bad HTTP method: {e}")))?;
        let mut rb = self.client.request(method, &req.url).query(&req.query);
        for (k, v) in &req.headers {
            rb = rb.header(*k, v);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| CliError::Upstream(format!("fetch failed: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }
}

/// The `tools/list` entries, as JSON.
#[wasm_bindgen]
pub fn tools() -> String {
    mcp::tools().to_string()
}

/// Run one tool. `config` is `{"credential": "<CDP key JSON>", "base_url":
/// "https://api.coinbase.com"}` (plus `"webcrypto": false` to sign in Rust,
/// for benchmarks); the shell builds it from its secret bindings on every
/// call, so nothing secret is kept in the module between requests.
#[wasm_bindgen]
pub async fn call(name: String, args: String, config: String) -> String {
    run(&name, &args, &config).await.to_string()
}

async fn run(name: &str, args: &str, config: &str) -> Value {
    let config: Value = serde_json::from_str(config).unwrap_or(Value::Null);
    let Some(blob) = config.get("credential").and_then(Value::as_str) else {
        return mcp::error_result(&CliError::Auth(
            "no Coinbase CDP API key is configured — set the CBASE_CDP_KEY secret on the Worker"
                .into(),
        ));
    };
    let (cred, es256_key) = match cached_credential(blob) {
        Ok(c) => c,
        Err(e) => return mcp::error_result(&e),
    };
    let base_url = config
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or(cbase::config::DEFAULT_BASE_URL);
    let args: Value = match serde_json::from_str(args) {
        Ok(v @ Value::Object(_)) => v,
        Ok(Value::Null) | Err(_) if args.trim().is_empty() => json!({}),
        _ => {
            return mcp::error_result(&CliError::Usage(
                "tool arguments must be a JSON object".into(),
            ))
        }
    };
    let transport = FetchTransport {
        client: reqwest::Client::new(),
        webcrypto: config
            .get("webcrypto")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        es256_key: RefCell::new(es256_key),
    };
    let client = Coinbase::with_transport(cred, base_url, transport, false);
    mcp::call(&client, name, &args).await
}
