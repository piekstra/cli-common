//! `<bin> mcp`: the subcommand's arguments and its dispatch to a transport.
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

use pk_cli_core::CliError;

use crate::toolset::Toolset;

/// Arguments of the `mcp` subcommand.
///
/// With the `http` feature it also takes `--http`, `--port`,
/// `--allow-writes`, `--new-token` and `--overwrite`. Non-exhaustive, so its
/// shape is the same to other crates with the feature on or off: clap builds
/// it, or `McpArgs::default()` does.
#[derive(clap::Args, Debug, Clone, Default)]
#[non_exhaustive]
pub struct McpArgs {
    /// Print the tool definitions (the `tools/list` result) as JSON and exit
    /// instead of serving.
    #[arg(long)]
    pub list_tools: bool,

    /// Serve over HTTP on 127.0.0.1 instead of stdio, for a tunnel from your
    /// own Cloudflare Worker. Needs --port and a bearer token
    /// (--new-token makes one). Serves the read-only tools only, unless
    /// --allow-writes.
    #[cfg(feature = "http")]
    #[arg(long, conflicts_with_all = ["list_tools", "new_token"])]
    pub http: bool,

    /// The port --http listens on, on 127.0.0.1 only.
    #[cfg(feature = "http")]
    #[arg(long, value_name = "PORT", requires = "http")]
    pub port: Option<u16>,

    /// With --http, also serve the tools that change data. Each call still
    /// needs the user's approval in the MCP client.
    #[cfg(feature = "http")]
    #[arg(long, requires = "http")]
    pub allow_writes: bool,

    /// Make a bearer token for --http, save it in the OS keychain, print it
    /// once for your Worker's secret, and exit.
    #[cfg(feature = "http")]
    #[arg(long, conflicts_with = "list_tools")]
    pub new_token: bool,

    /// With --new-token, replace the token already saved (the Worker's
    /// secret must then be updated too).
    #[cfg(feature = "http")]
    // `requires` alone lets `--http --overwrite` through: clap skips it when
    // the required flag conflicts with one present.
    #[arg(long, requires = "new_token", conflicts_with_all = ["http", "list_tools"])]
    pub overwrite: bool,
}

/// The `mcp` subcommand: serve `toolset` on stdio until the client closes
/// it, or with `--list-tools` print the definitions and exit. With the
/// `http` feature, `--http` serves it on 127.0.0.1 instead and `--new-token`
/// makes that server's bearer token.
pub fn run<T: Toolset + Send + Sync + 'static>(args: &McpArgs, toolset: T) -> Result<(), CliError> {
    if args.list_tools {
        let list = crate::toolset::tools_list_json(&toolset);
        let text = serde_json::to_string_pretty(&list)
            .map_err(|e| CliError::Other(format!("serializing the tool list: {e}")))?;
        println!("{text}");
        return Ok(());
    }
    #[cfg(feature = "http")]
    if args.new_token || args.http {
        return crate::http::run(args, toolset);
    }
    crate::stdio::serve(toolset)
}
