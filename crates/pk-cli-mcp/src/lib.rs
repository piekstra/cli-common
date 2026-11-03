//! MCP tools for the piekstra CLI family (DESIGN.md, "MCP profile").
//!
//! A family CLI exposes a few workflow-shaped tools over its existing command
//! logic, so anyone can connect Claude to their own account through the
//! open-source CLI, with no shared hosted service holding anyone's
//! credentials. Two ways to run the same tools:
//!
//! - **Local:** `<bin> mcp` serves them over stdio (feature `stdio`), for
//!   Claude Code, Claude Desktop (an `.mcpb` bundle) and any MCP client that
//!   launches a local server. Credentials stay in the user's OS keychain, or
//!   arrive as environment variables the client stores for them.
//! - **Deploy-your-own Worker:** the core of this crate builds for
//!   `wasm32-unknown-unknown`, so a CLI's toolset can run inside a
//!   Cloudflare Worker in the user's own account, reached from claude.ai on
//!   the web and mobile.
//!
//! # The core (default features)
//!
//! No Tokio, no keyring, no processes, no blocking I/O: it builds for
//! `wasm32-unknown-unknown` (`make wasm-check`, and CI, prove it).
//!
//! - [`ToolDef`]: a tool's name, title, description, input and output JSON
//!   Schemas (from `schemars` derives), annotations and `_meta`.
//! - [`ToolOutput`] / [`CallToolResult`]: structured JSON plus the same JSON
//!   as compact text.
//! - [`Toolset`]: the trait a CLI implements: async calls with no `Send`
//!   bound, tools in a stable order. The provider sits behind a per-target
//!   seam the toolset awaits (blocking natively, `fetch` in a Worker); see
//!   the trait's docs.
//! - [`shape`]: limits (default 25, max 200), opaque cursors,
//!   [`ResponseFormat`] and the per-result size budget that truncates a list
//!   with a hint on how to narrow.
//! - [`ToolError`]: the family exit codes as actionable tool errors. An auth
//!   failure names `<bin> auth login`, to be run in a terminal: a tool never
//!   signs in.
//! - [`instructions`]: the standard server instructions, including that
//!   free-text fields from third parties are data, not instructions.
//!
//! # The WASM boundary is MCP-shaped JSON
//!
//! A host (the stdio server here, a Worker shell elsewhere) owns the
//! transport and the JSON-RPC envelope; the toolset only ever sees and
//! returns MCP JSON. The contract is four functions, every value plain
//! `serde_json::Value`:
//!
//! | Host needs | Call | Returns |
//! |---|---|---|
//! | `tools/list` result | [`tools_list_json`] | `{"tools": [Tool, …]}`, MCP `Tool` objects in a stable order |
//! | `tools/call` | [`call_tool_json`]`(&toolset, &params).await` | `Ok(CallToolResult)` JSON: `{"content":[{"type":"text","text":…}],"structuredContent":{…},"isError":bool}`; `Err` a JSON-RPC error object (`-32602`) for an unknown tool or params without a name |
//! | `serverInfo` | [`Server::server_info_json`] | `{"name", "title", "version"}` |
//! | `instructions` | [`instructions`] | the server instructions string; its two host-specific sentences (where credentials live, how to sign in) come from [`Server`], local by default, both replaced at once with [`Server::hosted`] |
//!
//! Tool failures (bad arguments, auth, not found, upstream, an oversized
//! result) are `CallToolResult`s with `isError: true` and text saying how to
//! fix the call; only an unknown tool name is a protocol error. Envelope
//! fields that depend on the negotiated protocol version, such as
//! 2026-07-28's `resultType`, `ttlMs`/`cacheScope` and `_meta` server
//! identity, are the host's to add. The future [`call_tool_json`] returns is
//! not `Send`; a single-threaded host awaits it directly.
//!
//! # Features
//!
//! - `stdio` (native): `run` and `McpArgs`, the `<bin> mcp` subcommand
//!   on the official Rust SDK (`rmcp`).
//! - `http` (native, includes `stdio`): `<bin> mcp --http --port <PORT>`,
//!   the same tools over stateless Streamable HTTP on 127.0.0.1 behind a
//!   bearer token, read-only unless `--allow-writes`, for a tunnel from the
//!   user's own Cloudflare Worker (the connector's "home" modules); and
//!   `<bin> mcp --new-token`, which makes that token. The server binds
//!   127.0.0.1 only, refuses to start without a token of at least 32
//!   visible characters (`<BIN>_MCP_TOKEN`, else the keychain item
//!   `mcp-http-token`), refuses any `Host` but loopback and any request
//!   with an `Origin`, sends no CORS headers, and answers a call still
//!   running after 120 s with an error.
//! - `wasm`: [`wasm`] and [`export_wasm!`], the toolset as a WebAssembly
//!   module for the deploy-your-own Worker on a raw ABI (one `.wasm`, no
//!   JavaScript glue; the reference host is `js/pk-host.ts`), and
//!   [`wasm::HostTransport`], the host's `fetch` and WebCrypto behind
//!   `pk_cli_core::transport::Transport`.
//! - `test-util`: `conformance`, the checks every CLI's tests run, and
//!   `block_on` for calling a toolset synchronously; with `wasm`,
//!   `wasm::testing`, which runs a Worker toolset natively with the test
//!   playing the host.

pub mod error;
pub mod result;
pub mod shape;
pub mod tool;
pub mod toolset;

#[cfg(feature = "stdio")]
mod adapter;
#[cfg(feature = "stdio")]
mod command;
#[cfg(any(feature = "test-util", test))]
pub mod conformance;
#[cfg(feature = "http")]
mod http;
#[cfg(feature = "stdio")]
mod stdio;
#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "stdio")]
pub use command::{run, McpArgs};
pub use error::{ErrorKind, ToolError};
#[cfg(feature = "http")]
pub use http::restart_note;
/// `pk_cli_core::block_on`, re-exported for a CLI's tests.
#[cfg(feature = "test-util")]
pub use pk_cli_core::block_on;
pub use result::{CallToolResult, ToolOutput};
pub use shape::{ListArgs, Page, PageInfo, PageRequest, ResponseFormat};
pub use tool::{Annotations, ToolDef};
pub use toolset::{
    call_tool, call_tool_json, instructions, parse_args, tools_list, tools_list_json, Server,
    Toolset, UnknownTool,
};

/// `schemars`, re-exported for its traits. A CLI deriving `JsonSchema` on
/// its argument and result structs also depends on `schemars = "1"` itself
/// (the derive expands to `::schemars` paths); Cargo unifies the two.
pub use schemars;
