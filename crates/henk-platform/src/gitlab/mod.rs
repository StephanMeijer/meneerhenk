//! GitLab: the writer over a write-mode MCP session, and the few REST reads
//! an address run needs besides.

pub mod address;
pub mod issues;
pub mod rest;
pub mod writer;

pub use rest::GitLabRest;
pub use writer::GitLabWriter;
