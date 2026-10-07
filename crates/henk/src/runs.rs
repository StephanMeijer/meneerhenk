//! `henk runs show`: a run record as text, read from the local database.

use std::fmt::Write as _;

use henk_store::{
    EventRecord, FindingRecord, LaneRecord, RunRecord, ToolTally, ToolUsage, TranscriptSummary,
};
use serde_json::Value;

/// One tool's calls in words: `12 calls, 1 error, 840 ms`.
#[must_use]
pub fn tally_text(tally: &ToolTally) -> String {
    let mut parts = vec![match tally.calls {
        1 => "1 call".to_owned(),
        n => format!("{n} calls"),
    }];
    for (n, one, many) in [
        (tally.errors, "error", "errors"),
        (tally.refusals, "refused", "refused"),
        (tally.other, "not run", "not run"),
    ] {
        match n {
            0 => {}
            1 => parts.push(format!("1 {one}")),
            n => parts.push(format!("{n} {many}")),
        }
    }
    parts.push(format!("{} ms", tally.total_ms));
    parts.join(", ")
}

/// A stored transcript (#191), read back for people: the `henk runs show
/// --transcript` text and the dashboard page both render this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptView {
    /// The session's name.
    pub session: String,
    /// The model it ran on.
    pub model: String,
    /// Why it stopped.
    pub stop: String,
    /// Model turns taken.
    pub turns: u64,
    /// Prompt tokens, all of them, and tokens generated.
    pub tokens: (u64, u64),
    /// The system prompt.
    pub system: String,
    /// The conversation, in order.
    pub messages: Vec<MessageView>,
}

/// One message of a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// `user` or `assistant`.
    pub role: String,
    /// The model turn this message belongs to: the assistant's message is
    /// turn n, and the results it is answered with are of turn n too.
    pub turn: u64,
    /// Its content, in order.
    pub parts: Vec<PartView>,
}

/// One block of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartView {
    /// Text.
    Text(String),
    /// A call the model made, with its arguments as JSON or as the model
    /// wrote them when they were not JSON.
    Call {
        /// The tool.
        name: String,
        /// The arguments.
        arguments: String,
    },
    /// What a call returned.
    Result {
        /// Whether the tool failed.
        error: bool,
        /// What it returned.
        content: String,
    },
    /// Provider content the model wanted echoed back, such as thinking.
    Opaque,
}

impl TranscriptView {
    /// Reads a stored transcript's JSON. Anything it does not know is
    /// skipped, so an older or newer transcript still reads.
    ///
    /// # Errors
    ///
    /// Returns the error when the body is not JSON.
    pub fn parse(body: &str) -> Result<Self, serde_json::Error> {
        let value: Value = serde_json::from_str(body)?;
        let text = |key: &str| field(&value, key).as_str().unwrap_or_default().to_owned();
        let usage = field(&value, "usage");
        let count = |key: &str| field(usage, key).as_u64().unwrap_or(0);
        let mut turn = 0;
        let messages = field(&value, "messages")
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .map(|message| {
                        let role = field(message, "role")
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        if role == "assistant" {
                            turn += 1;
                        }
                        let parts = field(message, "blocks")
                            .as_array()
                            .map(|blocks| blocks.iter().filter_map(part).collect())
                            .unwrap_or_default();
                        MessageView { role, turn, parts }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            session: text("session"),
            model: text("model"),
            stop: text("stop"),
            turns: field(&value, "turns").as_u64().unwrap_or(0),
            tokens: (
                count("input_tokens") + count("cache_read_tokens") + count("cache_write_tokens"),
                count("output_tokens"),
            ),
            system: text("system"),
            messages,
        })
    }
}

/// `value[key]`, or null.
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

fn part(block: &Value) -> Option<PartView> {
    let (kind, inner) = block.as_object()?.iter().next()?;
    Some(match kind.as_str() {
        "text" => PartView::Text(inner.as_str()?.to_owned()),
        "tool_call" => PartView::Call {
            name: field(inner, "name").as_str().unwrap_or_default().to_owned(),
            arguments: match &field(inner, "arguments") {
                Value::Object(arguments) => match arguments.iter().next() {
                    Some((_, Value::String(raw))) => raw.clone(),
                    Some((_, parsed)) => parsed.to_string(),
                    None => String::new(),
                },
                other => other.to_string(),
            },
        },
        "tool_result" => PartView::Result {
            error: field(inner, "is_error").as_bool().unwrap_or(false),
            content: field(inner, "content")
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        },
        "opaque" => PartView::Opaque,
        _ => return None,
    })
}

/// A stored transcript as text, for `henk runs show --transcript`.
///
/// # Errors
///
/// Returns the error when the body is not JSON.
pub fn transcript_text(body: &str) -> Result<String, serde_json::Error> {
    let view = TranscriptView::parse(body)?;
    let mut out = String::new();
    let _ = writeln!(out, "transcript {}", view.session);
    let _ = writeln!(
        out,
        "  model {}, stopped {}, {} turns, tokens in {} out {}",
        view.model, view.stop, view.turns, view.tokens.0, view.tokens.1
    );
    let _ = writeln!(out, "\nsystem\n{}", indent(&view.system));
    for message in &view.messages {
        let _ = writeln!(out, "\n{} (turn {})", message.role, message.turn);
        for part in &message.parts {
            let _ = match part {
                PartView::Text(text) => writeln!(out, "{}", indent(text)),
                PartView::Call { name, arguments } => {
                    writeln!(out, "  call {name} {arguments}")
                }
                PartView::Result { error, content } => writeln!(
                    out,
                    "  result {}\n{}",
                    if *error { "error" } else { "ok" },
                    indent(&indent(content))
                ),
                PartView::Opaque => writeln!(out, "  (provider content, not shown)"),
            };
        }
    }
    Ok(out)
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders one run with its lanes, tool calls, transcripts, findings and
/// timeline.
#[must_use]
pub fn render(
    run: &RunRecord,
    lanes: &[LaneRecord],
    tools: &[ToolUsage],
    transcripts: &[TranscriptSummary],
    findings: &[FindingRecord],
    events: &[EventRecord],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "run {}", run.id);
    let _ = writeln!(
        out,
        "  {:?} of {} {} #{} ({:?})",
        run.kind, run.platform, run.repo, run.target, run.status
    );
    if let Some(commit) = &run.commit {
        let _ = writeln!(out, "  commit     {commit}");
    }
    let _ = writeln!(out, "  trigger    {}", run.trigger);
    if let Some(requester) = &run.requester {
        let _ = writeln!(out, "  requester  {requester}");
    }
    let _ = writeln!(out, "  started    {}", run.started_at);
    if let Some(finished) = &run.finished_at {
        let _ = writeln!(out, "  finished   {finished}");
    }
    let _ = writeln!(out, "  link       {}", run.link);
    if let Some(summary) = &run.summary {
        let _ = writeln!(
            out,
            "  summary    {}",
            summary.replace('\n', "\n             ")
        );
    }
    if let Some(error) = &run.error {
        let _ = writeln!(out, "  error      {error}");
    }

    let _ = writeln!(out, "\nlanes ({})", lanes.len());
    for lane in lanes {
        let _ = write!(
            out,
            "  {:<12} {:<9} {:<10} turns {:>3}  tokens in {:>7} out {:>6}",
            lane.name,
            format!("{:?}", lane.status).to_lowercase(),
            lane.model,
            lane.turns,
            lane.input_tokens,
            lane.output_tokens
        );
        match &lane.error {
            Some(error) => {
                let _ = writeln!(out, "  {error}");
            }
            None => out.push('\n'),
        }
    }

    if !tools.is_empty() {
        let calls: u64 = tools.iter().map(|t| t.tally.calls).sum();
        let _ = writeln!(out, "\ntool calls ({calls})");
        let mut session = "";
        for usage in tools {
            if usage.session != session {
                session = &usage.session;
                let _ = writeln!(out, "  {session}");
            }
            let _ = writeln!(out, "    {:<28} {}", usage.tool, tally_text(&usage.tally));
        }
    }

    if !transcripts.is_empty() {
        let _ = writeln!(
            out,
            "\ntranscripts ({}), read one with --transcript <session>",
            transcripts.len()
        );
        for transcript in transcripts {
            let _ = writeln!(
                out,
                "  {:<28} {:<9} turns {:>3}  {} bytes",
                transcript.session,
                transcript.stop.to_lowercase(),
                transcript.turns,
                transcript.bytes
            );
        }
    }

    let _ = writeln!(out, "\nfindings ({})", findings.len());
    for finding in findings {
        let _ = writeln!(
            out,
            "  {} {:<8} {:<12} {}:{} comment {}",
            finding.at,
            finding.action,
            finding.lane,
            finding.path,
            finding.line,
            finding.comment_id
        );
    }

    let _ = writeln!(out, "\ntimeline ({})", events.len());
    for event in events {
        let _ = writeln!(out, "  {} {:<5} {}", event.at, event.level, event.message);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::allowlist::Platform;
    use henk_domain::run::{RunId, RunKind};
    use henk_store::{LaneStatus, RunStatus};

    use super::*;

    #[test]
    fn renders_every_section() {
        let run = RunRecord {
            id: RunId::parse("r-1").unwrap(),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "o/r".into(),
            target: 7,
            commit: Some("abc".into()),
            requester: None,
            trigger: "cli".into(),
            status: RunStatus::Finished,
            started_at: "t0".into(),
            finished_at: Some("t1".into()),
            link: "http://x/runs/r-1".into(),
            summary: Some("Review of abc: 1 finding.\nSecond line.".into()),
            error: None,
            heartbeat_at: None,
            check_id: None,
        };
        let lanes = vec![LaneRecord {
            name: "lane-a".into(),
            model: "m".into(),
            status: LaneStatus::Dropped,
            turns: 3,
            input_tokens: 100,
            output_tokens: 20,
            error: Some("timed out".into()),
        }];
        let findings = vec![FindingRecord {
            at: "t".into(),
            lane: "lane-a".into(),
            path: "src/x.rs".into(),
            line: 12,
            comment_id: "c1".into(),
            action: "posted".into(),
        }];
        let events = vec![EventRecord {
            at: "t".into(),
            level: "warn".into(),
            message: "lane-a: could not post".into(),
        }];
        let transcripts = vec![TranscriptSummary {
            at: "t".into(),
            session: "lane-a".into(),
            model: "m".into(),
            stop: "EndTurn".into(),
            turns: 3,
            bytes: 1234,
        }];
        let text = render(&run, &lanes, &[], &transcripts, &findings, &events);
        assert!(text.starts_with("run r-1\n"));
        assert!(text.contains("commit     abc"));
        assert!(text.contains("lanes (1)"));
        assert!(text.contains("lane-a       dropped"));
        assert!(text.contains("timed out"));
        assert!(text.contains("src/x.rs:12 comment c1"));
        assert!(text.contains("timeline (1)"));
        assert!(text.contains("warn  lane-a: could not post"));
        assert!(text.contains("             Second line."), "{text}");
        assert!(text.contains("transcripts (1)"), "{text}");
        assert!(
            text.contains("lane-a                       endturn   turns   3  1234 bytes"),
            "{text}"
        );
    }

    /// A transcript as `henk-session` stores it: text, a tool call, an
    /// error result and provider content.
    const STORED: &str = r#"{"run":"r-1","session":"lane-a","model":"m","stop":"EndTurn","turns":2,
        "usage":{"input_tokens":10,"output_tokens":5,"cache_read_tokens":3,"cache_write_tokens":0},
        "system":"You review.\nCarefully.",
        "messages":[
          {"role":"user","blocks":[{"text":"Review <this>."}]},
          {"role":"assistant","blocks":[{"opaque":{"thinking":"x"}},{"tool_call":{"id":"c1","name":"read_file","arguments":{"parsed":{"path":"a.rs"}}}},{"tool_call":{"id":"c2","name":"search","arguments":{"malformed":"{oops"}}}]},
          {"role":"user","blocks":[{"tool_result":{"call_id":"c1","content":"fn main() {}","is_error":false}},{"tool_result":{"call_id":"c2","content":"bad arguments","is_error":true}}]},
          {"role":"assistant","blocks":[{"text":"No findings."}]}
        ]}"#;

    #[test]
    fn a_transcript_reads_as_its_conversation() {
        let view = TranscriptView::parse(STORED).unwrap();
        assert_eq!(view.tokens, (13, 5));
        assert_eq!(
            view.messages.iter().map(|m| m.turn).collect::<Vec<_>>(),
            [0, 1, 1, 2]
        );
        let text = transcript_text(STORED).unwrap();
        for expected in [
            "transcript lane-a\n",
            "model m, stopped EndTurn, 2 turns, tokens in 13 out 5",
            "system\n  You review.\n  Carefully.",
            "user (turn 0)\n  Review <this>.",
            "assistant (turn 1)\n  (provider content, not shown)",
            "  call read_file {\"path\":\"a.rs\"}",
            "  call search {oops",
            "  result ok\n    fn main() {}",
            "  result error\n    bad arguments",
            "assistant (turn 2)\n  No findings.",
        ] {
            assert!(text.contains(expected), "{expected:?} in\n{text}");
        }
        assert!(transcript_text("not json").is_err());
    }

    #[test]
    fn a_transcript_from_another_version_still_reads() {
        let view =
            TranscriptView::parse(r#"{"session":"s","messages":[{"role":"assistant","blocks":[{"image":1},{"text":"hi"}]}]}"#)
                .unwrap();
        assert_eq!(view.messages[0].parts, [PartView::Text("hi".into())]);
        assert_eq!(view.turns, 0);
    }

    /// A model asking for the same call each turn, as `henk runs show`
    /// sees it once the session is recorded.
    #[tokio::test]
    async fn the_repeat_guard_shows_on_the_run() {
        use std::sync::Arc;

        use henk_agent::{AgentConfig, ToolSet};
        use henk_llm::testing::ScriptedClient;
        use henk_llm::{
            Block, ChatMessage, Completion, Role, StopReason, ToolArguments, ToolCall, Usage,
        };
        use henk_session::{SessionSpec, run_session};
        use henk_store::{NewRun, RunStore, SqliteStore};
        use tokio_util::sync::CancellationToken;

        let same = || {
            Ok(Completion {
                message: ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolCall(ToolCall {
                        id: "c".into(),
                        name: "get_file_diff".into(),
                        arguments: ToolArguments::Parsed(serde_json::json!({"path": "a.rs"})),
                    })],
                },
                stop: StopReason::ToolUse,
                usage: Usage::default(),
            })
        };
        let store = SqliteStore::in_memory().unwrap();
        let id = RunId::parse("r-stuck").unwrap();
        store
            .create_run(&NewRun {
                id: id.clone(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "test".into(),
                link: "l".into(),
            })
            .await
            .unwrap();
        let spec = SessionSpec {
            name: "lane-a".into(),
            model: Arc::new(ScriptedClient::new("m", (0..5).map(|_| same()))),
            system: "s".into(),
            opening: vec![ChatMessage::user("go")],
            tools: ToolSet::new(),
            limits: AgentConfig::default(),
            continuation: None,
            turn_warning: None,
        };
        run_session(&store, &id, spec, CancellationToken::new()).await;

        let text = render(
            &store.run(&id).await.unwrap().unwrap(),
            &store.lanes(&id).await.unwrap(),
            &ToolUsage::from_calls(&store.tool_calls(&id).await.unwrap()),
            &store.transcripts(&id).await.unwrap(),
            &store.findings(&id).await.unwrap(),
            &store.events(&id).await.unwrap(),
        );
        assert!(text.contains("stuck repeating get_file_diff"), "{text}");
        assert!(text.contains("transcripts (1)"), "{text}");
        let stored = store.transcript(&id, "lane-a").await.unwrap().unwrap();
        let conversation = transcript_text(&stored.body).unwrap();
        assert!(
            conversation.contains("user (turn 0)\n  go\n"),
            "{conversation}"
        );
        assert_eq!(
            conversation
                .matches("  call get_file_diff {\"path\":\"a.rs\"}")
                .count(),
            5,
            "every call the model made is in its transcript: {conversation}"
        );
        assert!(
            text.contains("tool calls (5)\n  lane-a\n    get_file_diff                5 calls, 2 refused, 3 not run, 0 ms\n"),
            "every call is on the run, even one that never ran: {text}"
        );
        assert!(
            text.contains(
                "warn  lane-a: the model called get_file_diff with the same arguments \
                 4 times in a row; the call was refused"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "warn  lane-a: the model called get_file_diff with the same arguments \
                 5 times in a row; the session was ended as stuck"
            ),
            "{text}"
        );
    }
}
