//! `henk runs show`: a run record as text, read from the local database.

use std::fmt::Write as _;

use henk_store::{EventRecord, FindingRecord, LaneRecord, RunRecord, ToolTally, ToolUsage};

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

/// Renders one run with its lanes, tool calls, findings and timeline.
#[must_use]
pub fn render(
    run: &RunRecord,
    lanes: &[LaneRecord],
    tools: &[ToolUsage],
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
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

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
        let text = render(&run, &lanes, &[], &findings, &events);
        assert!(text.starts_with("run r-1\n"));
        assert!(text.contains("commit     abc"));
        assert!(text.contains("lanes (1)"));
        assert!(text.contains("lane-a       dropped"));
        assert!(text.contains("timed out"));
        assert!(text.contains("src/x.rs:12 comment c1"));
        assert!(text.contains("timeline (1)"));
        assert!(text.contains("warn  lane-a: could not post"));
        assert!(text.contains("             Second line."), "{text}");
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
            &store.findings(&id).await.unwrap(),
            &store.events(&id).await.unwrap(),
        );
        assert!(text.contains("stuck repeating get_file_diff"), "{text}");
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
