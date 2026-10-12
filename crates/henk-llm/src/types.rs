//! Provider-neutral message, tool and completion types.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who wrote a message. The system prompt is not a message; it travels
/// separately in [`CompletionRequest::system`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The caller: text, and tool results.
    User,
    /// The model: text, tool calls, and opaque blocks it wants echoed back.
    Assistant,
}

/// Why a tool name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("tool name {0:?} must match [a-zA-Z0-9_-]{{1,64}}")]
pub struct ToolNameError(String);

/// The longest tool name, and the longest name [`ToolName::shown`] gives.
const SHOWN_CHARS: usize = 64;
/// What ends a name [`ToolName::shown`] cut.
const CUT: &str = " [cut]";

/// A tool name every provider accepts: `[a-zA-Z0-9_-]{1,64}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Validates a tool name.
    ///
    /// # Errors
    ///
    /// Returns [`ToolNameError`] when the name is empty, longer than 64
    /// bytes, or contains anything but ASCII letters, digits, `_` and `-`.
    pub fn parse(value: impl Into<String>) -> Result<Self, ToolNameError> {
        let value = value.into();
        if Self::is_valid(&value) {
            Ok(Self(value))
        } else {
            Err(ToolNameError(value))
        }
    }

    fn is_valid(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= SHOWN_CHARS
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    /// A name a model called, fit to store, log and show: a valid name as
    /// it is, and anything else, such as prose a model wrote in the name
    /// field, on one line and at most 64 characters, ending in ` [cut]`
    /// when it was longer (#341). The conversation keeps what the model
    /// wrote; only copies of it go through here.
    #[must_use]
    pub fn shown(called: &str) -> Cow<'_, str> {
        if Self::is_valid(called) {
            return Cow::Borrowed(called);
        }
        let flat = called
            .split(|c: char| c.is_whitespace() || c.is_control())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if flat.is_empty() {
            return Cow::Borrowed("(empty)");
        }
        if flat.chars().count() <= SHOWN_CHARS {
            return Cow::Owned(flat);
        }
        let kept: String = flat.chars().take(SHOWN_CHARS - CUT.len()).collect();
        Cow::Owned(format!("{}{CUT}", kept.trim_end()))
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = ToolNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ToolName> for String {
    fn from(name: ToolName) -> Self {
        name.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDef {
    /// The name the model uses.
    pub name: ToolName,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments. Passed to the provider after
    /// [`crate::schema::clean`].
    pub input_schema: Value,
}

/// The arguments of a tool call as the model produced them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolArguments {
    /// Valid JSON.
    Parsed(Value),
    /// The model produced something that is not JSON. The caller should
    /// answer with an error result so the model can try again.
    Malformed(String),
}

/// One tool call in an assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolCall {
    /// The provider's id for this call; tool results refer to it.
    pub id: String,
    /// The tool name as the model wrote it.
    pub name: String,
    /// The arguments.
    pub arguments: ToolArguments,
}

/// The result of one tool call, sent back in a user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResult {
    /// The id of the call this answers.
    pub call_id: String,
    /// The result as text. Non-text MCP content is described in words by
    /// the caller before it gets here.
    pub content: String,
    /// Whether the tool failed. Providers that have no flag get the text only.
    pub is_error: bool,
}

/// One block of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Block {
    /// Plain text.
    Text(String),
    /// A call the model makes (assistant only).
    ToolCall(ToolCall),
    /// An answer to a call (user only).
    ToolResult(ToolResult),
    /// Provider-specific content the model wants echoed back unchanged on
    /// the next turn, such as Anthropic thinking blocks. Never inspected.
    Opaque(Value),
}

/// One message in the conversation. Serialises for local transcripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    /// Who wrote it.
    pub role: Role,
    /// Its content, in order.
    pub blocks: Vec<Block>,
}

impl ChatMessage {
    /// A user message with one text block.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            blocks: vec![Block::Text(text.into())],
        }
    }

    /// An assistant message with one text block.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            blocks: vec![Block::Text(text.into())],
        }
    }

    /// A user message carrying tool results.
    #[must_use]
    pub fn tool_results(results: impl IntoIterator<Item = ToolResult>) -> Self {
        Self {
            role: Role::User,
            blocks: results.into_iter().map(Block::ToolResult).collect(),
        }
    }

    /// All text blocks joined with newlines.
    #[must_use]
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }

    /// The tool calls in this message, in order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> + '_ {
        self.blocks.iter().filter_map(|block| match block {
            Block::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

/// How the model may use tools.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    /// The model decides.
    #[default]
    Auto,
    /// No tool calls this turn.
    None,
    /// The model must call some tool.
    Required,
    /// The model must call this tool.
    Named(ToolName),
}

/// How long the provider keeps the prompt cache a request writes (#330).
/// Only Anthropic takes a choice; others ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheTtl {
    /// The default, about 5 minutes: enough for the turns of one session,
    /// each of which refreshes it.
    #[default]
    Short,
    /// About an hour, at a higher write price: for a session that waits
    /// longer than 5 minutes between its turns, such as a role in a pairing
    /// while the other one works.
    Long,
}

/// Everything one completion needs. The model itself is fixed per client.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompletionRequest {
    /// The system prompt.
    pub system: Option<String>,
    /// The conversation so far.
    pub messages: Vec<ChatMessage>,
    /// Tools the model may call.
    pub tools: Vec<ToolDef>,
    /// How it may call them.
    pub tool_choice: ToolChoice,
    /// Output token cap. `None` uses the client's configured default.
    pub max_tokens: Option<u32>,
    /// Sampling temperature, when the caller wants to set it.
    pub temperature: Option<f64>,
    /// How long the prompt cache it writes should last.
    pub cache_ttl: CacheTtl,
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// A normal end of turn.
    EndTurn,
    /// The model wants tool results.
    ToolUse,
    /// The output token cap was hit.
    MaxTokens,
    /// The provider's safety layer declined: Anthropic `refusal`, OpenAI
    /// `content_filter`. The raw reason is kept.
    Refused(String),
    /// Something provider-specific.
    Other(String),
}

/// Token usage of one completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Usage {
    /// Prompt tokens processed at full price: the part not read from or
    /// written to a cache.
    pub input_tokens: u64,
    /// Tokens generated.
    pub output_tokens: u64,
    /// Prompt tokens read from the provider's prompt cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written to the provider's prompt cache.
    pub cache_write_tokens: u64,
    /// Of `output_tokens`, those the model spent reasoning before it
    /// answered. Already counted in `output_tokens`.
    pub reasoning_tokens: u64,
}

impl Usage {
    /// The whole prompt: uncached, read from cache and written to it.
    #[must_use]
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Adds another completion's usage.
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

/// One completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The assistant message, to be appended to the conversation as is.
    pub message: ChatMessage,
    /// Why the model stopped.
    pub stop: StopReason,
    /// Tokens used.
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn tool_names_are_validated() {
        assert!(ToolName::parse("get_pull_request").is_ok());
        assert!(ToolName::parse("github-get_file").is_ok());
        assert!(ToolName::parse("").is_err());
        assert!(ToolName::parse("has space").is_err());
        assert!(ToolName::parse("dot.name").is_err());
        assert!(ToolName::parse("x".repeat(65)).is_err());
    }

    #[test]
    fn an_invented_name_is_shown_on_one_line_and_cut() {
        assert!(matches!(
            ToolName::shown("github-get_file"),
            Cow::Borrowed("github-get_file")
        ));
        assert_eq!(
            ToolName::shown("review\tdone.\n\nEnd."),
            "review done. End."
        );
        assert_eq!(ToolName::shown(" \n\u{7}"), "(empty)");
        let prose = "Henk review done. No more findings. ".repeat(130);
        let shown = ToolName::shown(&prose);
        assert_eq!(shown.chars().count(), 64, "{shown}");
        assert!(shown.starts_with("Henk review done. No more"), "{shown}");
        assert!(shown.ends_with(" [cut]"), "{shown}");
        let wide = "é".repeat(100);
        assert_eq!(ToolName::shown(&wide), format!("{} [cut]", "é".repeat(58)));
    }

    #[test]
    fn message_helpers_collect_text_and_calls() {
        let message = ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Text("a".into()),
                Block::ToolCall(ToolCall {
                    id: "1".into(),
                    name: "t".into(),
                    arguments: ToolArguments::Parsed(Value::Null),
                }),
                Block::Text("b".into()),
            ],
        };
        assert_eq!(message.text(), "a\nb");
        assert_eq!(message.tool_calls().count(), 1);
    }
}
