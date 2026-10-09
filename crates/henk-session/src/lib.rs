//! One abstract way to run a model session.
//!
//! A [`SessionSpec`] says what varies: the model, the system prompt, the
//! opening messages, the tools and the limits. [`run_session`] does what every
//! session shares: a lane row in the run store, the loop, the mapping of how
//! it stopped to a lane status, and a line on the run's timeline. Review lanes
//! and the planner are both sessions.
//!
//! [`platform_tools`] is the one place where a platform's MCP read tools are
//! filtered and guarded by scope (spec §8.5) before a model may call them.
//!
//! When `HENK_TRANSCRIPT_DIR` names a directory, every session also writes
//! its full conversation there as JSON. That is a local diagnostic file for
//! the operator; nothing reads it back and nothing sends it anywhere.

pub mod transcript;

use std::sync::Arc;

use henk_agent::mcp_tools::McpTool;
use henk_agent::{
    Agent, AgentConfig, AgentEvent, Continuation, RepeatFiring, StopCause, ToolSet, TurnWarning,
    Verdict, mcp_tools,
};
use henk_domain::allowlist::Platform;
use henk_domain::marker::ModelId;
use henk_domain::run::RunId;
use henk_domain::scope::{self, Scope};
use henk_llm::{ChatMessage, ModelClient, Usage};
use henk_mcp::{McpError, McpSession, NameMap};
use henk_store::{LaneStatus, RunStore, ToolCallRecord, ToolUsage};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

/// What varies between sessions.
pub struct SessionSpec {
    /// Lane name for the run page: a configured lane name, or `planner`.
    pub name: String,
    /// The model.
    pub model: Arc<dyn ModelClient>,
    /// The system prompt, persona included.
    pub system: String,
    /// The first messages of the conversation.
    pub opening: Vec<ChatMessage>,
    /// Every tool the model may call.
    pub tools: ToolSet,
    /// Turn limit, deadline, output cap.
    pub limits: AgentConfig,
    /// Asked before the session ends without tool calls; may send one more
    /// message and take another turn.
    pub continuation: Option<Continuation>,
    /// Sent once, a few turns before the turn limit.
    pub turn_warning: Option<TurnWarning>,
}

impl std::fmt::Debug for SessionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSpec")
            .field("name", &self.name)
            .field("model", &self.model.model())
            .field("tools", &self.tools)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// How a session ended. `stop` says whether the time limit ended it.
#[derive(Debug)]
pub struct SessionOutcome {
    /// Why the loop stopped.
    pub stop: StopCause,
    /// Model calls made.
    pub turns: u32,
    /// Tokens over the run.
    pub usage: Usage,
    /// The last assistant text.
    pub final_text: String,
    /// The lane status recorded in the store.
    pub status: LaneStatus,
    /// The error recorded, when it did not finish.
    pub error: Option<String>,
}

impl SessionOutcome {
    /// Whether the session ran to an end: the end of its turn, its turn
    /// limit or its time limit, as opposed to being cancelled or failing.
    /// What it drafted stands either way.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(self.status, LaneStatus::Finished | LaneStatus::TimedOut)
    }
}

/// Runs one session and records it as a lane of `run`.
#[instrument(skip_all, fields(run = %run, session = %spec.name, model = %spec.model.model()))]
pub async fn run_session(
    store: &dyn RunStore,
    run: &RunId,
    spec: SessionSpec,
    cancel: CancellationToken,
) -> SessionOutcome {
    let model_name = spec.model.model().to_owned();
    if let Err(error) = store.start_lane(run, &spec.name, &model_name).await {
        tracing::warn!(%error, "could not record the lane start");
    }
    let system = spec.system;
    let mut agent = Agent::new(
        Arc::clone(&spec.model),
        spec.tools,
        system.clone(),
        spec.limits,
    );
    if let Some(continuation) = spec.continuation {
        agent = agent.with_continuation(continuation);
    }
    if let Some(warning) = spec.turn_warning {
        agent = agent.with_turn_warning(warning);
    }
    let (outcome, calls) = run_recorded(
        agent,
        spec.opening,
        cancel,
        store,
        run,
        &spec.name,
        &model_name,
        spec.limits.record_argument_bytes,
        0,
    )
    .await;
    if !calls.is_empty() {
        info!(lane = %spec.name, usage = %usage_line(&ToolUsage::from_calls(&calls)), "tool usage");
    }
    keep_transcript(store, run, &spec.name, &model_name, &system, &outcome).await;
    let (status, error) = lane_status(&outcome.stop);
    if let Err(store_error) = store
        .finish_lane(
            run,
            &spec.name,
            status,
            u64::from(outcome.turns),
            // The whole prompt, cached or not, so lanes stay comparable
            // with runs from before caching.
            outcome.usage.prompt_tokens(),
            outcome.usage.output_tokens,
            error.as_deref(),
        )
        .await
    {
        tracing::warn!(error = %store_error, "could not record the lane end");
    }
    // Each time the repeat guard fired goes on the run (§8.6), so
    // `henk runs show` says which tool, how often and whether it ended
    // the session.
    for firing in &outcome.repeats {
        let _ = store
            .event(run, "warn", &repeat_event(&spec.name, firing))
            .await;
    }
    end_events(store, run, &spec.name, &outcome, error.is_some()).await;
    info!(turns = outcome.turns, ?status, "session ended");
    SessionOutcome {
        stop: outcome.stop,
        turns: outcome.turns,
        usage: outcome.usage,
        final_text: outcome.final_text,
        status,
        error,
    }
}

/// Puts the whole conversation on the run (#191), and in
/// `HENK_TRANSCRIPT_DIR` when it is set. A store that cannot take it costs
/// the transcript, never the session.
async fn keep_transcript(
    store: &dyn RunStore,
    run: &RunId,
    name: &str,
    model: &str,
    system: &str,
    outcome: &henk_agent::AgentOutcome,
) {
    match transcript::json(run, name, model, system, outcome) {
        Ok(body) => {
            let record = henk_store::TranscriptRecord {
                at: String::new(),
                session: name.to_owned(),
                model: model.to_owned(),
                stop: format!("{:?}", outcome.stop),
                turns: outcome.turns,
                bytes: u64::try_from(body.len()).unwrap_or(u64::MAX),
                body,
            };
            if let Err(error) = store.record_transcript(run, &record).await {
                tracing::warn!(%error, "could not store the transcript");
            }
        }
        Err(error) => tracing::warn!(%error, "could not serialise the transcript"),
    }
    if let Some(dir) = transcript::directory_from_env() {
        match transcript::write(&dir, run, name, model, system, outcome) {
            Ok(path) => info!(path = %path.display(), "transcript written"),
            Err(error) => tracing::warn!(%error, "could not write the transcript"),
        }
    }
}

/// How a session ended, as its lane row records it (#231): it ran to an
/// end (finished), reached its time limit (timed out: what it drafted until
/// then stands, it drafts nothing more), or broke off (did not finish, with
/// why). How a caller presents a time limit is the caller's business: the
/// review reports such a lane as stopped at the time limit in its summary,
/// the planner treats it as a failed plan.
fn lane_status(stop: &StopCause) -> (LaneStatus, Option<String>) {
    match stop {
        StopCause::EndTurn | StopCause::MaxTurns => (LaneStatus::Finished, None),
        StopCause::Timeout => (LaneStatus::TimedOut, None),
        StopCause::Cancelled => (LaneStatus::DidNotFinish, Some("cancelled".to_owned())),
        StopCause::ModelError(e) => (LaneStatus::DidNotFinish, Some(e.to_string())),
        StopCause::Refused(why) => (
            LaneStatus::DidNotFinish,
            Some(format!("the model declined ({why})")),
        ),
        StopCause::Stuck { tool, .. } => (
            LaneStatus::DidNotFinish,
            Some(format!("stuck repeating {tool}")),
        ),
    }
}

/// A session kept across the rounds of a review loop (#284). Each round
/// resumes the same conversation with one new message, so the model sees
/// everything it said and read before. The run keeps one lane row for it,
/// ended by [`ResumableSession::finish`] with the turns and tokens of every
/// round, and its newest transcript holds the whole conversation.
pub struct ResumableSession {
    name: String,
    model: Arc<dyn ModelClient>,
    model_name: String,
    system: String,
    tools: ToolSet,
    limits: AgentConfig,
    continuation: Option<ContinuationFactory>,
    messages: Vec<ChatMessage>,
    /// Where each round's opening message sits in `messages`.
    round_starts: Vec<usize>,
    /// The rounds [`ResumableSession::compact_rounds`] dropped, kept for
    /// the transcript, which holds the whole conversation.
    dropped: Vec<ChatMessage>,
    turns: u32,
    usage: Usage,
    status: Option<(LaneStatus, Option<String>)>,
}

/// Makes a fresh [`Continuation`] for each round of a [`ResumableSession`]:
/// one is not `Clone`, and each round's nudges start over.
pub type ContinuationFactory = Arc<dyn Fn() -> Continuation + Send + Sync>;

impl std::fmt::Debug for ResumableSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumableSession")
            .field("name", &self.name)
            .field("model", &self.model_name)
            .field("messages", &self.messages.len())
            .field("turns", &self.turns)
            .finish_non_exhaustive()
    }
}

/// How one round of a [`ResumableSession`] ended.
#[derive(Debug)]
pub struct RoundOutcome {
    /// Why this round stopped.
    pub stop: StopCause,
    /// Model calls this round.
    pub turns: u32,
    /// The last assistant text of this round.
    pub final_text: String,
}

impl ResumableSession {
    /// A session that has not run yet. `limits` hold per round: every round
    /// gets the whole turn limit and time limit again.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        model: Arc<dyn ModelClient>,
        system: impl Into<String>,
        tools: ToolSet,
        limits: AgentConfig,
    ) -> Self {
        let model_name = model.model().to_owned();
        Self {
            name: name.into(),
            model,
            model_name,
            system: system.into(),
            tools,
            limits,
            continuation: None,
            messages: Vec::new(),
            round_starts: Vec::new(),
            dropped: Vec::new(),
            turns: 0,
            usage: Usage::default(),
            status: None,
        }
    }

    /// Asks `continuation()`'s continuation, in every round, before the
    /// round ends without tool calls.
    #[must_use]
    pub fn with_continuation(mut self, continuation: ContinuationFactory) -> Self {
        self.continuation = Some(continuation);
        self
    }

    /// The conversation so far.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// The time limit of the next rounds, such as what is left of a whole
    /// run's (#286).
    pub fn set_timeout(&mut self, timeout: std::time::Duration) {
        self.limits.timeout = timeout;
    }

    /// When the conversation is over its `max_conversation_chars`, drops
    /// every round but the last `keep_rounds` and puts `summary` in front
    /// of the first one kept (#286). Rounds are dropped whole, so every
    /// tool call keeps its result and the roles still alternate. Thinking
    /// blocks go too: a model refuses one replayed after an edit of the
    /// history before it (see `henk_agent::compact`). The stored transcript
    /// still holds the dropped rounds. Returns whether anything was dropped.
    pub fn compact_rounds(&mut self, summary: &str, keep_rounds: usize) -> bool {
        if henk_agent::compact::size(&self.messages) <= self.limits.max_conversation_chars {
            return false;
        }
        let keep = keep_rounds.max(1);
        if self.round_starts.len() <= keep {
            return false;
        }
        let first_kept = self.round_starts.len() - keep;
        let cut = self.round_starts.get(first_kept).copied().unwrap_or(0);
        self.dropped.extend(self.messages.drain(..cut));
        for message in &mut self.messages {
            message
                .blocks
                .retain(|block| !matches!(block, henk_llm::Block::Opaque(_)));
        }
        self.round_starts = self
            .round_starts
            .split_off(first_kept)
            .into_iter()
            .map(|start| start - cut)
            .collect();
        if let Some(opening) = self.messages.first_mut() {
            opening.blocks.insert(
                0,
                henk_llm::Block::Text(format!(
                    "What happened in the earlier rounds, which this conversation no longer holds:\n\n{summary}"
                )),
            );
        }
        info!(lane = %self.name, dropped = cut, "compacted the earlier rounds");
        true
    }

    /// Model calls over every round.
    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// Sends `message` after the conversation so far and runs until the
    /// model ends its turn or a limit of this round stops it. Tool calls
    /// and the live view are recorded as for any session; the transcript
    /// stored after the round holds the whole conversation.
    pub async fn resume(
        &mut self,
        store: &dyn RunStore,
        run: &RunId,
        message: ChatMessage,
        cancel: CancellationToken,
    ) -> RoundOutcome {
        if self.status.is_none()
            && let Err(error) = store.start_lane(run, &self.name, &self.model_name).await
        {
            tracing::warn!(%error, "could not record the lane start");
        }
        // The earlier messages are already in the live view.
        let seen = self.messages.len();
        self.round_starts.push(seen);
        let mut opening = std::mem::take(&mut self.messages);
        opening.push(message);
        let mut agent = Agent::new(
            Arc::clone(&self.model),
            self.tools.clone(),
            self.system.clone(),
            self.limits,
        );
        if let Some(continuation) = &self.continuation {
            agent = agent.with_continuation(continuation());
        }
        let (mut outcome, _) = run_recorded(
            agent,
            opening,
            cancel,
            store,
            run,
            &self.name,
            &self.model_name,
            self.limits.record_argument_bytes,
            seen,
        )
        .await;
        let status = lane_status(&outcome.stop);
        for firing in &outcome.repeats {
            let _ = store
                .event(run, "warn", &repeat_event(&self.name, firing))
                .await;
        }
        end_events(store, run, &self.name, &outcome, status.1.is_some()).await;
        let round_turns = outcome.turns;
        self.turns = self.turns.saturating_add(round_turns);
        self.usage.add(&outcome.usage);
        outcome.turns = self.turns;
        outcome.usage = self.usage;
        // The transcript holds the rounds compaction dropped as well.
        let kept = outcome.messages.len();
        outcome.messages.splice(0..0, self.dropped.iter().cloned());
        keep_transcript(
            store,
            run,
            &self.name,
            &self.model_name,
            &self.system,
            &outcome,
        )
        .await;
        info!(lane = %self.name, turns = round_turns, status = ?status.0, "round ended");
        self.status = Some(status);
        let first_kept = outcome.messages.len().saturating_sub(kept);
        self.messages = outcome.messages.split_off(first_kept);
        RoundOutcome {
            stop: outcome.stop,
            turns: round_turns,
            final_text: outcome.final_text,
        }
    }

    /// Ends the session's lane row with the turns and tokens of every
    /// round, as its last round ended. A session that never ran has none.
    pub async fn finish(&self, store: &dyn RunStore, run: &RunId) {
        let Some((status, error)) = &self.status else {
            return;
        };
        if let Err(store_error) = store
            .finish_lane(
                run,
                &self.name,
                *status,
                u64::from(self.turns),
                self.usage.prompt_tokens(),
                self.usage.output_tokens,
                error.as_deref(),
            )
            .await
        {
            tracing::warn!(error = %store_error, "could not record the lane end");
        }
    }
}

/// The session's last lines on the run's timeline: how it stopped with its
/// last words, and what prompt caching saved when it saved anything.
async fn end_events(
    store: &dyn RunStore,
    run: &RunId,
    name: &str,
    outcome: &henk_agent::AgentOutcome,
    failed: bool,
) {
    let last_words: String = outcome.final_text.chars().take(200).collect();
    let level = if failed || matches!(outcome.stop, StopCause::Timeout) {
        "warn"
    } else {
        "info"
    };
    let _ = store
        .event(
            run,
            level,
            &format!(
                "{}: {:?} after {} turns; last words: {last_words}",
                name, outcome.stop, outcome.turns
            ),
        )
        .await;
    let usage = &outcome.usage;
    if usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0 {
        let _ = store
            .event(
                run,
                "info",
                &format!(
                    "{}: cache: {} of {} prompt tokens read from cache, {} written",
                    name,
                    usage.cache_read_tokens,
                    usage.prompt_tokens(),
                    usage.cache_write_tokens
                ),
            )
            .await;
    }
}

/// Runs `agent` and puts every tool call on the run as it happens (#190),
/// so the dashboard sees a session's calls while it runs, and passes every
/// message to the store's live view (#238). The recorder ends
/// when the agent, and with it the sender, is gone. Returns the outcome and
/// the calls recorded.
#[expect(
    clippy::too_many_arguments,
    reason = "the session's own inputs, passed through from run_session"
)]
async fn run_recorded(
    agent: Agent,
    opening: Vec<ChatMessage>,
    cancel: CancellationToken,
    store: &dyn RunStore,
    run: &RunId,
    session: &str,
    model: &str,
    cap: usize,
    seen: usize,
) -> (henk_agent::AgentOutcome, Vec<ToolCallRecord>) {
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let agent = agent.with_events(events);
    let recorder = async {
        let mut calls = Vec::new();
        // The first `seen` opening messages were sent in an earlier round
        // of the same session (#284).
        let mut skip = seen;
        while let Some(event) = received.recv().await {
            // What the session says goes to the live view as it happens
            // (#238); the transcript kept at the end is the record.
            if let AgentEvent::Message { turn, message } = &event {
                if *turn == 0 && skip > 0 {
                    skip -= 1;
                    continue;
                }
                match serde_json::to_string(message) {
                    Ok(json) => store.session_message(run, session, *turn, &json).await,
                    Err(error) => tracing::warn!(%error, "could not serialise a message"),
                }
                continue;
            }
            let Some(call) = call_record(session, model, cap, event) else {
                continue;
            };
            if let Err(error) = store.record_tool_call(run, &call).await {
                tracing::warn!(%error, "could not record a tool call");
            }
            calls.push(call);
        }
        calls
    };
    tokio::join!(
        async move {
            let outcome = agent.run(opening, cancel).await;
            drop(agent);
            outcome
        },
        recorder
    )
}

/// The run record of one `ToolCalled` event, its arguments cut to `cap`
/// bytes on a character boundary; none for any other event.
fn call_record(
    session: &str,
    model: &str,
    cap: usize,
    event: AgentEvent,
) -> Option<ToolCallRecord> {
    let AgentEvent::ToolCalled {
        turn,
        name,
        origin,
        arguments,
        outcome,
        elapsed,
        result_chars,
    } = event
    else {
        return None;
    };
    let mut end = arguments.len().min(cap);
    while !arguments.is_char_boundary(end) {
        end -= 1;
    }
    Some(ToolCallRecord {
        at: String::new(),
        session: session.to_owned(),
        model: model.to_owned(),
        turn,
        tool: name,
        origin,
        outcome: outcome.as_str().to_owned(),
        arguments: arguments.get(..end).unwrap_or_default().to_owned(),
        arguments_len: u64::try_from(arguments.len()).unwrap_or(u64::MAX),
        result_chars: u64::try_from(result_chars).unwrap_or(u64::MAX),
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    })
}

/// One session's tool usage on one log line: `read_file 12 (1 error),
/// search 3`, in tool order.
fn usage_line(usage: &[ToolUsage]) -> String {
    usage
        .iter()
        .map(|u| {
            let t = &u.tally;
            let mut notes = Vec::new();
            for (n, what) in [
                (t.errors, "error"),
                (t.refusals, "refused"),
                (t.other, "not run"),
            ] {
                if n > 0 {
                    notes.push(format!("{n} {what}"));
                }
            }
            if notes.is_empty() {
                format!("{} {}", u.tool, t.calls)
            } else {
                format!("{} {} ({})", u.tool, t.calls, notes.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The timeline line for one firing of the repeat guard.
fn repeat_event(lane: &str, firing: &RepeatFiring) -> String {
    let what = if firing.ended {
        "the session was ended as stuck"
    } else {
        "the call was refused"
    };
    format!(
        "{lane}: the model called {} with the same arguments {} times in a row; {what}",
        firing.tool, firing.repeats
    )
}

/// The read tools of one platform MCP session that `scope` allows, each
/// wrapped so that every call passes [`henk_domain::scope::guard`] first.
/// `exclude` names server tools to leave out even though the scope allows
/// them, for a tool known to be useless on this repository.
///
/// # Errors
///
/// Returns the [`McpError`] of listing the server's tools.
pub async fn platform_tools(
    session: Arc<dyn McpSession>,
    platform: Platform,
    scope: Scope,
    exclude: &[&str],
) -> Result<Vec<McpTool>, McpError> {
    let guard: henk_agent::Guard = Arc::new(move |tool: &str, args: &serde_json::Value| {
        match scope::guard(platform, tool, args, &scope) {
            scope::Verdict::Allow(rewritten) => Verdict::Allow(rewritten),
            scope::Verdict::Deny(reason) => Verdict::Deny(reason),
        }
    });
    let mut names = NameMap::new();
    mcp_tools(
        session,
        &mut names,
        |info| scope::is_exposed(platform, &info.name) && !exclude.contains(&info.name.as_str()),
        guard,
    )
    .await
}

/// The model id to put in markers: the model's own name, validated.
#[must_use]
pub fn model_id(model: &dyn ModelClient) -> ModelId {
    ModelId::parse(model.model().to_owned())
        .unwrap_or_else(|_| ModelId::parse("model").unwrap_or_else(|_| unreachable!("constant")))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unnecessary_wraps
    )]

    use std::time::Duration;

    use henk_agent::Tool as _;
    use henk_domain::allowlist::RepoRef;
    use henk_domain::review::CommitSha;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{Completion, LlmError, Role, StopReason};
    use henk_mcp::testing::{FakeServer, echo_behaviour};
    use serde_json::json;

    use super::*;

    fn text(text: &str) -> Result<Completion, LlmError> {
        Ok(Completion {
            message: ChatMessage::assistant(text),
            stop: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 12,
                output_tokens: 3,
                ..Usage::default()
            },
        })
    }

    fn run_id() -> RunId {
        RunId::parse("r-1").unwrap()
    }

    async fn store_with_run() -> henk_store::SqliteStore {
        let store = henk_store::SqliteStore::in_memory().unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run_id(),
                kind: henk_domain::run::RunKind::Plan,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 1,
                commit: None,
                requester: None,
                trigger: "test".into(),
                link: "l".into(),
            })
            .await
            .unwrap();
        store
    }

    fn spec(model: Arc<dyn ModelClient>) -> SessionSpec {
        SessionSpec {
            name: "planner".into(),
            model,
            system: "s".into(),
            opening: vec![ChatMessage::user("go")],
            tools: ToolSet::new(),
            limits: AgentConfig {
                max_turns: 3,
                timeout: Duration::from_secs(5),
                max_tool_output_chars: 100,
                max_conversation_chars: 100_000,
                keep_recent_turns: 2,
                max_repeated_calls: 3,
                record_argument_bytes: 4096,
            },
            continuation: None,
            turn_warning: None,
        }
    }

    /// A tool that answers `ok`, or fails when `fails`.
    struct Answer {
        name: &'static str,
        fails: bool,
    }

    #[async_trait::async_trait]
    impl henk_agent::Tool for Answer {
        fn definition(&self) -> henk_llm::ToolDef {
            henk_llm::ToolDef {
                name: henk_llm::ToolName::parse(self.name).unwrap(),
                description: String::new(),
                input_schema: json!({}),
            }
        }

        async fn call(&self, _: serde_json::Value) -> henk_agent::ToolOutput {
            if self.fails {
                henk_agent::ToolOutput::error("no")
            } else {
                henk_agent::ToolOutput::ok("fine")
            }
        }
    }

    fn calling(calls: &[(&str, &str, serde_json::Value)]) -> Result<Completion, LlmError> {
        Ok(Completion {
            message: ChatMessage {
                role: henk_llm::Role::Assistant,
                blocks: calls
                    .iter()
                    .map(|(id, name, arguments)| {
                        henk_llm::Block::ToolCall(henk_llm::ToolCall {
                            id: (*id).into(),
                            name: (*name).into(),
                            arguments: henk_llm::ToolArguments::Parsed(arguments.clone()),
                        })
                    })
                    .collect(),
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        })
    }

    #[tokio::test]
    async fn every_tool_call_of_a_session_is_on_the_run() {
        let store = store_with_run().await;
        let fake = FakeServer::new(
            vec![FakeServer::tool(
                "merge_pull_request",
                "Merges.",
                &["owner"],
            )],
            echo_behaviour(),
        );
        let session = Arc::new(fake.connect("github").await);
        let mut names = NameMap::new();
        let deny: henk_agent::Guard =
            Arc::new(|tool: &str, _: &serde_json::Value| Verdict::Deny(format!("{tool} no")));
        let mut tools = ToolSet::new();
        for tool in henk_agent::mcp_tools(session, &mut names, |_| true, deny)
            .await
            .unwrap()
        {
            tools.add(tool);
        }
        tools.add(Answer {
            name: "read",
            fails: false,
        });
        tools.add(Answer {
            name: "fail",
            fails: true,
        });
        let read = || ("r", "read", json!({"path": "a.rs"}));
        let model = Arc::new(ScriptedClient::new(
            "scripted",
            [
                calling(&[
                    read(),
                    ("f", "fail", json!({})),
                    ("m", "github__merge_pull_request", json!({"owner": "o"})),
                ]),
                calling(&[read()]),
                calling(&[read()]),
                text("Done."),
            ],
        ));
        let mut spec = spec(model);
        spec.tools = tools;
        spec.limits.max_turns = 6;
        spec.limits.max_repeated_calls = 1;
        run_session(&store, &run_id(), spec, CancellationToken::new()).await;

        let calls = store.tool_calls(&run_id()).await.unwrap();
        let shown: Vec<(u32, &str, &str, &str)> = calls
            .iter()
            .map(|c| {
                (
                    c.turn,
                    c.tool.as_str(),
                    c.origin.as_str(),
                    c.outcome.as_str(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (1, "read", "henk", "ok"),
                (1, "fail", "henk", "error"),
                (1, "github__merge_pull_request", "github", "refused_scope"),
                (2, "read", "henk", "ok"),
                (3, "read", "henk", "refused_repeat"),
            ]
        );
        assert!(
            calls
                .iter()
                .all(|c| c.session == "planner" && c.model == "scripted")
        );
        assert_eq!(calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(calls[0].result_chars, 4);
    }

    #[test]
    fn arguments_are_cut_on_a_character_and_their_length_kept() {
        let event = |arguments: &str| AgentEvent::ToolCalled {
            turn: 2,
            name: "bash".into(),
            origin: "workspace".into(),
            arguments: arguments.into(),
            outcome: henk_agent::CallOutcome::Ok,
            elapsed: Duration::from_millis(1500),
            result_chars: 10,
        };
        let cut = call_record("lane-a", "m", 3, event("a\u{e9}b")).unwrap();
        assert_eq!((cut.arguments.as_str(), cut.arguments_len), ("a\u{e9}", 4));
        let cut = call_record("lane-a", "m", 2, event("a\u{e9}b")).unwrap();
        assert_eq!(cut.arguments, "a", "never half a character");
        let none = call_record("lane-a", "m", 0, event("{}")).unwrap();
        assert_eq!((none.arguments.as_str(), none.arguments_len), ("", 2));
        assert_eq!(
            (none.elapsed_ms, none.turn, none.outcome.as_str()),
            (1500, 2, "ok")
        );
        let other = AgentEvent::RepeatRefused {
            tool: "x".into(),
            repeats: 2,
            ended: false,
        };
        assert!(call_record("lane-a", "m", 10, other).is_none());
    }

    #[test]
    fn the_usage_line_counts_per_tool_with_what_went_wrong() {
        let mut read = henk_store::ToolTally::default();
        read.add("ok", 10, 50);
        read.add("error", 1, 5);
        let mut bash = henk_store::ToolTally::default();
        bash.add("refused_repeat", 2, 0);
        bash.add("not_run", 1, 0);
        let row = |tool: &str, tally: henk_store::ToolTally| ToolUsage {
            session: "lane-a".into(),
            model: String::new(),
            tool: tool.into(),
            tally,
        };
        assert_eq!(
            usage_line(&[row("bash", bash), row("read_file", read)]),
            "bash 3 (2 refused, 1 not run), read_file 11 (1 error)"
        );
    }

    #[tokio::test]
    async fn a_finished_session_records_a_lane_row() {
        let store = store_with_run().await;
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new("m", [text("done")]));
        let outcome = run_session(&store, &run_id(), spec(model), CancellationToken::new()).await;
        assert!(outcome.finished());
        assert_eq!(outcome.final_text, "done");
        let lanes = store.lanes(&run_id()).await.unwrap();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].name, "planner");
        assert_eq!(lanes[0].model, "m");
        assert_eq!(lanes[0].status, LaneStatus::Finished);
        assert_eq!(lanes[0].input_tokens, 12);
        assert_eq!(store.events(&run_id()).await.unwrap().len(), 1);

        let listed = store.transcripts(&run_id()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (
                listed[0].session.as_str(),
                listed[0].model.as_str(),
                listed[0].turns
            ),
            ("planner", "m", 1)
        );
        let stored = store
            .transcript(&run_id(), "planner")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes, stored.body.len() as u64);
        let body: serde_json::Value = serde_json::from_str(&stored.body).unwrap();
        assert_eq!(body["session"], "planner");
        assert_eq!(body["stop"], "EndTurn", "{body}");
        assert_eq!(body["system"], "s");
        assert_eq!(
            body["messages"],
            serde_json::to_value([ChatMessage::user("go"), ChatMessage::assistant("done")])
                .unwrap(),
            "the whole conversation, as the model saw it"
        );
    }

    #[tokio::test]
    async fn a_resumed_session_sees_its_earlier_rounds_and_keeps_one_lane() {
        let store = store_with_run().await;
        let scripted = Arc::new(ScriptedClient::new("m", [text("first"), text("second")]));
        let model: Arc<dyn ModelClient> = scripted.clone();
        let mut session = ResumableSession::new(
            "reviewer",
            model,
            "s",
            ToolSet::new(),
            AgentConfig::default(),
        );
        let one = session
            .resume(
                &store,
                &run_id(),
                ChatMessage::user("go"),
                CancellationToken::new(),
            )
            .await;
        assert_eq!((one.final_text.as_str(), one.turns), ("first", 1));
        let two = session
            .resume(
                &store,
                &run_id(),
                ChatMessage::user("again"),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(two.final_text, "second");

        let requests = scripted.requests();
        assert_eq!(
            serde_json::to_value(&requests[1].messages).unwrap(),
            serde_json::to_value([
                ChatMessage::user("go"),
                ChatMessage::assistant("first"),
                ChatMessage::user("again"),
            ])
            .unwrap(),
            "the second round goes on from the first"
        );
        assert_eq!(session.messages().len(), 4);

        session.finish(&store, &run_id()).await;
        let lanes = store.lanes(&run_id()).await.unwrap();
        assert_eq!(lanes.len(), 1, "one lane row for every round");
        assert_eq!(
            (lanes[0].status, lanes[0].turns, lanes[0].input_tokens),
            (LaneStatus::Finished, 2, 24)
        );
        let stored = store
            .transcript(&run_id(), "reviewer")
            .await
            .unwrap()
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&stored.body).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 4, "{body}");
        assert_eq!(body["turns"], 2);
    }

    #[tokio::test]
    async fn rounds_past_the_budget_give_way_to_their_summary() {
        let store = store_with_run().await;
        let answers = ["one", "two", "three", "four"].map(text);
        let scripted = Arc::new(ScriptedClient::new("m", answers));
        let model: Arc<dyn ModelClient> = scripted.clone();
        let limits = AgentConfig {
            max_conversation_chars: 10,
            ..AgentConfig::default()
        };
        let mut session = ResumableSession::new("fixer", model, "s", ToolSet::new(), limits);
        for round in ["round 1", "round 2", "round 3"] {
            session
                .resume(
                    &store,
                    &run_id(),
                    ChatMessage::user(round),
                    CancellationToken::new(),
                )
                .await;
        }
        assert!(session.compact_rounds("f1 fixed in abc.", 1));
        let kept: Vec<String> = session.messages().iter().map(ChatMessage::text).collect();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(
            kept[0].starts_with("What happened in the earlier rounds"),
            "{kept:?}"
        );
        assert!(kept[0].contains("f1 fixed in abc.") && kept[0].ends_with("round 3"));
        assert_eq!(kept[1], "three");
        assert!(!session.compact_rounds("again", 1), "one round left");

        session
            .resume(
                &store,
                &run_id(),
                ChatMessage::user("round 4"),
                CancellationToken::new(),
            )
            .await;
        let last = scripted.requests().pop().unwrap();
        let roles: Vec<_> = last.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::User]);
        assert!(last.messages[0].text().contains("f1 fixed in abc."));

        let roomy = AgentConfig::default();
        let model: Arc<dyn ModelClient> =
            Arc::new(ScriptedClient::new("m", [text("a"), text("b")]));
        let mut small = ResumableSession::new("reviewer", model, "s", ToolSet::new(), roomy);
        for round in ["1", "2"] {
            small
                .resume(
                    &store,
                    &run_id(),
                    ChatMessage::user(round),
                    CancellationToken::new(),
                )
                .await;
        }
        assert!(!small.compact_rounds("s", 1), "within budget, nothing goes");
    }

    /// A compaction edits the history, so the kept rounds lose their
    /// thinking blocks; the stored transcript still holds every round.
    #[tokio::test]
    async fn a_compaction_drops_thinking_and_keeps_the_whole_transcript() {
        let store = store_with_run().await;
        let mut thought = ChatMessage::assistant("three");
        thought
            .blocks
            .insert(0, henk_llm::Block::Opaque(json!({"type": "thinking"})));
        let third = Ok(Completion {
            message: thought,
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        });
        let answers = [text("one"), text("two"), third, text("four")];
        let scripted = Arc::new(ScriptedClient::new("m", answers));
        let model: Arc<dyn ModelClient> = scripted.clone();
        let limits = AgentConfig {
            max_conversation_chars: 10,
            ..AgentConfig::default()
        };
        let mut session = ResumableSession::new("fixer", model, "s", ToolSet::new(), limits);
        let round = async |session: &mut ResumableSession, text: &str| {
            session
                .resume(
                    &store,
                    &run_id(),
                    ChatMessage::user(text),
                    CancellationToken::new(),
                )
                .await;
        };
        for text in ["round 1", "round 2", "round 3"] {
            round(&mut session, text).await;
        }
        assert!(session.compact_rounds("f1 fixed in abc.", 1));
        round(&mut session, "round 4").await;

        let sent = scripted.requests().pop().unwrap();
        assert!(
            sent.messages.iter().all(|m| !m
                .blocks
                .iter()
                .any(|b| matches!(b, henk_llm::Block::Opaque(_)))),
            "no thinking replayed after the edit"
        );
        assert_eq!(sent.messages.len(), 3);
        let stored = store.transcript(&run_id(), "fixer").await.unwrap().unwrap();
        let body: serde_json::Value = serde_json::from_str(&stored.body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 8, "every round: {body}");
        assert!(messages[0].to_string().contains("round 1"), "{body}");
        assert_eq!(session.messages().len(), 4, "the session keeps two rounds");
    }

    /// A store that takes nothing, here because the run is not in it, costs
    /// the records and the transcript, never the session.
    #[tokio::test]
    async fn a_store_that_refuses_the_transcript_still_ends_the_session() {
        let store = henk_store::SqliteStore::in_memory().unwrap();
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new("m", [text("done")]));
        let outcome = run_session(&store, &run_id(), spec(model), CancellationToken::new()).await;
        assert!(outcome.finished());
        assert_eq!(outcome.final_text, "done");
        assert!(store.transcripts(&run_id()).await.unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_time_limit_ends_the_session_as_timed_out_not_as_dropped() {
        let store = store_with_run().await;
        let model: Arc<dyn ModelClient> =
            Arc::new(ScriptedClient::new("m", [text("late")]).with_delay(Duration::from_secs(30)));
        let mut spec = spec(model);
        spec.limits.timeout = Duration::from_millis(50);
        let outcome = run_session(&store, &run_id(), spec, CancellationToken::new()).await;
        assert!(matches!(outcome.stop, StopCause::Timeout));
        assert!(outcome.finished());
        assert_eq!(outcome.error, None);
        let lanes = store.lanes(&run_id()).await.unwrap();
        assert_eq!(lanes[0].status, LaneStatus::TimedOut);
        assert_eq!(lanes[0].error, None);
        let events = store.events(&run_id()).await.unwrap();
        assert_eq!(events[0].level, "warn");
        assert!(
            events[0].message.contains("Timeout"),
            "{}",
            events[0].message
        );
    }

    #[tokio::test]
    async fn a_model_error_drops_the_session_with_the_error_text() {
        let store = store_with_run().await;
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new(
            "m",
            [Err(LlmError::Unauthorized {
                status: 401,
                body: "nope".into(),
            })],
        ));
        let outcome = run_session(&store, &run_id(), spec(model), CancellationToken::new()).await;
        assert!(!outcome.finished());
        assert_eq!(outcome.status, LaneStatus::DidNotFinish);
        assert!(outcome.error.as_deref().unwrap_or("").contains("401"));
        let lanes = store.lanes(&run_id()).await.unwrap();
        assert_eq!(lanes[0].status, LaneStatus::DidNotFinish);
    }

    #[tokio::test]
    async fn a_refusal_drops_the_session_and_says_the_model_declined() {
        let store = store_with_run().await;
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new(
            "m",
            [Ok(Completion {
                message: henk_llm::ChatMessage::assistant(""),
                stop: henk_llm::StopReason::Refused("refusal".into()),
                usage: henk_llm::Usage::default(),
            })],
        ));
        let outcome = run_session(&store, &run_id(), spec(model), CancellationToken::new()).await;
        assert!(!outcome.finished(), "a refusal is not a finished lane");
        assert_eq!(outcome.status, LaneStatus::DidNotFinish);
        assert_eq!(
            outcome.error.as_deref(),
            Some("the model declined (refusal)")
        );
        let lanes = store.lanes(&run_id()).await.unwrap();
        assert_eq!(lanes[0].status, LaneStatus::DidNotFinish);
        assert_eq!(
            lanes[0].error.as_deref(),
            Some("the model declined (refusal)")
        );
    }

    fn same_call(id: &str) -> Result<Completion, LlmError> {
        Ok(Completion {
            message: ChatMessage {
                role: henk_llm::Role::Assistant,
                blocks: vec![henk_llm::Block::ToolCall(henk_llm::ToolCall {
                    id: id.into(),
                    // No such tool: the guard looks at the call, not at
                    // whether it could run.
                    name: "read".into(),
                    arguments: henk_llm::ToolArguments::Parsed(json!({"path": "a.rs"})),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        })
    }

    #[tokio::test]
    async fn repeat_guard_firings_are_recorded_on_the_run() {
        let store = store_with_run().await;
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new(
            "m",
            [same_call("c1"), same_call("c2"), same_call("c3")],
        ));
        let mut spec = spec(model);
        spec.limits.max_repeated_calls = 1;
        let outcome = run_session(&store, &run_id(), spec, CancellationToken::new()).await;
        assert!(matches!(&outcome.stop, StopCause::Stuck { tool, repeats: 3 } if tool == "read"));
        assert_eq!(outcome.status, LaneStatus::DidNotFinish);
        assert_eq!(outcome.error.as_deref(), Some("stuck repeating read"));
        let lanes = store.lanes(&run_id()).await.unwrap();
        assert_eq!(lanes[0].error.as_deref(), Some("stuck repeating read"));
        let events = store.events(&run_id()).await.unwrap();
        let timeline: Vec<(&str, &str)> = events
            .iter()
            .map(|e| (e.level.as_str(), e.message.as_str()))
            .collect();
        assert_eq!(
            timeline[..2],
            [
                (
                    "warn",
                    "planner: the model called read with the same arguments 2 times in a row; \
                     the call was refused"
                ),
                (
                    "warn",
                    "planner: the model called read with the same arguments 3 times in a row; \
                     the session was ended as stuck"
                ),
            ]
        );
        assert_eq!(events.len(), 3, "and the session summary");
        assert_eq!(events[2].level, "warn");
        assert!(
            events
                .iter()
                .all(|e| henk_domain::text::is_in_style(&e.message))
        );
    }

    #[tokio::test]
    async fn platform_tools_exposes_only_the_scope_table_and_pins_arguments() {
        let fake = FakeServer::new(
            vec![
                FakeServer::tool(
                    "pull_request_read",
                    "",
                    &["owner", "repo", "pullNumber", "method"],
                ),
                FakeServer::tool("merge_pull_request", "", &["owner"]),
            ],
            echo_behaviour(),
        );
        let session: Arc<dyn McpSession> = Arc::new(fake.connect("github").await);
        let scope = Scope::Review {
            repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            number: 7,
            commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
        };
        let tools = platform_tools(Arc::clone(&session), Platform::GitHub, scope.clone(), &[])
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        let excluded = platform_tools(session, Platform::GitHub, scope, &["pull_request_read"])
            .await
            .unwrap();
        assert!(excluded.is_empty(), "an excluded tool is not exposed");
        assert_eq!(
            tools[0].definition().name.as_str(),
            "github__pull_request_read"
        );
        let refused = tools[0]
            .call(json!({"owner": "evil", "repo": "x", "pullNumber": 1, "method": "get"}))
            .await;
        assert!(
            refused.is_error,
            "another repository is refused, not swapped"
        );
        assert!(fake.calls().is_empty(), "nothing reached the server");
        let output = tools[0].call(json!({"method": "get"})).await;
        assert!(!output.is_error, "{output:?}");
        assert_eq!(fake.calls()[0].arguments["owner"], "docspec");
        assert_eq!(fake.calls()[0].arguments["pullNumber"], 7);
    }

    #[test]
    fn model_id_falls_back_for_unusable_names() {
        let good = ScriptedClient::new("claude-x", []);
        assert_eq!(model_id(&good).as_str(), "claude-x");
        let bad = ScriptedClient::new("has space", []);
        assert_eq!(model_id(&bad).as_str(), "model");
    }
}
