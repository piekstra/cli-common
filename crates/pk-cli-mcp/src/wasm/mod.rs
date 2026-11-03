//! A [`Toolset`] as a WebAssembly module for the deploy-your-own Worker
//! (feature `wasm`; DESIGN.md §1.9 path B).
//!
//! A CLI builds its library for `wasm32-unknown-unknown` with one line,
//!
//! ```ignore
//! pk_cli_mcp::export_wasm!(worker_tools);   // fn worker_tools(config: &Value) -> MyTools<…>
//! ```
//!
//! and releases the `.wasm`. The deploy-your-own Worker loads it with the
//! reference host, `js/pk-host.ts` in this crate, and serves its tools to
//! claude.ai. Provider requests go through [`HostTransport`]: the host's
//! `fetch` behind `pk_cli_core::transport::Transport` and its WebCrypto
//! behind `pk_cli_core::transport::PlatformSigner`, so the provider client
//! the native binary uses runs here unchanged.
//!
//! # Why a raw ABI and not `wasm-bindgen`
//!
//! `wasm-bindgen` generates JavaScript glue that must match its `.wasm`
//! byte for byte, so a release would have to ship executable glue beside
//! every module and the Worker would import each module's own copy. With
//! this ABI a module is one self-contained `.wasm` file, pinned by its
//! SHA-256; one host implementation serves every module; and a module's
//! only imports are the five `pk_host` functions below, which the host
//! checks before running it. The cost is that nothing in a module's
//! dependency graph may use `wasm-bindgen` (`getrandom`'s `wasm_js` backend,
//! `reqwest`'s wasm32 client): randomness and time come from [`fill_random`]
//! and [`now_ms`] instead.
//!
//! # ABI version 1
//!
//! Every integer is a wasm `i32`; pointers and lengths address the module's
//! exported `memory`. Strings are UTF-8; structured values are JSON.
//!
//! **The module imports** (module name `pk_host`):
//!
//! | Import | Does |
//! |---|---|
//! | `now_ms() -> f64` | `Date.now()` |
//! | `random(ptr, len)` | `crypto.getRandomValues` into the buffer |
//! | `fetch(op, ptr, len)` | starts a request: `{"method", "url", "headers": [[name, value], …], "body": string \| null}`, sent without following redirects or keeping cookies. Answered later with `pk_complete(op, http_status, answer)`, where `answer` is the length of the response headers' JSON (`u32`, little-endian), that JSON (`[[name, value], …]`) and then the body; or status `-1` and a message when no HTTP answer came |
//! | `sign_es256(op, key_ptr, key_len, data_ptr, data_len)` | starts an ECDSA P-256 / SHA-256 signature with the PKCS#8 DER key. Answered with status `0` and the 64-byte `r‖s`, `-1` and a message, or `-2` when the host has no WebCrypto ECDSA (the module then signs itself) |
//! | `log(level, ptr, len)` | a line for the host's log: `0` error, `1` warn, `2` info |
//!
//! An import copies what it reads before returning and never calls back
//! into the module.
//!
//! **The module exports:**
//!
//! | Export | Does |
//! |---|---|
//! | `pk_abi_version() -> u32` | `1`; the host refuses a module whose version it does not speak |
//! | `pk_alloc(len) -> ptr`, `pk_free(ptr, len)` | buffers the host fills; every buffer passed into the module was allocated here, and the module owns it from then on |
//! | `pk_describe() -> len` | `{"abi", "server": serverInfo, "instructions", "tools": […]}`, the bytes at `pk_out_ptr()` |
//! | `pk_call(params_ptr, params_len, config_ptr, config_len) -> task` | starts a `tools/call`. `params` is the call's params object (`{"name", "arguments"}`), `config` the host's per-call JSON the CLI's factory reads (credentials from the Worker's secrets) |
//! | `pk_poll(task) -> len` | `-1` while the task waits on the host; otherwise `{"result": CallToolResult}` or `{"error": {"code", "message"}}` (a JSON-RPC error: an unknown tool), the bytes at `pk_out_ptr()`, and the task is gone |
//! | `pk_complete(op, status, ptr, len)` | the host's answer to an operation |
//! | `pk_drop(task)` | abandons a task; its operations' late answers are discarded |
//! | `pk_out_ptr() -> ptr` | where the last `pk_describe`/`pk_poll` answer is, valid until the next call into the module |
//!
//! The host drives a call: `pk_call`, then `pk_poll`; while it answers `-1`,
//! wait for one of the operations that task started during its polls to
//! settle, deliver it with `pk_complete`, and poll again. Operations a poll
//! starts belong to the task being polled, because a module only runs inside
//! the host's own call into it. Several calls may be in flight at once. A
//! module panics by trapping, after logging the message at level 0; the host
//! then discards the instance and starts a fresh one for the next call.
//!
//! Every tool description, instruction and auth remedy a module sends says
//! where it runs: `pk_describe` and `pk_call` replace the local host's wording
//! (credentials on the user's machine, `<bin> auth login` in a terminal) with
//! the Worker's (the credentials are Worker secrets).

mod host;
mod runtime;
#[cfg(feature = "test-util")]
pub mod testing;
mod transport;

use serde_json::Value;

pub use host::{fill_random, log, now_ms, Level};
pub use runtime::{STATUS_FAILED, STATUS_UNSUPPORTED};
pub use transport::HostTransport;

use crate::error::ToolError;
use crate::result::ToolOutput;
use crate::tool::ToolDef;
use crate::toolset::{Server, Toolset};

/// The ABI version a module built with this crate speaks.
pub const ABI_VERSION: u32 = 1;

/// A toolset as the Worker serves it: the same tools, with the instructions
/// and auth remedies saying the credentials are the Worker's secrets.
pub(crate) struct InWorker<T>(pub(crate) T);

impl<T: Toolset> Toolset for InWorker<T> {
    fn server(&self) -> Server {
        let server = self.0.server();
        let note = format!(
            "through the open-source `{}` code running in the user's own Cloudflare Worker, \
             which holds the {} credentials as Worker secrets",
            server.bin, server.service
        );
        let remedy = format!(
            "ask the user to update the {} credentials in their Cloudflare Worker's secrets",
            server.service
        );
        server.hosted(note, remedy)
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.0.tools()
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        self.0.call(name, args).await
    }
}

/// The functions behind [`export_wasm!`](crate::export_wasm)'s exports. Not for direct use.
#[doc(hidden)]
pub mod abi {
    use super::*;

    /// A zeroed buffer of `len` bytes, owned by whoever holds the pointer.
    pub fn alloc(len: usize) -> *mut u8 {
        Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
    }

    /// Take back a buffer from [`alloc`].
    ///
    /// # Safety
    /// `ptr` and `len` must be exactly what one [`alloc`] call returned and
    /// was given, and the buffer must not be used afterwards.
    pub unsafe fn take(ptr: *mut u8, len: usize) -> Vec<u8> {
        // SAFETY: per the contract, this is the boxed slice `alloc` leaked.
        unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) }.into_vec()
    }

    /// # Safety
    /// As for [`take`].
    pub unsafe fn free(ptr: *mut u8, len: usize) {
        // SAFETY: forwarded contract.
        drop(unsafe { take(ptr, len) });
    }

    pub fn out_ptr() -> *const u8 {
        super::runtime::out_ptr()
    }

    pub fn describe<T: Toolset, F: Fn(&Value) -> T>(factory: F) -> i32 {
        install_panic_hook();
        let answer = super::runtime::describe(&factory(&Value::Null));
        hand_out(&answer)
    }

    /// # Safety
    /// Both buffers must come from [`alloc`], as for [`take`].
    pub unsafe fn call<T, F>(
        params: *mut u8,
        params_len: usize,
        config: *mut u8,
        config_len: usize,
        factory: F,
    ) -> u32
    where
        T: Toolset + 'static,
        F: Fn(&Value) -> T,
    {
        install_panic_hook();
        // SAFETY: forwarded contract.
        let (params, config) = unsafe { (take(params, params_len), take(config, config_len)) };
        super::runtime::spawn_call(&params, &config, factory)
    }

    pub fn poll(task: u32) -> i32 {
        match super::runtime::poll(task) {
            Some(answer) => hand_out(&answer),
            None => -1,
        }
    }

    /// # Safety
    /// The buffer must come from [`alloc`], as for [`take`].
    pub unsafe fn complete(op: u32, status: i32, data: *mut u8, len: usize) {
        // SAFETY: forwarded contract.
        let data = unsafe { take(data, len) };
        super::runtime::complete(op, status, data);
    }

    pub fn drop_task(task: u32) {
        super::runtime::drop_task(task);
    }

    fn hand_out(answer: &Value) -> i32 {
        let len = super::runtime::hand_out(answer.to_string().into_bytes());
        i32::try_from(len).unwrap_or(i32::MAX)
    }

    /// A panic traps the module; the message goes to the host's log first,
    /// where the Worker's logs keep it.
    fn install_panic_hook() {
        #[cfg(target_arch = "wasm32")]
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                std::panic::set_hook(Box::new(|info| {
                    super::host::log(super::Level::Error, &info.to_string());
                }));
            });
        }
    }
}

/// Export a toolset as a WebAssembly module speaking [ABI version
/// 1](self#abi-version-1).
///
/// `$factory` is a function (or closure without captures) from the host's
/// per-call config JSON to the toolset. It is also called with `null` to
/// describe the tools, so it must not fail or touch the network: a missing or
/// unusable credential is an auth error from the tool call, not from the
/// factory.
///
/// Invoke it once, in the crate built as the `cdylib`, behind the CLI's own
/// `wasm` feature:
///
/// ```ignore
/// #[cfg(feature = "wasm")]
/// pk_cli_mcp::export_wasm!(worker::tools);
/// ```
///
/// and build with `cargo rustc --lib --crate-type cdylib --release --target
/// wasm32-unknown-unknown --no-default-features --features wasm`.
#[macro_export]
macro_rules! export_wasm {
    ($factory:expr) => {
        /// ABI: the version this module speaks.
        #[no_mangle]
        pub extern "C" fn pk_abi_version() -> u32 {
            $crate::wasm::ABI_VERSION
        }

        /// ABI: a buffer for the host to fill.
        #[no_mangle]
        pub extern "C" fn pk_alloc(len: usize) -> *mut u8 {
            $crate::wasm::abi::alloc(len)
        }

        /// ABI: release a buffer from `pk_alloc` the host did not pass in.
        ///
        /// # Safety
        /// `ptr` and `len` are one `pk_alloc` call's.
        #[no_mangle]
        pub unsafe extern "C" fn pk_free(ptr: *mut u8, len: usize) {
            // SAFETY: the ABI's contract with the host.
            unsafe { $crate::wasm::abi::free(ptr, len) }
        }

        /// ABI: where the last answer is.
        #[no_mangle]
        pub extern "C" fn pk_out_ptr() -> *const u8 {
            $crate::wasm::abi::out_ptr()
        }

        /// ABI: identity, instructions and tools.
        #[no_mangle]
        pub extern "C" fn pk_describe() -> i32 {
            $crate::wasm::abi::describe($factory)
        }

        /// ABI: start a `tools/call`.
        ///
        /// # Safety
        /// Both buffers come from `pk_alloc`; the module takes them.
        #[no_mangle]
        pub unsafe extern "C" fn pk_call(
            params: *mut u8,
            params_len: usize,
            config: *mut u8,
            config_len: usize,
        ) -> u32 {
            // SAFETY: the ABI's contract with the host.
            unsafe { $crate::wasm::abi::call(params, params_len, config, config_len, $factory) }
        }

        /// ABI: poll a call.
        #[no_mangle]
        pub extern "C" fn pk_poll(task: u32) -> i32 {
            $crate::wasm::abi::poll(task)
        }

        /// ABI: the host's answer to an operation.
        ///
        /// # Safety
        /// The buffer comes from `pk_alloc`; the module takes it.
        #[no_mangle]
        pub unsafe extern "C" fn pk_complete(op: u32, status: i32, data: *mut u8, len: usize) {
            // SAFETY: the ABI's contract with the host.
            unsafe { $crate::wasm::abi::complete(op, status, data, len) }
        }

        /// ABI: abandon a call.
        #[no_mangle]
        pub extern "C" fn pk_drop(task: u32) {
            $crate::wasm::abi::drop_task(task)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolset::{tools_list_json, Server};
    use pk_cli_core::transport::{HttpRequest, PlatformSigner, Transport};
    use pk_cli_core::CliError;
    use serde_json::json;

    /// One tool that fetches through the host, one that fails auth.
    struct Probe {
        base: Option<String>,
    }

    impl Toolset for Probe {
        fn server(&self) -> Server {
            Server::new("probe", "0.0.0", "Probe", "probe")
        }

        fn tools(&self) -> Vec<ToolDef> {
            vec![
                ToolDef::read("probe_get", "Get", "Gets. Always one page. Nothing else."),
                ToolDef::read("probe_auth", "Auth", "Fails auth. On purpose. Always."),
            ]
        }

        async fn call(&self, name: &str, _args: Value) -> Result<ToolOutput, ToolError> {
            let base = self
                .base
                .clone()
                .ok_or_else(|| CliError::Auth("no base configured".into()))?;
            match name {
                "probe_get" => {
                    let resp = HostTransport
                        .send(
                            HttpRequest::get(format!("{base}/x"))
                                .query(&[("q", "a b".into())])
                                .header("Accept", "application/json"),
                        )
                        .await?;
                    ToolOutput::new(&json!({"status": resp.status, "body": resp.body}))
                }
                _ => Err(CliError::Auth("the key was rejected".into()).into()),
            }
        }
    }

    fn probe(config: &Value) -> Probe {
        Probe {
            base: config.get("base").and_then(Value::as_str).map(String::from),
        }
    }

    fn read_out(len: i32) -> Value {
        let len = usize::try_from(len).expect("an answer, not pending");
        // SAFETY: `pk_out_ptr` points at `len` bytes until the next call.
        let bytes = unsafe { std::slice::from_raw_parts(abi::out_ptr(), len) };
        serde_json::from_slice(bytes).unwrap()
    }

    fn pass(bytes: &[u8]) -> (*mut u8, usize) {
        let ptr = abi::alloc(bytes.len());
        // SAFETY: `ptr` has room for exactly `bytes.len()` bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
        (ptr, bytes.len())
    }

    fn start(params: Value, config: Value) -> u32 {
        let (p, pl) = pass(params.to_string().as_bytes());
        let (c, cl) = pass(config.to_string().as_bytes());
        // SAFETY: both buffers come from `alloc`.
        unsafe { abi::call(p, pl, c, cl, probe) }
    }

    fn answer(op: u32, status: i32, data: &[u8]) {
        let (ptr, len) = pass(data);
        // SAFETY: the buffer comes from `alloc`.
        unsafe { abi::complete(op, status, ptr, len) };
    }

    /// An HTTP answer to a fetch, framed as the ABI says.
    fn respond(op: u32, status: i32, body: &[u8]) {
        answer(op, status, &transport::encode_response(&[], body));
    }

    fn fetch_op() -> (u32, Value) {
        match &host::take_started()[..] {
            [host::Started::Fetch { op, request }] => {
                (*op, serde_json::from_slice(request).unwrap())
            }
            other => panic!("expected one fetch, got {other:?}"),
        }
    }

    #[test]
    fn describe_sends_the_worker_wording_and_the_tools() {
        let d = read_out(abi::describe(probe));
        assert_eq!(d["abi"], ABI_VERSION);
        assert_eq!(d["server"]["name"], "probe");
        assert_eq!(d["tools"], tools_list_json(&Probe { base: None })["tools"]);
        let text = d["instructions"].as_str().unwrap();
        assert!(
            text.contains("running in the user's own Cloudflare Worker"),
            "{text}"
        );
        assert!(!text.contains("auth login"), "{text}");
    }

    #[test]
    fn a_call_waits_for_the_host_and_then_answers() {
        let task = start(
            json!({"name": "probe_get", "arguments": {}}),
            json!({"base": "https://api.example.test"}),
        );
        assert_eq!(abi::poll(task), -1);
        let (op, request) = fetch_op();
        assert_eq!(request["method"], "GET");
        assert_eq!(request["url"], "https://api.example.test/x?q=a%20b");
        assert_eq!(request["headers"], json!([["Accept", "application/json"]]));
        assert_eq!(request["body"], Value::Null);
        // Polling again before the answer changes nothing.
        assert_eq!(abi::poll(task), -1);
        assert!(host::take_started().is_empty());
        respond(op, 201, b"made");
        let out = read_out(abi::poll(task));
        assert_eq!(out["result"]["isError"], false);
        assert_eq!(
            out["result"]["structuredContent"],
            json!({"status": 201, "body": "made"})
        );
        assert_eq!(runtime::live(), (0, 0), "the task and its op are gone");
    }

    #[test]
    fn calls_interleave_and_a_failed_fetch_is_an_upstream_error() {
        let get = json!({"name": "probe_get"});
        let cfg = json!({"base": "https://h.test"});
        let a = start(get.clone(), cfg.clone());
        let b = start(get, cfg);
        assert_eq!(abi::poll(a), -1);
        let (op_a, _) = fetch_op();
        assert_eq!(abi::poll(b), -1);
        let (op_b, _) = fetch_op();
        answer(op_b, STATUS_FAILED, b"TypeError: fetch failed");
        let out_b = read_out(abi::poll(b));
        assert_eq!(out_b["result"]["isError"], true);
        let text = out_b["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with(
                "upstream: the request to https://h.test failed: TypeError: fetch failed."
            ),
            "{text}"
        );
        respond(op_a, 200, b"");
        assert_eq!(read_out(abi::poll(a))["result"]["isError"], false);
    }

    #[test]
    fn auth_errors_name_the_worker_secrets_not_a_terminal() {
        let task = start(
            json!({"name": "probe_auth"}),
            json!({"base": "https://h.test"}),
        );
        let out = read_out(abi::poll(task));
        let text = out["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(
                "Ask the user to update the Probe credentials in their Cloudflare Worker's secrets"
            ),
            "{text}"
        );
        assert!(!text.contains("auth login"), "{text}");
        let task = start(json!({"name": "probe_get"}), Value::Null);
        let out = read_out(abi::poll(task));
        assert!(out["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("auth: no base configured."));
    }

    #[test]
    fn protocol_errors_come_back_as_json_rpc_errors() {
        let task = start(json!({"name": "probe_nope"}), Value::Null);
        assert_eq!(read_out(abi::poll(task))["error"]["code"], -32602);
        let (p, pl) = pass(b"not json");
        let (c, cl) = pass(b"{}");
        // SAFETY: both buffers come from `alloc`.
        let task = unsafe { abi::call(p, pl, c, cl, probe) };
        let out = read_out(abi::poll(task));
        assert_eq!(out["error"]["code"], -32603);
        assert!(out["error"]["message"]
            .as_str()
            .unwrap()
            .contains("params are not JSON"));
    }

    #[test]
    fn a_dropped_task_discards_the_late_answer() {
        let task = start(
            json!({"name": "probe_get"}),
            json!({"base": "https://h.test"}),
        );
        assert_eq!(abi::poll(task), -1);
        let (op, _) = fetch_op();
        abi::drop_task(task);
        respond(op, 200, b"late");
        assert_eq!(runtime::live(), (0, 0));
    }

    #[test]
    fn the_platform_signer_answers_or_steps_aside() {
        use pk_cli_core::block_on;
        use std::future::Future;
        // Unsupported: the caller signs itself.
        let fut = HostTransport.sign_es256(b"der", b"msg");
        let mut fut = std::pin::pin!(fut);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        let op = match &host::take_started()[..] {
            [host::Started::SignEs256 { op, .. }] => *op,
            other => panic!("{other:?}"),
        };
        answer(op, STATUS_UNSUPPORTED, b"");
        assert!(block_on(fut).is_none());

        for (status, data, ok) in [
            (0, vec![7u8; 64], true),
            (0, vec![7u8; 70], false),
            (STATUS_FAILED, b"bad key".to_vec(), false),
        ] {
            let fut = HostTransport.sign_es256(b"der", b"msg");
            let mut fut = std::pin::pin!(fut);
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            let op = match &host::take_started()[..] {
                [host::Started::SignEs256 { op, .. }] => *op,
                other => panic!("{other:?}"),
            };
            answer(op, status, &data);
            let got = block_on(fut).expect("an answer");
            assert_eq!(got.is_ok(), ok, "{status} {}", data.len());
        }
    }

    #[test]
    fn buffers_round_trip_through_alloc_and_take() {
        let (ptr, len) = pass(b"hello");
        // SAFETY: from `alloc`, taken once.
        assert_eq!(unsafe { abi::take(ptr, len) }, b"hello");
        let (ptr, len) = pass(b"");
        // SAFETY: as above; a zero-length buffer is still a valid one.
        unsafe { abi::free(ptr, len) };
    }
}
