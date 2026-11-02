//! The rmcp server for a [`Toolset`], shared by the transports (`stdio`,
//! and with the `http` feature, `http`).
//!
//! Each call runs on Tokio's blocking pool through
//! [`pk_cli_core::block_on`], outside any Tokio context, so a toolset may call
//! blocking provider code (`reqwest::blocking`) directly, and read-only calls
//! the client sends together run in parallel.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use pk_cli_core::{block_on, CliError};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult as RmcpResult, Implementation,
    ListToolsResult, PaginatedRequestParams, ResultType, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::Value;

use crate::error::ToolError;
use crate::result::CallToolResult;
use crate::toolset::{call_tool, instructions, tools_list, Server, Toolset, UnknownTool};

/// The rmcp server for a toolset. Definitions and instructions are computed
/// once: the list is fixed for the life of the process. Cloning shares them,
/// so a stateless HTTP server can hand each request its own copy.
pub(crate) struct Adapter<T> {
    toolset: Arc<T>,
    tools: Arc<Vec<Tool>>,
    /// The tools that are not read-only, for the timeout's wording.
    writes: Arc<HashSet<String>>,
    config: ServerConfig,
    /// How long a call may run before the client is answered with an error.
    /// `None` on stdio, where the client keeps its own clock.
    call_timeout: Option<Duration>,
}

impl<T> Clone for Adapter<T> {
    fn clone(&self) -> Self {
        Self {
            toolset: Arc::clone(&self.toolset),
            tools: Arc::clone(&self.tools),
            writes: Arc::clone(&self.writes),
            config: self.config.clone(),
            call_timeout: self.call_timeout,
        }
    }
}

impl<T: Toolset + Send + Sync + 'static> Adapter<T> {
    pub(crate) fn new(toolset: T, call_timeout: Option<Duration>) -> Result<Self, CliError> {
        let tools = tools_list(&toolset)
            .into_iter()
            .map(serde_json::from_value::<Tool>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CliError::Other(format!("a tool definition rmcp rejects: {e}")))?;
        let writes = toolset
            .tools()
            .into_iter()
            .filter(|t| !t.annotations.read_only)
            .map(|t| t.name)
            .collect();
        let server = toolset.server();
        let mut identity = Implementation::new(&server.bin, &server.version);
        identity.title = Some(server.service.clone());
        let config = ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(identity)
            .with_instructions(instructions(&toolset));
        Ok(Self {
            toolset: Arc::new(toolset),
            tools: Arc::new(tools),
            writes: Arc::new(writes),
            config,
            call_timeout,
        })
    }

    /// One call, on the blocking pool, answered within the call timeout
    /// when there is one.
    async fn call(&self, name: String, arguments: Option<Value>) -> Result<Answer, String> {
        let toolset = Arc::clone(&self.toolset);
        let tool = name.clone();
        let work = move || block_on(call_tool(toolset.as_ref(), &tool, arguments));
        match bounded(work, self.call_timeout).await {
            Ok(Some(Ok(result))) => Ok(Answer::Result(result)),
            Ok(Some(Err(unknown))) => Ok(Answer::Unknown(unknown)),
            Ok(None) => {
                let limit = self.call_timeout.unwrap_or_default();
                let write = self.writes.contains(&name);
                Ok(Answer::Result(timed_out(
                    &self.toolset.server(),
                    limit,
                    write,
                )))
            }
            Err(panicked) => Err(panicked),
        }
    }
}

/// How a call ended.
enum Answer {
    Result(CallToolResult),
    Unknown(UnknownTool),
}

/// Run `work` on the blocking pool. `Ok(None)` when it was still running
/// after `limit`: it is not cancelled (blocking work cannot be) and runs on
/// to its end on its thread, unobserved. `Err` when it panicked.
async fn bounded<R: Send + 'static>(
    work: impl FnOnce() -> R + Send + 'static,
    limit: Option<Duration>,
) -> Result<Option<R>, String> {
    let call = tokio::task::spawn_blocking(work);
    let joined = match limit {
        None => call.await,
        Some(limit) => match tokio::time::timeout(limit, call).await {
            Ok(joined) => joined,
            Err(_) => return Ok(None),
        },
    };
    joined
        .map(Some)
        .map_err(|e| format!("the tool call panicked: {e}"))
}

/// The answer for a call still running when its time ran out. Its outcome
/// is unknown: the call goes on running, so a write may still land. A read
/// can simply be tried again; a write must not be, until the current state
/// shows whether it happened.
fn timed_out(server: &Server, limit: Duration, write: bool) -> CallToolResult {
    let secs = limit.as_secs();
    let error = if write {
        ToolError::other(format!(
            "the call was still running after {secs} s and was not waited for; it may still \
             complete, so whether it changed anything is unknown"
        ))
        .with_fix(
            "Do not call it again yet: check the current state with a read tool first, and tell \
             the user the outcome is unknown until then",
        )
    } else {
        ToolError::upstream(format!(
            "the call was still running after {secs} s and was not waited for"
        ))
        .with_fix("Try again later, or narrow the call (a shorter date range, a smaller limit)")
    };
    CallToolResult::error(error.render(server))
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
        Ok(ListToolsResult::with_all_items(self.tools.as_ref().clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.map(Value::Object);
        match self.call(request.name.to_string(), arguments).await {
            Ok(Answer::Result(result)) => {
                let mut result =
                    serde_json::from_value::<RmcpResult>(result.to_json()).map_err(|e| {
                        McpError::internal_error(format!("encoding the result: {e}"), None)
                    })?;
                // As rmcp's own constructors do; it clears the field for a
                // peer on a protocol version older than 2026-07-28.
                result.result_type = Some(ResultType::COMPLETE);
                Ok(CallToolResponse::Complete(result))
            }
            Ok(Answer::Unknown(unknown)) => {
                Err(McpError::invalid_params(unknown.to_string(), None))
            }
            Err(panicked) => Err(McpError::internal_error(panicked, None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ToolOutput;
    use crate::tool::ToolDef;

    /// Two tools that take longer than any test's limit.
    struct Slow;

    impl Toolset for Slow {
        fn server(&self) -> Server {
            Server::new("slow", "0.0.0", "Slow", "slow")
        }

        fn tools(&self) -> Vec<ToolDef> {
            vec![
                ToolDef::read("slow_read", "Read", "Reads slowly. On purpose."),
                ToolDef::write("slow_write", "Write", "Writes slowly. On purpose.", false),
            ]
        }

        async fn call(&self, _name: &str, _args: Value) -> Result<ToolOutput, ToolError> {
            std::thread::sleep(Duration::from_millis(300));
            ToolOutput::new(&serde_json::json!({ "done": true }))
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn text(result: &CallToolResult) -> String {
        result.to_json()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn bounded_answers_none_when_the_work_outlives_its_limit() {
        let rt = rt();
        let slow = || std::thread::sleep(Duration::from_millis(300));
        let got = rt.block_on(bounded(slow, Some(Duration::from_millis(10))));
        assert_eq!(got, Ok(None));
        let got = rt.block_on(bounded(|| 7, Some(Duration::from_secs(10))));
        assert_eq!(got, Ok(Some(7)));
        let got = rt.block_on(bounded(|| 7, None));
        assert_eq!(got, Ok(Some(7)));
        let got = rt.block_on(bounded(|| -> u8 { panic!("boom") }, None));
        assert!(got.unwrap_err().contains("panicked"));
    }

    #[test]
    fn without_a_limit_a_slow_call_is_waited_for() {
        let adapter = Adapter::new(Slow, None).unwrap();
        let answer = rt()
            .block_on(adapter.call("slow_read".into(), None))
            .unwrap();
        let Answer::Result(result) = answer else {
            panic!("a known tool")
        };
        assert!(!result.is_error());
    }

    #[test]
    fn a_read_past_its_limit_is_an_upstream_error_that_may_be_retried() {
        let adapter = Adapter::new(Slow, Some(Duration::from_millis(10))).unwrap();
        let answer = rt()
            .block_on(adapter.call("slow_read".into(), None))
            .unwrap();
        let Answer::Result(result) = answer else {
            panic!("a known tool")
        };
        assert!(result.is_error());
        let text = text(&result);
        assert!(text.starts_with("upstream: "), "{text}");
        assert!(text.contains("was not waited for") && text.contains("Try again later"));
    }

    #[test]
    fn a_write_past_its_limit_is_reported_as_unknown_and_not_to_be_retried() {
        let adapter = Adapter::new(Slow, Some(Duration::from_millis(10))).unwrap();
        let answer = rt()
            .block_on(adapter.call("slow_write".into(), None))
            .unwrap();
        let Answer::Result(result) = answer else {
            panic!("a known tool")
        };
        assert!(result.is_error());
        let text = text(&result);
        assert!(text.starts_with("other: "), "{text}");
        assert!(
            text.contains("may still complete") && text.contains("unknown"),
            "{text}"
        );
        assert!(text.contains("Do not call it again yet"), "{text}");
        assert!(!text.contains("Try again later"), "{text}");
    }

    #[test]
    fn an_unknown_tool_is_not_a_result() {
        let adapter = Adapter::new(Slow, None).unwrap();
        let answer = rt().block_on(adapter.call("nope".into(), None)).unwrap();
        assert!(matches!(answer, Answer::Unknown(_)));
    }
}
