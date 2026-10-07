//! The tool-calling loop.
//!
//! One [`Agent`] runs one lane of a review or one planner: it calls the
//! model, dispatches tool calls through a [`ToolSet`], and stops at a turn
//! limit, a timeout, cancellation, or a clean end of turn. MCP tools reach
//! the set through [`mcp_tools()`], where every call first passes a guard that
//! can rewrite or refuse the arguments (spec §8.5).

pub mod agent;
pub mod compact;
pub mod mcp_tools;
pub mod prompts;
pub mod tool;

pub use agent::{
    Agent, AgentConfig, AgentEvent, AgentOutcome, CallOutcome, Continuation, EndReason, Ending,
    RepeatFiring, StopCause, TurnWarning,
};
pub use mcp_tools::{Guard, Verdict, mcp_tools};
pub use tool::{Tool, ToolOutput, ToolSet};
