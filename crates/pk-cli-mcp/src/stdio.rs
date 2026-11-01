//! `<bin> mcp`: a [`Toolset`] served over stdio by the official Rust SDK
//! (`rmcp`), for Claude Code, Claude Desktop (`.mcpb`) and any MCP client
//! that launches a local server.
//!
//! A CLI adds the subcommand in a few lines:
//!
//! ```ignore
//! #[derive(Subcommand)]
//! enum Command {
//!     /// Serve this CLI's tools to Claude and other MCP clients over stdio.
//!     Mcp(pk_cli_mcp::McpArgs),
//!     // …
//! }
//!
//! Command::Mcp(args) => pk_cli_mcp::run(args, MyTools::new(cfg, creds)),
//! ```
//!
//! While serving, stdout carries the protocol and nothing else; diagnostics
//! go to stderr. Each call runs on Tokio's blocking pool through
//! [`crate::block_on`], outside any Tokio context, so a toolset may call
//! blocking provider code (`reqwest::blocking`) directly, and read-only calls
//! the client sends together run in parallel.

use std::sync::Arc;

use pk_cli_core::CliError;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult as RmcpResult, Implementation,
    ListToolsResult, PaginatedRequestParams, ResultType, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;

use crate::block_on::block_on;
use crate::toolset::{call_tool, instructions, tools_list, Toolset};

/// Arguments of the `mcp` subcommand.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct McpArgs {
    /// Print the tool definitions (the `tools/list` result) as JSON and exit
    /// instead of serving.
    #[arg(long)]
    pub list_tools: bool,
}

/// The `mcp` subcommand: serve `toolset` on stdio until the client closes
/// it, or with `--list-tools` print the definitions and exit.
pub fn run<T: Toolset + Send + Sync + 'static>(args: &McpArgs, toolset: T) -> Result<(), CliError> {
    if args.list_tools {
        let list = crate::toolset::tools_list_json(&toolset);
        let text = serde_json::to_string_pretty(&list)
            .map_err(|e| CliError::Other(format!("serializing the tool list: {e}")))?;
        println!("{text}");
        return Ok(());
    }
    serve_stdio(toolset)
}

/// Serve `toolset` on stdin/stdout until the client disconnects.
pub(crate) fn serve_stdio<T: Toolset + Send + Sync + 'static>(toolset: T) -> Result<(), CliError> {
    let adapter = Adapter::new(toolset)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Other(format!("starting the MCP runtime: {e}")))?;
    runtime.block_on(async move {
        let running = adapter
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| CliError::Other(format!("MCP session failed to start: {e}")))?;
        running
            .waiting()
            .await
            .map_err(|e| CliError::Other(format!("MCP session ended abnormally: {e}")))?;
        Ok(())
    })
}

/// The rmcp server for a toolset. Definitions and instructions are computed
/// once: the list is fixed for the life of the process.
struct Adapter<T> {
    toolset: Arc<T>,
    tools: Vec<Tool>,
    config: ServerConfig,
}

impl<T: Toolset + Send + Sync + 'static> Adapter<T> {
    fn new(toolset: T) -> Result<Self, CliError> {
        let tools = tools_list(&toolset)
            .into_iter()
            .map(serde_json::from_value::<Tool>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CliError::Other(format!("a tool definition rmcp rejects: {e}")))?;
        let server = toolset.server();
        let mut identity = Implementation::new(&server.bin, &server.version);
        identity.title = Some(server.service.clone());
        let config = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(identity)
            .with_instructions(instructions(&toolset));
        Ok(Self {
            toolset: Arc::new(toolset),
            tools,
            config,
        })
    }
}

impl<T: Toolset + Send + Sync + 'static> ServerHandler for Adapter<T> {
    fn get_info(&self) -> ServerConfig {
        self.config.clone()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let toolset = Arc::clone(&self.toolset);
        let name = request.name.to_string();
        let arguments = request.arguments.map(Value::Object);
        let outcome = tokio::task::spawn_blocking(move || {
            block_on(call_tool(toolset.as_ref(), &name, arguments))
        })
        .await
        .map_err(|e| McpError::internal_error(format!("the tool call panicked: {e}"), None))?;
        match outcome {
            Ok(result) => {
                let mut result =
                    serde_json::from_value::<RmcpResult>(result.to_json()).map_err(|e| {
                        McpError::internal_error(format!("encoding the result: {e}"), None)
                    })?;
                // As rmcp's own constructors do; it clears the field for a
                // peer on a protocol version older than 2026-07-28.
                result.result_type = Some(ResultType::COMPLETE);
                Ok(CallToolResponse::Complete(result))
            }
            Err(unknown) => Err(McpError::invalid_params(unknown.to_string(), None)),
        }
    }
}
