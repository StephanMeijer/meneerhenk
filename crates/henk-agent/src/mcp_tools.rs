//! MCP server tools as agent tools, each behind a guard.

use std::sync::Arc;

use henk_llm::{ToolDef, ToolName};
use henk_mcp::{McpError, McpSession, NameMap, ToolInfo};
use serde_json::Value;
use tracing::{debug, warn};

use crate::tool::{Tool, ToolOutput};

/// What a guard decides about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Forward the call with these (possibly rewritten) arguments.
    Allow(Value),
    /// Refuse. The reason is returned to the model as a tool error.
    Deny(String),
}

/// Inspects a call before it leaves the process: the server's tool name and
/// the arguments the model supplied.
pub type Guard = Arc<dyn Fn(&str, &Value) -> Verdict + Send + Sync>;

/// A guard that allows everything unchanged. For probes, never for lanes.
#[must_use]
pub fn allow_all() -> Guard {
    Arc::new(|_, arguments| Verdict::Allow(arguments.clone()))
}

/// One MCP tool exposed to a model.
pub struct McpTool {
    session: Arc<dyn McpSession>,
    server_tool: String,
    definition: ToolDef,
    guard: Guard,
}

impl McpTool {
    /// The server's own name for this tool.
    #[must_use]
    pub fn server_tool(&self) -> &str {
        &self.server_tool
    }
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("server", &self.session.alias())
            .field("tool", &self.server_tool)
            .field("as", &self.definition.name)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDef {
        self.definition.clone()
    }

    async fn call(&self, arguments: Value) -> ToolOutput {
        let arguments = match (self.guard)(&self.server_tool, &arguments) {
            Verdict::Allow(rewritten) => rewritten,
            Verdict::Deny(reason) => {
                warn!(server = self.session.alias(), tool = %self.server_tool, %reason, "guard refused tool call");
                return ToolOutput::error(format!("Refused: {reason}"));
            }
        };
        match self.session.call_tool(&self.server_tool, arguments).await {
            Ok(outcome) => {
                debug!(server = self.session.alias(), tool = %self.server_tool, is_error = outcome.is_error, "tool call done");
                ToolOutput {
                    content: outcome.text,
                    is_error: outcome.is_error,
                }
            }
            Err(error) => ToolOutput::error(format!("Tool call failed: {error}")),
        }
    }
}

/// Wraps the tools of `session` that `expose` accepts. Names are registered
/// in `names` so the dispatcher can route calls and logs can show origins.
///
/// # Errors
///
/// Returns the [`McpError`] of listing the server's tools.
pub async fn mcp_tools(
    session: Arc<dyn McpSession>,
    names: &mut NameMap,
    expose: impl Fn(&ToolInfo) -> bool,
    guard: Guard,
) -> Result<Vec<McpTool>, McpError> {
    let mut tools = Vec::new();
    for info in session.list_tools().await? {
        if !expose(&info) {
            continue;
        }
        let model_name = names.register(session.alias(), &info.name);
        let Ok(name) = ToolName::parse(model_name.clone()) else {
            warn!(name = %model_name, "sanitised tool name still invalid; skipping");
            continue;
        };
        tools.push(McpTool {
            session: Arc::clone(&session),
            server_tool: info.name.clone(),
            definition: ToolDef {
                name,
                description: info.description.clone(),
                input_schema: info.input_schema.clone(),
            },
            guard: Arc::clone(&guard),
        });
    }
    Ok(tools)
}
