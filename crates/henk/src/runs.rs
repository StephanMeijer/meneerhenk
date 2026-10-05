//! `henk runs show`: a run record as text, read from the local database.

use std::fmt::Write as _;

use henk_store::{EventRecord, FindingRecord, LaneRecord, RunRecord};

/// Renders one run with its lanes, findings and timeline.
#[must_use]
pub fn render(
    run: &RunRecord,
    lanes: &[LaneRecord],
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
        let text = render(&run, &lanes, &findings, &events);
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
}
