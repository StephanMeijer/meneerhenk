//! The loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use henk_domain::repeat::{self, CallArguments, RepeatGuard, RepeatVerdict};
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
    /// Identical tool calls in a row that are let through. The next one is
    /// refused, and one more in a later turn ends the run with
    /// [`StopCause::Stuck`]. 0 turns the guard off (see
    /// [`henk_domain::repeat`]).
    pub max_repeated_calls: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 40,
            timeout: Duration::from_mins(10),
            max_tool_output_chars: 60_000,
            max_conversation_chars: 240_000,
            keep_recent_turns: 2,
            max_repeated_calls: repeat::DEFAULT_LIMIT,
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
    /// The repeat guard refused a call that repeated the ones before it.
    RepeatRefused {
        /// Model-facing name.
        tool: String,
        /// Identical calls in a row, the refused one included.
        repeats: u32,
        /// Whether the run ends because of it.
        ended: bool,
    },
}

/// One time the repeat guard fired, for the run record (§8.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatFiring {
    /// Model-facing name of the repeated tool.
    pub tool: String,
    /// Identical calls in a row, the refused one included.
    pub repeats: u32,
    /// Whether the run ended because of it.
    pub ended: bool,
}

/// Why the model stopped calling tools, as offered to a [`Continuation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The model ended its turn.
    EndTurn,
    /// The model hit its output cap mid-answer, without a tool call.
    OutputCap,
}

/// What a [`Continuation`] sees when the model stops.
#[derive(Debug)]
pub struct Ending<'a> {
    /// Why.
    pub reason: EndReason,
    /// The turn that ended.
    pub turn: u32,
    /// The conversation so far.
    pub messages: &'a [ChatMessage],
}

/// Asked when the model stops without tool calls. `Some(text)` is sent as
/// a user message and the loop takes another turn (still bounded by the
/// turn limit and the deadline); `None` lets the run end. The caller
/// decides how often to insist.
pub type Continuation = Box<dyn Fn(&Ending<'_>) -> Option<String> + Send + Sync>;

/// A message the agent sends once, as a user message before the model call
/// that leaves exactly `turns_left` turns, counting that one. A session that
/// would otherwise run into its turn limit mid-task gets to wrap up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnWarning {
    /// Turns left, including the one the message precedes.
    pub turns_left: u32,
    /// What the model is told.
    pub message: String,
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
    /// The provider's safety layer declined to answer (raw reason). Not a
    /// clean end: whatever the run was for did not happen (§8.8).
    Refused(String),
    /// The model kept calling one tool with the same arguments after the
    /// repeat guard refused it. Not a clean end.
    Stuck {
        /// Model-facing name of the repeated tool.
        tool: String,
        /// Identical calls in a row, the last one included.
        repeats: u32,
    },
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
    /// Every time the repeat guard fired, in order.
    pub repeats: Vec<RepeatFiring>,
}

/// How a turn's tool calls went.
enum ToolRound {
    /// Every call has a result.
    Done(Vec<ToolResult>),
    /// Cancelled midway.
    Cancelled,
    /// The repeat guard ended the run with [`StopCause::Stuck`]. Every call
    /// still has a result, so the conversation stays well formed.
    Stuck(Vec<ToolResult>, StopCause),
}

/// One model, one tool set, one system prompt.
pub struct Agent {
    model: Arc<dyn ModelClient>,
    tools: ToolSet,
    system: String,
    config: AgentConfig,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
    continuation: Option<Continuation>,
    turn_warning: Option<TurnWarning>,
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
            continuation: None,
            turn_warning: None,
        }
    }

    /// Asks `continuation` before ending a run that stopped without tool
    /// calls.
    #[must_use]
    pub fn with_continuation(mut self, continuation: Continuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    /// Sends `warning` once, a few turns before the turn limit.
    #[must_use]
    pub fn with_turn_warning(mut self, warning: TurnWarning) -> Self {
        self.turn_warning = Some(warning);
        self
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
        // One guard for the whole run, so repeats across turns count.
        let mut guard = RepeatGuard::new(self.config.max_repeated_calls);
        let mut repeats = Vec::new();

        let stop = loop {
            if turns >= self.config.max_turns {
                break StopCause::MaxTurns;
            }
            if let Some(warning) = &self.turn_warning
                && self.config.max_turns > warning.turns_left
                && self.config.max_turns - turns == warning.turns_left
            {
                debug!(
                    turn = turns + 1,
                    turns_left = warning.turns_left,
                    "turn warning sent"
                );
                messages.push(ChatMessage::user(warning.message.clone()));
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
            usage.add(&completion.usage);
            let calls: Vec<_> = completion.message.tool_calls().cloned().collect();
            self.emit(AgentEvent::ModelAnswered {
                turn: turns,
                usage: completion.usage,
                tool_calls: calls.len(),
            });
            debug!(turn = turns, tool_calls = calls.len(), stop = ?completion.stop, "model answered");
            messages.push(completion.message);

            if calls.is_empty() {
                // A refusal ends the run as one. No nudge: that would be a
                // second try at what the provider's safety layer stopped.
                if let StopReason::Refused(why) = &completion.stop {
                    warn!(reason = %why, "the model declined");
                    break StopCause::Refused(why.clone());
                }
                let reason = if completion.stop == StopReason::MaxTokens {
                    warn!("model hit its output cap without tool calls");
                    EndReason::OutputCap
                } else {
                    EndReason::EndTurn
                };
                let nudge = self.continuation.as_ref().and_then(|c| {
                    c(&Ending {
                        reason,
                        turn: turns,
                        messages: &messages,
                    })
                });
                match nudge {
                    Some(text) => {
                        debug!(turn = turns, ?reason, "continuing after a nudge");
                        messages.push(ChatMessage::user(text));
                        continue;
                    }
                    None => break StopCause::EndTurn,
                }
            }

            // Tool calls run to completion even past the deadline: a finding
            // being posted lands, and nothing is left half done. Only a
            // cancel interrupts them. The deadline is checked again before
            // the next model call.
            let results = match self
                .call_tools(calls, turns, &cancel, &mut guard, &mut repeats)
                .await
            {
                ToolRound::Done(results) => results,
                ToolRound::Cancelled => {
                    return Self::finish(messages, turns, usage, StopCause::Cancelled, repeats);
                }
                ToolRound::Stuck(results, stop) => {
                    messages.push(ChatMessage::tool_results(results));
                    break stop;
                }
            };
            messages.push(ChatMessage::tool_results(results));
            self.compact(&mut messages, turns);
            if tokio::time::Instant::now() >= deadline {
                break StopCause::Timeout;
            }
        };
        Self::finish(messages, turns, usage, stop, repeats)
    }

    /// Runs the model's tool calls in order, each first past the repeat
    /// guard. A refused call is not dispatched; its result tells the model
    /// to change course. A call the guard calls stuck ends the run, and the
    /// calls after it in the turn are not run.
    async fn call_tools(
        &self,
        calls: Vec<henk_llm::ToolCall>,
        turn: u32,
        cancel: &CancellationToken,
        guard: &mut RepeatGuard,
        firings: &mut Vec<RepeatFiring>,
    ) -> ToolRound {
        let mut results = Vec::with_capacity(calls.len());
        let mut stuck: Option<(String, u32)> = None;
        for call in calls {
            if stuck.is_some() {
                results.push(ToolResult {
                    call_id: call.id,
                    content: "Not run: the session is ending.".to_owned(),
                    is_error: true,
                });
                continue;
            }
            let arguments = match &call.arguments {
                ToolArguments::Parsed(value) => CallArguments::Json(value),
                ToolArguments::Malformed(raw) => CallArguments::Raw(raw),
            };
            let refusal = match guard.observe(&call.name, arguments) {
                RepeatVerdict::Allow => None,
                RepeatVerdict::Refuse { repeats } => {
                    Some((repeat::refusal_message(&call.name, repeats), repeats, false))
                }
                RepeatVerdict::Stuck { repeats } => {
                    Some((repeat::stuck_message(&call.name, repeats), repeats, true))
                }
            };
            if let Some((message, repeats, ended)) = refusal {
                warn!(turn, tool = %call.name, repeats, ended, "repeated tool call refused");
                self.emit(AgentEvent::RepeatRefused {
                    tool: call.name.clone(),
                    repeats,
                    ended,
                });
                firings.push(RepeatFiring {
                    tool: call.name.clone(),
                    repeats,
                    ended,
                });
                if ended {
                    stuck = Some((call.name.clone(), repeats));
                }
                results.push(ToolResult {
                    call_id: call.id,
                    content: message,
                    is_error: true,
                });
                continue;
            }
            let started = Instant::now();
            let output = tokio::select! {
                biased;
                () = cancel.cancelled() => return ToolRound::Cancelled,
                output = self.dispatch(&call.name, &call.arguments) => output,
            };
            let elapsed = started.elapsed();
            debug!(
                turn,
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
        // The results, refusals included, now go back to the model.
        guard.end_turn();
        match stuck {
            Some((tool, repeats)) => ToolRound::Stuck(results, StopCause::Stuck { tool, repeats }),
            None => ToolRound::Done(results),
        }
    }

    fn compact(&self, messages: &mut [ChatMessage], turn: u32) {
        let stubbed = crate::compact::compact(
            messages,
            self.config.max_conversation_chars,
            self.config.keep_recent_turns,
            |name| self.tools.keeps_in_context(name),
        );
        if stubbed > 0 {
            debug!(
                turn,
                stubbed,
                chars = crate::compact::size(messages),
                "old tool results elided to stay under the conversation budget"
            );
        }
    }

    async fn dispatch(&self, name: &str, arguments: &ToolArguments) -> ToolOutput {
        let Some(tool) = self.tools.get(name) else {
            let known: Vec<&str> = self.tools.names().collect();
            let hint = closest_tool(name, &known)
                .map(|known| format!(" Did you mean {known:?}?"))
                .unwrap_or_default();
            return ToolOutput::error(format!(
                "Unknown tool {name:?}.{hint} Available: {}",
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
        repeats: Vec<RepeatFiring>,
    ) -> AgentOutcome {
        let final_text = messages
            .iter()
            .rev()
            .find(|m| m.role == henk_llm::Role::Assistant)
            .map(|m| without_thinking(&m.text()))
            .unwrap_or_default();
        info!(turns, input_tokens = usage.input_tokens, output_tokens = usage.output_tokens, stop = ?stop, "agent run ended");
        debug!(final_text = %one_line(&final_text, 300), "last assistant text");
        AgentOutcome {
            final_text,
            turns,
            usage,
            stop,
            messages,
            repeats,
        }
    }
}

/// The known tool an unknown name most likely meant: the same name with an
/// MCP server prefix added or taken away (`github__x` for `x`, or the
/// reverse). Models that see both kinds of name in one list guess the
/// prefix.
fn closest_tool<'a>(name: &str, known: &[&'a str]) -> Option<&'a str> {
    let bare = |n: &'a str| n.split_once("__").map_or(n, |(_, rest)| rest);
    let wanted = name.split_once("__").map_or(name, |(_, rest)| rest);
    known.iter().copied().find(|k| bare(k) == wanted)
}

/// Text with reasoning some models write inline removed: every
/// `<think>...</think>` span, and an unclosed `<think>` with everything after
/// it. The last words of a session reach the run page; reasoning does not
/// belong there.
fn without_thinking(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start + OPEN.len()..).unwrap_or_default();
        let Some(end) = after.find(CLOSE) else {
            rest = "";
            break;
        };
        rest = after.get(end + CLOSE.len()..).unwrap_or_default();
    }
    out.push_str(rest);
    out.trim().to_owned()
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_guessed_server_prefix_points_at_the_real_tool() {
        let known = [
            "get_file_diff",
            "github__get_commit",
            "list_existing_findings",
        ];
        assert_eq!(
            closest_tool("github__list_existing_findings", &known),
            Some("list_existing_findings")
        );
        assert_eq!(
            closest_tool("get_commit", &known),
            Some("github__get_commit")
        );
        assert_eq!(closest_tool("delete_everything", &known), None);
    }

    #[test]
    fn inline_reasoning_is_removed_from_the_final_text() {
        assert_eq!(without_thinking("<think>hmm</think>Done."), "Done.");
        assert_eq!(
            without_thinking("A <think>x</think>B<think>y</think> C"),
            "A B C"
        );
        assert_eq!(without_thinking("Done.<think>never closed"), "Done.");
        assert_eq!(without_thinking("Plain."), "Plain.");
    }
}
