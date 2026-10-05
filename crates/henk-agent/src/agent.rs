//! The loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use henk_llm::{
    ChatMessage, CompletionRequest, LlmError, ModelClient, StopReason, ToolArguments, ToolChoice,
    ToolResult, Usage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::tool::{ToolOutput, ToolSet};

/// Limits for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConfig {
    /// Model calls allowed. Each turn is one call plus its tool calls.
    pub max_turns: u32,
    /// Wall-clock limit for the whole run. When it passes, a model call in
    /// flight is abandoned, tool calls in flight finish, and no new turn
    /// starts; the run ends with [`StopCause::Timeout`].
    pub timeout: Duration,
    /// Tool output longer than this is cut, with a note, before the model
    /// sees it. Keeps one huge diff from eating the context.
    pub max_tool_output_chars: usize,
    /// When the conversation grows past this many characters, old tool
    /// results are replaced by one-line stubs (see [`crate::compact`]).
    pub max_conversation_chars: usize,
    /// Turns whose tool results are never stubbed, counted from the end.
    pub keep_recent_turns: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 40,
            timeout: Duration::from_secs(10 * 60),
            max_tool_output_chars: 60_000,
            max_conversation_chars: 240_000,
            keep_recent_turns: 2,
        }
    }
}

/// Something that happened during a run, for logs and run pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// The model answered.
    ModelAnswered {
        /// 1-based turn.
        turn: u32,
        /// Tokens of this call.
        usage: Usage,
        /// Tool calls it asked for.
        tool_calls: usize,
    },
    /// A tool was called.
    ToolCalled {
        /// Model-facing name.
        name: String,
        /// Whether it reported an error.
        is_error: bool,
        /// How long it took.
        elapsed: Duration,
    },
}

/// Why a run ended.
#[derive(Debug)]
pub enum StopCause {
    /// The model ended its turn without tool calls.
    EndTurn,
    /// The turn limit was hit while the model still wanted tools.
    MaxTurns,
    /// The wall-clock limit was hit.
    Timeout,
    /// The caller cancelled.
    Cancelled,
    /// The model endpoint failed after its own retries.
    ModelError(LlmError),
}

impl StopCause {
    /// Whether the run finished on its own terms.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self, Self::EndTurn)
    }
}

/// What a run produced.
#[derive(Debug)]
pub struct AgentOutcome {
    /// The text of the last assistant message.
    pub final_text: String,
    /// Model calls made.
    pub turns: u32,
    /// Tokens over the whole run.
    pub usage: Usage,
    /// Why it stopped.
    pub stop: StopCause,
    /// The full conversation, for diagnosis.
    pub messages: Vec<ChatMessage>,
}

/// One model, one tool set, one system prompt.
pub struct Agent {
    model: Arc<dyn ModelClient>,
    tools: ToolSet,
    system: String,
    config: AgentConfig,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("model", &self.model.model())
            .field("tools", &self.tools)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Builds an agent.
    #[must_use]
    pub fn new(
        model: Arc<dyn ModelClient>,
        tools: ToolSet,
        system: impl Into<String>,
        config: AgentConfig,
    ) -> Self {
        Self {
            model,
            tools,
            system: system.into(),
            config,
            events: None,
        }
    }

    /// Sends an [`AgentEvent`] for every model answer and tool call.
    #[must_use]
    pub fn with_events(mut self, events: mpsc::UnboundedSender<AgentEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// The model name, for markers.
    #[must_use]
    pub fn model_name(&self) -> &str {
        self.model.model()
    }

    fn emit(&self, event: AgentEvent) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }

    /// Runs the loop from `initial` messages until it stops.
    #[instrument(skip_all, fields(model = %self.model.model()))]
    pub async fn run(&self, initial: Vec<ChatMessage>, cancel: CancellationToken) -> AgentOutcome {
        let deadline = tokio::time::Instant::now() + self.config.timeout;
        let mut messages = initial;
        let mut usage = Usage::default();
        let mut turns = 0;
        let definitions = self.tools.definitions();

        let stop = loop {
            if turns >= self.config.max_turns {
                break StopCause::MaxTurns;
            }
            turns += 1;
            let request = CompletionRequest {
                system: Some(self.system.clone()),
                messages: messages.clone(),
                tools: definitions.clone(),
                tool_choice: ToolChoice::Auto,
                max_tokens: None,
                temperature: None,
            };
            let completion = tokio::select! {
                biased;
                () = cancel.cancelled() => break StopCause::Cancelled,
                () = tokio::time::sleep_until(deadline) => break StopCause::Timeout,
                result = self.model.complete(&request) => match result {
                    Ok(completion) => completion,
                    Err(error) => break StopCause::ModelError(error),
                },
            };
            usage.input_tokens += completion.usage.input_tokens;
            usage.output_tokens += completion.usage.output_tokens;
            let calls: Vec<_> = completion.message.tool_calls().cloned().collect();
            self.emit(AgentEvent::ModelAnswered {
                turn: turns,
                usage: completion.usage,
                tool_calls: calls.len(),
            });
            debug!(turn = turns, tool_calls = calls.len(), stop = ?completion.stop, "model answered");
            messages.push(completion.message);

            if calls.is_empty() {
                if completion.stop == StopReason::MaxTokens {
                    warn!("model hit its output cap without tool calls");
                }
                break StopCause::EndTurn;
            }

            // Tool calls run to completion even past the deadline: a finding
            // being posted lands, and nothing is left half done. Only a
            // cancel interrupts them. The deadline is checked again before
            // the next model call.
            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                let started = Instant::now();
                let output = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Self::finish(messages, turns, usage, StopCause::Cancelled),
                    output = self.dispatch(&call.name, &call.arguments) => output,
                };
                let elapsed = started.elapsed();
                debug!(
                    turn = turns,
                    tool = %call.name,
                    args = %one_line(&arguments_text(&call.arguments), 300),
                    result_chars = output.content.chars().count(),
                    is_error = output.is_error,
                    elapsed_ms = elapsed.as_millis(),
                    "tool called"
                );
                self.emit(AgentEvent::ToolCalled {
                    name: call.name.clone(),
                    is_error: output.is_error,
                    elapsed,
                });
                results.push(ToolResult {
                    call_id: call.id,
                    content: self.truncate(output.content),
                    is_error: output.is_error,
                });
            }
            messages.push(ChatMessage::tool_results(results));
            let stubbed = crate::compact::compact(
                &mut messages,
                self.config.max_conversation_chars,
                self.config.keep_recent_turns,
            );
            if stubbed > 0 {
                debug!(
                    turn = turns,
                    stubbed,
                    chars = crate::compact::size(&messages),
                    "old tool results elided to stay under the conversation budget"
                );
            }
            if tokio::time::Instant::now() >= deadline {
                break StopCause::Timeout;
            }
        };
        Self::finish(messages, turns, usage, stop)
    }

    async fn dispatch(&self, name: &str, arguments: &ToolArguments) -> ToolOutput {
        let Some(tool) = self.tools.get(name) else {
            let known: Vec<&str> = self.tools.names().collect();
            return ToolOutput::error(format!(
                "Unknown tool {name:?}. Available: {}",
                known.join(", ")
            ));
        };
        match arguments {
            ToolArguments::Parsed(value) => tool.call(value.clone()).await,
            ToolArguments::Malformed(raw) => ToolOutput::error(format!(
                "Arguments were not valid JSON: {}",
                truncate_chars(raw, 200)
            )),
        }
    }

    fn truncate(&self, content: String) -> String {
        if content.chars().count() <= self.config.max_tool_output_chars {
            return content;
        }
        let kept = truncate_chars(&content, self.config.max_tool_output_chars);
        format!(
            "{kept}\n\n[output truncated at {} characters]",
            self.config.max_tool_output_chars
        )
    }

    fn finish(
        messages: Vec<ChatMessage>,
        turns: u32,
        usage: Usage,
        stop: StopCause,
    ) -> AgentOutcome {
        let final_text = messages
            .iter()
            .rev()
            .find(|m| m.role == henk_llm::Role::Assistant)
            .map(ChatMessage::text)
            .unwrap_or_default();
        info!(turns, input_tokens = usage.input_tokens, output_tokens = usage.output_tokens, stop = ?stop, "agent run ended");
        debug!(final_text = %one_line(&final_text, 300), "last assistant text");
        AgentOutcome {
            final_text,
            turns,
            usage,
            stop,
            messages,
        }
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Text cut to `max` characters with line breaks flattened, for one log line.
fn one_line(text: &str, max: usize) -> String {
    truncate_chars(text, max).replace(['\n', '\r'], " ")
}

/// The arguments as one line of text, for logs.
fn arguments_text(arguments: &ToolArguments) -> String {
    match arguments {
        ToolArguments::Parsed(value) => value.to_string(),
        ToolArguments::Malformed(raw) => format!("(malformed) {raw}"),
    }
}
