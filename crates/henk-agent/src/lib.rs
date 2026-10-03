//! The tool-calling loop.
//!
//! One [`Agent`] runs one lane of a review or one planner: it calls the model,
//! dispatches tool calls through a guarded tool set, and stops at a turn limit,
//! a timeout, or a clean end of turn.
