//! MCP client sessions.
//!
//! Henk is an MCP client. Each external server (github-mcp-server, gitlab-mcp,
//! Fastmail) is one session, started as a child process over stdio or reached
//! over streamable HTTP. No proxies or aggregators.
