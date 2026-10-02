//! MCP client sessions.
//!
//! Henk is an MCP client. Each external server (github-mcp-server,
//! gitlab-mcp, Fastmail) is one [`McpSession`], started as a child process
//! over stdio or reached over streamable HTTP. No proxies or aggregators:
//! Henk's process talks to each server directly, and the credentials a
//! server needs reach it through its environment, never through the model
//! (spec §8.4).
//!
//! [`NameMap`] gives every server tool a model-facing name and routes calls
//! back. The `testing` feature adds an in-process fake server so callers can
//! be tested without any binary.

pub mod config;
pub mod error;
pub mod names;
pub mod session;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use config::{McpServerConfig, McpTransport};
pub use error::McpError;
pub use names::{NameMap, ToolOrigin};
pub use session::{McpSession, RmcpSession, ToolInfo, ToolOutcome};
