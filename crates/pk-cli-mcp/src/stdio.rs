//! `<bin> mcp`: a [`Toolset`] served over stdio by the official Rust SDK
//! (`rmcp`), for Claude Code, Claude Desktop (`.mcpb`) and any MCP client
//! that launches a local server.
//!
//! While serving, stdout carries the protocol and nothing else; diagnostics
//! go to stderr.

use pk_cli_core::CliError;
use rmcp::ServiceExt;

use crate::adapter::Adapter;
use crate::toolset::Toolset;

/// Serve `toolset` on stdin/stdout until the client disconnects.
pub(crate) fn serve<T: Toolset + Send + Sync + 'static>(toolset: T) -> Result<(), CliError> {
    let adapter = Adapter::new(toolset, None)?;
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
