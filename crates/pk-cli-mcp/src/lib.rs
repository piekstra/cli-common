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
//!   bound, tools in a stable order.
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
//! | `instructions` | [`instructions`] | the server instructions string |
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
//! - `stdio` (native): [`run`] and [`McpArgs`], the `<bin> mcp` subcommand
//!   on the official Rust SDK (`rmcp`).
//! - `test-util`: [`conformance`], the checks every CLI's tests run, and
//!   [`block_on`] for calling a toolset synchronously.

pub mod error;
pub mod result;
pub mod shape;
pub mod tool;
pub mod toolset;

#[cfg(any(feature = "stdio", feature = "test-util", test))]
mod block_on;
#[cfg(feature = "test-util")]
pub mod conformance;
#[cfg(feature = "stdio")]
mod stdio;

#[cfg(any(feature = "stdio", feature = "test-util"))]
pub use block_on::block_on;
pub use error::{ErrorKind, ToolError};
pub use result::{CallToolResult, ToolOutput};
pub use shape::{ListArgs, Page, PageInfo, PageRequest, ResponseFormat};
#[cfg(feature = "stdio")]
pub use stdio::{run, serve_stdio, McpArgs};
pub use tool::{Annotations, ToolDef};
pub use toolset::{
    call_tool, call_tool_json, instructions, parse_args, tools_list, tools_list_json, Server,
    Toolset, UnknownTool,
};

/// `schemars`, re-exported for its traits. A CLI deriving `JsonSchema` on
/// its argument and result structs also depends on `schemars = "1"` itself
/// (the derive expands to `::schemars` paths); Cargo unifies the two.
pub use schemars;
