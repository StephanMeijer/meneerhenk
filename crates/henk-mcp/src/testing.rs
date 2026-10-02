//! An in-process MCP server for tests, connected over a duplex pipe.

#![allow(clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt as _};
use serde_json::Value;

use crate::session::RmcpSession;

/// Decides what a fake tool returns.
pub type ToolBehaviour = dyn Fn(&str, &Value) -> CallToolResult + Send + Sync;

/// A record of one call the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    /// Tool name.
    pub name: String,
    /// Arguments as received.
    pub arguments: Value,
}

/// A fake MCP server with a fixed tool list and scripted behaviour.
#[derive(Clone)]
pub struct FakeServer {
    tools: Vec<Tool>,
    behaviour: Arc<ToolBehaviour>,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl FakeServer {
    /// Builds a fake offering `tools` whose calls are answered by `behaviour`.
    pub fn new(
        tools: Vec<Tool>,
        behaviour: impl Fn(&str, &Value) -> CallToolResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            tools,
            behaviour: Arc::new(behaviour),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A tool definition with an object schema of string properties.
    #[must_use]
    pub fn tool(name: &str, description: &str, properties: &[&str]) -> Tool {
        let props: serde_json::Map<String, Value> = properties
            .iter()
            .map(|p| ((*p).to_owned(), serde_json::json!({"type": "string"})))
            .collect();
        let schema = serde_json::json!({"type": "object", "properties": props});
        let Value::Object(schema) = schema else {
            unreachable!("json! object literal")
        };
        Tool::new(name.to_owned(), description.to_owned(), Arc::new(schema))
    }

    /// Every call received so far.
    #[must_use]
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Connects a client session to this fake over an in-memory pipe.
    ///
    /// # Panics
    ///
    /// Panics when the in-process handshake fails, which indicates a bug in
    /// the fake itself.
    pub async fn connect(&self, alias: &str) -> RmcpSession {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_side);
        let (client_read, client_write) = tokio::io::split(client_side);
        let server = self.clone();
        tokio::spawn(async move {
            match server.serve((server_read, server_write)).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(error) => tracing::error!(%error, "fake MCP server failed to start"),
            }
        });
        let service = ()
            .serve((client_read, client_write))
            .await
            .unwrap_or_else(|error| panic!("fake MCP handshake failed: {error}"));
        RmcpSession::from_service(alias, service, Duration::from_secs(5))
    }
}

impl ServerHandler for FakeServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("henk-fake-mcp", "0.0.0");
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.map_or(Value::Null, Value::Object);
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(RecordedCall {
                name: request.name.to_string(),
                arguments: arguments.clone(),
            });
        }
        if !self.tools.iter().any(|t| t.name == request.name) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown tool {}",
                request.name
            ))])
            .into());
        }
        Ok((self.behaviour)(&request.name, &arguments).into())
    }
}

/// A behaviour that answers every call with a text block naming the tool.
pub fn echo_behaviour() -> impl Fn(&str, &Value) -> CallToolResult + Send + Sync + 'static {
    |name, arguments| {
        CallToolResult::success(vec![ContentBlock::text(format!("{name}({arguments})"))])
    }
}
