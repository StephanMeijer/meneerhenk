//! What every [`RunStore`] backend must do. Each scenario runs against
//! `SQLite` always and against `PostgreSQL` when `HENK_TEST_DATABASE_URL` is
//! set (`cargo test -p henk-store -- --ignored`).

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::type_complexity
)]

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{
    DraftDecision, DraftFilter, DraftGroup, DraftKey, DraftRates, DraftRecord, DraftVerdict,
    EventFilter, EventKey, FindingAction, InboundEvent, LaneStatus, MAX_PAYLOAD_BYTES, NewRun,
    OutcomeFilter, OutcomeRecord, Page, PgStore, PruneCounts, RunFilter, RunKey, RunRecord,
    RunStatus, RunStore, SqliteStore, ToolCallFilter, ToolCallKey, ToolCallRecord,
    TranscriptRecord, VerdictFilter,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn new_run(id: &str) -> NewRun {
    NewRun {
        id: RunId::parse(id).unwrap(),
        kind: RunKind::Review,
        platform: Platform::GitHub,
        repo: "o/r".into(),
        target: 7,
        commit: Some("abc".into()),
        requester: None,
        trigger: "opened".into(),
        link: format!("https://henk.example/runs/{id}"),
    }
}

fn id(value: &str) -> RunId {
    RunId::parse(value).unwrap()
}

fn event_id(value: &str) -> EventId {
    EventId::parse(value).unwrap()
}

fn inbound(id: &str, received_at: &str) -> InboundEvent {
    InboundEvent {
        id: event_id(id),
        received_at: received_at.into(),
        source: "github_webhook".into(),
        kind: "pull_request".into(),
        repo: Some("o/r".into()),
        target: Some(7),
        payload: Some("{}".into()),
        requester: None,
    }
}

fn outcome(event: &str, listener: &str, run: Option<&str>) -> OutcomeRecord {
    OutcomeRecord {
        event_id: event_id(event),
        listener: listener.into(),
        outcome: if run.is_some() { "started" } else { "ignored" }.into(),
        detail: "detail".into(),
        run_id: run.map(Into::into),
        at: String::new(),
    }
}

/// Every scenario, by name. Each backend module turns them into tests.
macro_rules! for_each_scenario {
    ($tests:ident) => {
        $tests!(
            run_round_trips,
            a_heartbeat_moves_and_a_check_id_is_kept,
            only_running_runs_with_a_stale_heartbeat_are_orphaned,
            dropping_running_lanes_leaves_finished_ones_alone,
            lanes_findings_and_events_attach_to_a_run,
            events_and_outcomes_round_trip,
            old_events_and_their_outcomes_are_pruned,
            outcomes_and_linked_events_keep_their_order,
            a_duplicate_run_id_is_an_error,
            joining_is_accepted,
            runs_are_listed_newest_first_by_filter_and_page,
            runs_page_by_keyset_and_filter_by_target_and_time,
            a_whole_second_bound_holds_for_a_run_started_within_that_second,
            inbound_events_page_by_keyset_with_ties_on_the_time,
            inbound_events_are_listed_with_their_outcomes,
            a_number_beyond_i64_is_refused_not_stored_as_something_else,
            tool_calls_are_kept_in_order_and_add_up_across_runs,
            transcripts_are_kept_whole_listed_and_pruned,
            drafts_are_kept_replaced_and_decided,
            drafts_are_counted_by_group_and_listed_across_runs,
            tool_calls_are_tallied_and_listed_across_runs_by_filter,
        );
    };
}

fn draft(number: &str, lane: &str, row: u32, body: &str) -> DraftRecord {
    DraftRecord {
        at: String::new(),
        draft: number.into(),
        lane: lane.into(),
        model: "opus".into(),
        kind: "finding".into(),
        path: "src/a.rs".into(),
        line: row,
        target: String::new(),
        body: body.into(),
        decision: None,
    }
}

async fn tool_calls_are_tallied_and_listed_across_runs_by_filter(store: &dyn RunStore) {
    for run in ["r-1", "r-2", "r-3"] {
        store.create_run(&new_run(run)).await.unwrap();
    }
    // (run, session, model, tool, outcome, minute)
    let calls = [
        ("r-1", "lane-a", "mistral", "read_file", "ok", 1),
        ("r-1", "lane-a", "mistral", "read_file", "error", 2),
        (
            "r-1",
            "lane-a",
            "mistral",
            "github__get_file",
            "refused_scope",
            3,
        ),
        ("r-1", "check-1", "opus", "read_file", "ok", 4),
        (
            "r-2",
            "lane-b",
            "deepseek",
            "read_file",
            "refused_repeat",
            5,
        ),
        (
            "r-2",
            "lane-b",
            "deepseek",
            "bash",
            "malformed_arguments",
            6,
        ),
        ("r-2", "planner", "deepseek", "bash", "ok", 7),
        ("r-3", "address", "opus", "edit_file", "cancelled", 8),
        ("r-3", "lane-a", "mistral", "read_file", "ok", 8),
    ];
    for (turn, (run, session, model, tool, outcome, minute)) in (1u32..).zip(calls) {
        store
            .record_tool_call(
                &id(run),
                &ToolCallRecord {
                    at: format!("2026-10-07T10:{minute:02}:00Z"),
                    session: session.into(),
                    model: model.into(),
                    turn,
                    tool: tool.into(),
                    origin: "henk".into(),
                    outcome: outcome.into(),
                    arguments: format!("{{\"n\":{turn}}}"),
                    arguments_len: 7,
                    result_chars: 10,
                    elapsed_ms: u64::from(turn),
                },
            )
            .await
            .unwrap();
    }

    let tallies = |usage: Vec<henk_store::ToolUsage>| {
        usage
            .into_iter()
            .map(|u| {
                (
                    u.model,
                    u.session,
                    u.tool,
                    u.tally.calls,
                    u.tally.errors,
                    u.tally.refusals,
                    u.tally.other,
                )
            })
            .collect::<Vec<_>>()
    };
    let all = store.tool_usage(&ToolCallFilter::default()).await.unwrap();
    let mistral_reads = tallies(all)
        .into_iter()
        .find(|t| t.0 == "mistral" && t.2 == "read_file")
        .unwrap();
    assert_eq!(
        mistral_reads,
        (
            "mistral".into(),
            "lane".into(),
            "read_file".into(),
            3,
            1,
            0,
            0
        )
    );
    let checks = store
        .tool_usage(&ToolCallFilter {
            session_kind: Some("check".into()),
            ..ToolCallFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(
        tallies(checks),
        [(
            "opus".into(),
            "check".into(),
            "read_file".into(),
            1,
            0,
            0,
            0
        )]
    );
    let lanes = store
        .tool_usage(&ToolCallFilter {
            session_kind: Some("lane".into()),
            since: Some("2026-10-07T10:02:00Z".into()),
            until: Some("2026-10-07T10:06:00Z".into()),
            ..ToolCallFilter::default()
        })
        .await
        .unwrap();
    let mut lanes = tallies(lanes);
    lanes.sort();
    assert_eq!(
        lanes,
        [
            (
                "deepseek".into(),
                "lane".into(),
                "read_file".into(),
                1,
                0,
                1,
                0
            ),
            (
                "mistral".into(),
                "lane".into(),
                "github__get_file".into(),
                1,
                0,
                1,
                0
            ),
            (
                "mistral".into(),
                "lane".into(),
                "read_file".into(),
                1,
                1,
                0,
                0
            ),
        ],
        "since is inclusive, until is not; a planner is not a lane"
    );

    let listed = |filter: ToolCallFilter| async move {
        store
            .list_tool_calls(&filter, Page::new(50, 0))
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.call.turn, c.call.outcome))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        listed(ToolCallFilter {
            outcome: Some(OutcomeFilter::Problems),
            ..ToolCallFilter::default()
        })
        .await,
        [
            (8, "cancelled".to_owned()),
            (6, "malformed_arguments".to_owned()),
            (5, "refused_repeat".to_owned()),
            (3, "refused_scope".to_owned()),
            (2, "error".to_owned()),
        ],
        "newest first, everything but ok"
    );
    assert_eq!(
        listed(ToolCallFilter {
            outcome: Some(OutcomeFilter::Is("ok".into())),
            session_kind: Some("planner".into()),
            ..ToolCallFilter::default()
        })
        .await,
        [(7, "ok".to_owned())]
    );
    assert_eq!(
        listed(ToolCallFilter {
            tool: Some("read_file".into()),
            model: Some("mistral".into()),
            ..ToolCallFilter::default()
        })
        .await
        .len(),
        3
    );
    let first = store
        .list_tool_calls(&ToolCallFilter::default(), Page::new(1, 0))
        .await
        .unwrap();
    assert_eq!(first[0].run_id, "r-3");
    assert_eq!(first[0].repo, "o/r");
    assert_eq!(first[0].target, 7);
    assert_eq!(first[0].call.arguments, "{\"n\":9}");

    let mut seen = Vec::new();
    let mut filter = ToolCallFilter::default();
    loop {
        let page = store
            .list_tool_calls(&filter, Page::new(4, 0))
            .await
            .unwrap();
        let Some(last) = page.last() else { break };
        filter.before = Some(ToolCallKey {
            at: last.call.at.clone(),
            id: last.id,
        });
        seen.extend(page.into_iter().map(|c| c.call.turn));
    }
    seen.sort_unstable();
    assert_eq!(seen, (1..=9).collect::<Vec<_>>(), "every call once");

    // A call says whether its run still keeps its session's conversation.
    store
        .record_transcript(&id("r-3"), &transcript("lane-a", "", "{}"))
        .await
        .unwrap();
    let kept = store
        .list_tool_calls(&ToolCallFilter::default(), Page::new(50, 0))
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.transcript_kept)
        .map(|c| (c.run_id, c.call.session))
        .collect::<Vec<_>>();
    assert_eq!(
        kept,
        [("r-3".to_owned(), "lane-a".to_owned())],
        "only that run's session: not r-3's other session, not lane-a of r-1"
    );
}

async fn drafts_are_counted_by_group_and_listed_across_runs(store: &dyn RunStore) {
    let on = |run: &str, repo: &str, target: u64| NewRun {
        repo: repo.into(),
        target,
        ..new_run(run)
    };
    for run in [
        on("r-1", "o/a", 1),
        on("r-2", "o/a", 2),
        on("r-3", "o/b", 3),
    ] {
        store.create_run(&run).await.unwrap();
    }
    // (run, draft, lane, model, minute, verdict)
    let drafts: [(&str, &str, &str, &str, u32, Option<DraftVerdict>); 9] = [
        (
            "r-1",
            "d1",
            "lane-a",
            "mistral",
            1,
            Some(DraftVerdict::Rejected),
        ),
        (
            "r-1",
            "d2",
            "lane-a",
            "mistral",
            2,
            Some(DraftVerdict::Rejected),
        ),
        (
            "r-1",
            "d3",
            "lane-b",
            "deepseek",
            3,
            Some(DraftVerdict::Confirmed),
        ),
        (
            "r-2",
            "d1",
            "lane-a",
            "mistral",
            4,
            Some(DraftVerdict::SameAs),
        ),
        (
            "r-2",
            "d2",
            "lane-b",
            "deepseek",
            5,
            Some(DraftVerdict::Unchecked),
        ),
        ("r-2", "d3", "lane-b", "deepseek", 6, None),
        (
            "r-3",
            "d1",
            "lane-a",
            "mistral",
            7,
            Some(DraftVerdict::Confirmed),
        ),
        (
            "r-3",
            "d2",
            "lane-a",
            "mistral",
            7,
            Some(DraftVerdict::Rejected),
        ),
        (
            "r-3",
            "d3",
            "lane-b",
            "deepseek",
            8,
            Some(DraftVerdict::NotChecked),
        ),
    ];
    for (run, draft, lane, model, minute, verdict) in drafts {
        let at = format!("2026-10-07T10:{minute:02}:00Z");
        store
            .record_draft(
                &id(run),
                &DraftRecord {
                    at: at.clone(),
                    draft: draft.into(),
                    lane: lane.into(),
                    model: model.into(),
                    kind: "finding".into(),
                    path: "a.rs".into(),
                    line: 4,
                    target: String::new(),
                    body: format!("{run} {draft}"),
                    decision: None,
                },
            )
            .await
            .unwrap();
        if let Some(verdict) = verdict {
            store
                .decide_draft(
                    &id(run),
                    draft,
                    &DraftDecision {
                        at,
                        verdict,
                        checker: "opus".into(),
                        reason: "Why.".into(),
                        same_as: String::new(),
                        comment_id: String::new(),
                    },
                )
                .await
                .unwrap();
        }
    }

    let all = DraftFilter::default();
    let by_model = store.draft_rates(DraftGroup::Model, &all).await.unwrap();
    assert_eq!(
        by_model,
        [
            DraftRates {
                key: "mistral".into(),
                drafts: 5,
                confirmed: 1,
                rejected: 3,
                same_as: 1,
                ..DraftRates::default()
            },
            DraftRates {
                key: "deepseek".into(),
                drafts: 4,
                confirmed: 1,
                unchecked: 1,
                not_checked: 1,
                waiting: 1,
                ..DraftRates::default()
            },
        ],
        "the largest group first"
    );
    let by_target = store.draft_rates(DraftGroup::Target, &all).await.unwrap();
    let keys: Vec<_> = by_target
        .iter()
        .map(|r| (r.key.clone(), r.repo.clone(), r.target, r.drafts))
        .collect();
    assert_eq!(
        keys,
        [
            ("o/a #1".to_owned(), Some("o/a".to_owned()), Some(1), 3),
            ("o/a #2".to_owned(), Some("o/a".to_owned()), Some(2), 3),
            ("o/b #3".to_owned(), Some("o/b".to_owned()), Some(3), 3),
        ]
    );
    let by_repo = store.draft_rates(DraftGroup::Repo, &all).await.unwrap();
    assert_eq!(
        by_repo
            .iter()
            .map(|r| (r.key.as_str(), r.drafts))
            .collect::<Vec<_>>(),
        [("o/a", 6), ("o/b", 3)]
    );
    let window = DraftFilter {
        since: Some("2026-10-07T10:02:00Z".into()),
        until: Some("2026-10-07T10:07:00Z".into()),
        repo: Some("o/a".into()),
        ..DraftFilter::default()
    };
    let by_lane = store.draft_rates(DraftGroup::Lane, &window).await.unwrap();
    assert_eq!(
        by_lane
            .iter()
            .map(|r| (r.key.as_str(), r.drafts))
            .collect::<Vec<_>>(),
        [("lane-b", 3), ("lane-a", 2)],
        "since is inclusive, until is not"
    );

    let listed = |filter: DraftFilter| async move {
        store
            .list_drafts(&filter, Page::new(50, 0))
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.draft.body)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        listed(DraftFilter {
            verdict: Some(VerdictFilter::Is(DraftVerdict::Rejected)),
            ..DraftFilter::default()
        })
        .await,
        ["r-3 d2", "r-1 d2", "r-1 d1"],
        "newest first, a tie on the time by row"
    );
    assert_eq!(
        listed(DraftFilter {
            verdict: Some(VerdictFilter::Waiting),
            ..DraftFilter::default()
        })
        .await,
        ["r-2 d3"]
    );
    assert_eq!(
        listed(DraftFilter {
            model: Some("deepseek".into()),
            repo: Some("o/b".into()),
            ..DraftFilter::default()
        })
        .await,
        ["r-3 d3"]
    );
    let first = store.list_drafts(&all, Page::new(50, 0)).await.unwrap();
    assert_eq!(first[0].repo, "o/b");
    assert_eq!(first[0].target, 3);
    assert_eq!(first[0].run_id, "r-3");
    assert_eq!(
        first[0].draft.decision.as_ref().unwrap().verdict,
        DraftVerdict::NotChecked
    );

    let mut seen = Vec::new();
    let mut filter = DraftFilter::default();
    loop {
        let page = store.list_drafts(&filter, Page::new(4, 0)).await.unwrap();
        let Some(last) = page.last() else { break };
        filter.before = Some(DraftKey {
            created_at: last.draft.at.clone(),
            id: last.id,
        });
        seen.extend(page.into_iter().map(|d| d.draft.body));
    }
    assert_eq!(seen.len(), 9, "every draft once: {seen:?}");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 9);
}

async fn drafts_are_kept_replaced_and_decided(store: &dyn RunStore) {
    let run = new_run("r-drafts");
    store.create_run(&run).await.unwrap();
    store
        .record_draft(&run.id, &draft("d1", "lane-a", 4, "x is never set."))
        .await
        .unwrap();
    store
        .record_draft(&run.id, &draft("d2", "lane-b", 9, "y leaks."))
        .await
        .unwrap();
    store
        .record_draft(
            &run.id,
            &draft("d1", "lane-a", 4, "x is never set on the error path."),
        )
        .await
        .unwrap();
    let mut rewrite = draft("d3", "lane-b", 12, "Better text.");
    "rewrite".clone_into(&mut rewrite.kind);
    "c-7".clone_into(&mut rewrite.target);
    store.record_draft(&run.id, &rewrite).await.unwrap();

    let waiting = store.drafts(&run.id).await.unwrap();
    assert_eq!(
        waiting.iter().map(|d| d.draft.as_str()).collect::<Vec<_>>(),
        ["d1", "d2", "d3"],
        "a replaced draft keeps its place"
    );
    assert_eq!(waiting[0].body, "x is never set on the error path.");
    assert_eq!(waiting[2].target, "c-7");
    assert!(waiting.iter().all(|d| d.decision.is_none()));
    assert!(OffsetDateTime::parse(&waiting[0].at, &Rfc3339).is_ok());

    let confirmed = DraftDecision {
        at: String::new(),
        verdict: DraftVerdict::Confirmed,
        checker: "sonnet".into(),
        reason: "Line 4 never assigns x.".into(),
        same_as: String::new(),
        comment_id: "c-9".into(),
    };
    store.decide_draft(&run.id, "d1", &confirmed).await.unwrap();
    let merged = DraftDecision {
        verdict: DraftVerdict::SameAs,
        same_as: "d1".into(),
        ..confirmed.clone()
    };
    store.decide_draft(&run.id, "d2", &merged).await.unwrap();

    let decided = store.drafts(&run.id).await.unwrap();
    let first = decided[0].decision.clone().unwrap();
    assert_eq!(first.verdict, DraftVerdict::Confirmed);
    assert_eq!(
        (
            first.checker.as_str(),
            first.reason.as_str(),
            first.comment_id.as_str()
        ),
        ("sonnet", "Line 4 never assigns x.", "c-9")
    );
    assert!(OffsetDateTime::parse(&first.at, &Rfc3339).is_ok());
    let second = decided[1].decision.clone().unwrap();
    assert_eq!(
        (second.verdict, second.same_as.as_str()),
        (DraftVerdict::SameAs, "d1")
    );
    assert!(decided[2].decision.is_none(), "d3 still waits");
    assert!(
        store.drafts(&id("r-other")).await.unwrap().is_empty(),
        "only the run's own drafts"
    );
}

fn transcript(session: &str, at: &str, body: &str) -> TranscriptRecord {
    TranscriptRecord {
        at: at.into(),
        session: session.into(),
        model: "opus".into(),
        stop: "EndTurn".into(),
        turns: 4,
        bytes: u64::try_from(body.len()).unwrap(),
        body: body.into(),
    }
}

async fn transcripts_are_kept_whole_listed_and_pruned(store: &dyn RunStore) {
    let run = new_run("r-transcripts");
    store.create_run(&run).await.unwrap();
    // Large and with every kind of character a conversation has: kept
    // byte for byte, never cut.
    let large = format!(
        "{{\"messages\":\"{}\"}}",
        "caf\u{e9} \\n \u{1f50d} ".repeat(150_000)
    );
    assert!(large.len() > 2 * 1024 * 1024);
    store
        .record_transcript(
            &run.id,
            &transcript("lane-a", "2026-01-01T00:00:00Z", "old"),
        )
        .await
        .unwrap();
    store
        .record_transcript(&run.id, &transcript("lane-a", "", &large))
        .await
        .unwrap();
    store
        .record_transcript(&run.id, &transcript("check-lane-a-1", "", "{}"))
        .await
        .unwrap();

    let listed = store.transcripts(&run.id).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|t| t.session.as_str())
            .collect::<Vec<_>>(),
        ["lane-a", "lane-a", "check-lane-a-1"]
    );
    assert_eq!(listed[1].bytes, u64::try_from(large.len()).unwrap());
    let kept = store.transcript(&run.id, "lane-a").await.unwrap().unwrap();
    assert_eq!(kept.body, large, "the latest of the name, whole");
    assert_eq!(
        (kept.model.as_str(), kept.stop.as_str(), kept.turns),
        ("opus", "EndTurn", 4)
    );
    assert!(store.transcript(&run.id, "lane-b").await.unwrap().is_none());

    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(30);
    let pruned = store.prune_events(cutoff).await.unwrap();
    assert_eq!(pruned.transcripts, 1, "only the old one");
    assert_eq!(store.transcripts(&run.id).await.unwrap().len(), 2);
    assert!(store.run(&run.id).await.unwrap().is_some(), "the run stays");
}

fn tool_call(session: &str, model: &str, tool: &str, outcome: &str, ms: u64) -> ToolCallRecord {
    ToolCallRecord {
        at: String::new(),
        session: session.into(),
        model: model.into(),
        turn: 1,
        tool: tool.into(),
        origin: "henk".into(),
        outcome: outcome.into(),
        arguments: r#"{"path":"a.rs"}"#.into(),
        arguments_len: 15,
        result_chars: 40,
        elapsed_ms: ms,
    }
}

async fn tool_calls_are_kept_in_order_and_add_up_across_runs(store: &dyn RunStore) {
    let before = OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let (one, two) = (new_run("r-tools-1"), new_run("r-tools-2"));
    store.create_run(&one).await.unwrap();
    store.create_run(&two).await.unwrap();
    let mut cut = tool_call("lane-a", "opus", "bash", "ok", 120);
    cut.arguments = "x".repeat(16);
    cut.arguments_len = 9000;
    cut.turn = 3;
    cut.origin = "workspace".into();
    let calls = [
        tool_call("lane-a", "opus", "read_file", "ok", 10),
        tool_call("lane-a", "opus", "read_file", "error", 5),
        cut.clone(),
        tool_call(
            "check-lane-a-1",
            "sonnet",
            "github__get_file_contents",
            "refused_scope",
            0,
        ),
    ];
    for call in &calls {
        store.record_tool_call(&one.id, call).await.unwrap();
    }
    store
        .record_tool_call(
            &two.id,
            &tool_call("lane-b", "opus", "read_file", "not_run", 0),
        )
        .await
        .unwrap();

    let kept = store.tool_calls(&one.id).await.unwrap();
    assert_eq!(kept.len(), 4, "only this run's, in order");
    assert_eq!(
        kept.iter().map(|c| c.tool.as_str()).collect::<Vec<_>>(),
        [
            "read_file",
            "read_file",
            "bash",
            "github__get_file_contents"
        ]
    );
    assert!(!kept[0].at.is_empty(), "an empty time is now");
    let bash = &kept[2];
    assert_eq!(
        (
            bash.turn,
            bash.origin.as_str(),
            bash.arguments.as_str(),
            bash.arguments_len,
            bash.elapsed_ms
        ),
        (3, "workspace", "x".repeat(16).as_str(), 9000, 120),
        "the cut arguments and their original length"
    );
    assert_eq!(kept[3].outcome, "refused_scope");

    let usage = store.tool_usage_since(before).await.unwrap();
    let row = |kind: &str, model: &str, tool: &str| {
        usage
            .iter()
            .find(|u| u.session == kind && u.model == model && u.tool == tool)
            .map(|u| u.tally.clone())
            .unwrap_or_default()
    };
    let read = row("lane", "opus", "read_file");
    assert_eq!(
        (read.calls, read.errors, read.other, read.total_ms),
        (3, 1, 1, 15),
        "both runs"
    );
    assert_eq!(
        row("check", "sonnet", "github__get_file_contents").refusals,
        1
    );
    assert_eq!(row("lane", "opus", "bash").total_ms, 120);
    let later = OffsetDateTime::now_utc() + time::Duration::minutes(1);
    assert!(store.tool_usage_since(later).await.unwrap().is_empty());
}

async fn old_events_and_their_outcomes_are_pruned(store: &dyn RunStore) {
    store.create_run(&new_run("r-pruned")).await.unwrap();
    let now = OffsetDateTime::now_utc();
    let recent = now.format(&Rfc3339).unwrap();
    store
        .record_event(&inbound("e-old", "2026-01-01T00:00:00Z"))
        .await
        .unwrap();
    store
        .record_event(&inbound("e-new", &recent))
        .await
        .unwrap();
    for (event, listener) in [
        ("e-old", "review"),
        ("e-old", "mention"),
        ("e-new", "review"),
    ] {
        store
            .record_outcome(&outcome(event, listener, Some("r-pruned")))
            .await
            .unwrap();
    }

    let counts = store
        .prune_events(now - time::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(
        counts,
        PruneCounts {
            events: 1,
            outcomes: 2,
            transcripts: 0,
        }
    );
    assert!(
        store
            .inbound_event(&event_id("e-old"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.outcomes(&event_id("e-old")).await.unwrap().is_empty());
    assert!(
        store
            .inbound_event(&event_id("e-new"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(store.outcomes(&event_id("e-new")).await.unwrap().len(), 1);
    assert!(
        store.run(&id("r-pruned")).await.unwrap().is_some(),
        "runs are kept: their links are posted on the platforms"
    );

    let again = store
        .prune_events(now - time::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(again, PruneCounts::default(), "nothing left to prune");
}

async fn run_round_trips(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let run = store.run(&id("r-1")).await.unwrap().unwrap();
    assert_eq!(run.kind, RunKind::Review);
    assert_eq!(run.platform, Platform::GitHub);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.repo, "o/r");
    assert_eq!(run.target, 7);
    assert_eq!(run.commit.as_deref(), Some("abc"));
    assert_eq!(run.link, "https://henk.example/runs/r-1");
    assert!(OffsetDateTime::parse(&run.started_at, &Rfc3339).is_ok());
    assert!(run.finished_at.is_none());

    store
        .finish_run(&run.id, RunStatus::Finished, Some("No issues found."), None)
        .await
        .unwrap();
    let run = store.run(&run.id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.summary.as_deref(), Some("No issues found."));
    assert!(run.finished_at.is_some());
    assert!(store.run(&id("nope")).await.unwrap().is_none());
}

async fn a_heartbeat_moves_and_a_check_id_is_kept(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let first = store.run(&id("r-1")).await.unwrap().unwrap();
    assert!(first.heartbeat_at.is_some(), "a new run starts alive");
    assert_eq!(first.check_id, None);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store.heartbeat(&id("r-1")).await.unwrap();
    store.set_check(&id("r-1"), "42").await.unwrap();
    let later = store.run(&id("r-1")).await.unwrap().unwrap();
    // Compared as times: RFC 3339 text drops trailing zeros, so it does not sort.
    let at = |text: Option<String>| OffsetDateTime::parse(&text.unwrap(), &Rfc3339).unwrap();
    assert!(at(later.heartbeat_at.clone()) > at(first.heartbeat_at.clone()));
    assert_eq!(later.check_id.as_deref(), Some("42"));
}

async fn only_running_runs_with_a_stale_heartbeat_are_orphaned(store: &dyn RunStore) {
    for run in ["r-a", "r-b", "r-done"] {
        store.create_run(&new_run(run)).await.unwrap();
    }
    store
        .finish_run(&id("r-done"), RunStatus::Finished, None, None)
        .await
        .unwrap();
    let ids = |runs: Vec<RunRecord>| {
        runs.into_iter()
            .map(|r| r.id.to_string())
            .collect::<Vec<_>>()
    };

    let an_hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
    assert!(store.orphaned_runs(an_hour_ago).await.unwrap().is_empty());
    let in_an_hour = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert_eq!(
        ids(store.orphaned_runs(in_an_hour).await.unwrap()),
        ["r-a", "r-b"],
        "a finished run is never orphaned; oldest first"
    );
}

async fn dropping_running_lanes_leaves_finished_ones_alone(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    store.start_lane(&id("r-1"), "a", "m").await.unwrap();
    store.start_lane(&id("r-1"), "b", "m").await.unwrap();
    store
        .finish_lane(&id("r-1"), "a", LaneStatus::Finished, 3, 1, 1, None)
        .await
        .unwrap();
    store
        .drop_running_lanes(&id("r-1"), "interrupted")
        .await
        .unwrap();
    let lanes = store.lanes(&id("r-1")).await.unwrap();
    let a = lanes.iter().find(|l| l.name == "a").unwrap();
    let b = lanes.iter().find(|l| l.name == "b").unwrap();
    assert_eq!(a.status, LaneStatus::Finished);
    assert_eq!(a.error, None);
    assert_eq!(b.status, LaneStatus::Dropped);
    assert_eq!(b.error.as_deref(), Some("interrupted"));
}

async fn lanes_findings_and_events_attach_to_a_run(store: &dyn RunStore) {
    let run = new_run("r-2");
    store.create_run(&run).await.unwrap();
    store.start_lane(&run.id, "b", "model-y").await.unwrap();
    store.start_lane(&run.id, "a", "model-x").await.unwrap();
    store
        .finish_lane(&run.id, "a", LaneStatus::Finished, 4, 1000, 200, None)
        .await
        .unwrap();
    store
        .finish_lane(&run.id, "b", LaneStatus::Dropped, 1, 10, 0, Some("timeout"))
        .await
        .unwrap();
    store
        .record_finding(&run.id, "a", "src/x.rs", 12, "c1", FindingAction::Posted)
        .await
        .unwrap();
    store
        .record_finding(&run.id, "b", "src/y.rs", 3, "c2", FindingAction::Withdrawn)
        .await
        .unwrap();
    store.event(&run.id, "info", "started").await.unwrap();
    store.event(&run.id, "warn", "slow").await.unwrap();

    let lanes = store.lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    assert_eq!(lanes[0].name, "a", "lanes come back by name");
    assert_eq!(lanes[0].model, "model-x");
    assert_eq!(lanes[0].status, LaneStatus::Finished);
    assert_eq!(
        (
            lanes[0].turns,
            lanes[0].input_tokens,
            lanes[0].output_tokens
        ),
        (4, 1000, 200)
    );
    assert_eq!(lanes[1].error.as_deref(), Some("timeout"));

    let events = store.events(&run.id).await.unwrap();
    let messages: Vec<_> = events.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(messages, ["started", "slow"], "timeline is oldest first");
    assert_eq!(events[1].level, "warn");

    let findings = store.findings(&run.id).await.unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].lane, "a");
    assert_eq!(findings[0].path, "src/x.rs");
    assert_eq!(findings[0].line, 12);
    assert_eq!(findings[0].comment_id, "c1");
    assert_eq!(findings[0].action, "posted");
    assert_eq!(findings[1].action, "withdrawn");
    assert!(store.findings(&id("r-none")).await.unwrap().is_empty());
}

async fn events_and_outcomes_round_trip(store: &dyn RunStore) {
    store.create_run(&new_run("r-9")).await.unwrap();
    let event = inbound("e-1", "2026-10-03T00:00:00Z");
    store.record_event(&event).await.unwrap();
    store
        .record_outcome(&outcome("e-1", "review", Some("r-9")))
        .await
        .unwrap();

    let read = store
        .inbound_event(&event_id("e-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.kind, "pull_request");
    assert_eq!(read.source, "github_webhook");
    assert_eq!(read.repo.as_deref(), Some("o/r"));
    assert_eq!(read.target, Some(7));
    assert_eq!(read.payload.as_deref(), Some("{}"));
    assert_eq!(read.requester, None);
    let asked = InboundEvent {
        source: "dashboard".into(),
        requester: Some("github:1234".into()),
        ..inbound("e-asked", "2026-10-03T00:00:01Z")
    };
    store.record_event(&asked).await.unwrap();
    assert_eq!(
        store
            .inbound_event(&event_id("e-asked"))
            .await
            .unwrap()
            .unwrap()
            .requester
            .as_deref(),
        Some("github:1234"),
        "who asked is kept"
    );
    let received = OffsetDateTime::parse(&read.received_at, &Rfc3339).unwrap();
    assert_eq!(
        received.unix_timestamp(),
        1_790_985_600,
        "the given time is kept"
    );

    let outcomes = store.outcomes(&event_id("e-1")).await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].run_id.as_deref(), Some("r-9"));
    assert!(!outcomes[0].at.is_empty(), "an empty time means now");
    assert_eq!(
        store
            .inbound_events_for_run(&id("r-9"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .inbound_event(&event_id("e-nope"))
            .await
            .unwrap()
            .is_none()
    );

    let big = "x".repeat(MAX_PAYLOAD_BYTES + 1);
    store
        .record_event(&InboundEvent {
            id: event_id("e-2"),
            payload: Some(big),
            ..event
        })
        .await
        .unwrap();
    assert!(
        store
            .inbound_event(&event_id("e-2"))
            .await
            .unwrap()
            .unwrap()
            .payload
            .is_none(),
        "oversized payload dropped"
    );
}

async fn outcomes_and_linked_events_keep_their_order(store: &dyn RunStore) {
    store.create_run(&new_run("r-5")).await.unwrap();
    store
        .record_event(&inbound("e-late", "2026-10-03T00:00:02Z"))
        .await
        .unwrap();
    store
        .record_event(&inbound("e-early", "2026-10-03T00:00:01Z"))
        .await
        .unwrap();
    for listener in ["review", "mention", "plan"] {
        let run = (listener == "review").then_some("r-5");
        store
            .record_outcome(&outcome("e-late", listener, run))
            .await
            .unwrap();
    }
    // Two outcomes of one event point at the run: the event is listed once.
    store
        .record_outcome(&outcome("e-early", "review", Some("r-5")))
        .await
        .unwrap();
    store
        .record_outcome(&outcome("e-early", "mention", Some("r-5")))
        .await
        .unwrap();

    let listeners: Vec<_> = store
        .outcomes(&event_id("e-late"))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.listener)
        .collect();
    assert_eq!(listeners, ["review", "mention", "plan"], "recording order");
    let linked: Vec<_> = store
        .inbound_events_for_run(&id("r-5"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.id.to_string())
        .collect();
    assert_eq!(linked, ["e-early", "e-late"], "oldest first, each once");
}

async fn a_duplicate_run_id_is_an_error(store: &dyn RunStore) {
    store.create_run(&new_run("r-dup")).await.unwrap();
    assert!(store.create_run(&new_run("r-dup")).await.is_err());
    store
        .record_event(&inbound("e-dup", "2026-10-03T00:00:00Z"))
        .await
        .unwrap();
    assert!(
        store
            .record_event(&inbound("e-dup", "2026-10-03T00:00:00Z"))
            .await
            .is_err()
    );
}

async fn joining_is_accepted(store: &dyn RunStore) {
    store.create_run(&new_run("r-3")).await.unwrap();
    store.joined(&id("r-3"), "comment").await.unwrap();
    store.joined(&id("r-3"), "webhook").await.unwrap();
}

async fn a_number_beyond_i64_is_refused_not_stored_as_something_else(store: &dyn RunStore) {
    let run = NewRun {
        target: u64::MAX,
        ..new_run("r-big")
    };
    assert!(matches!(
        store.create_run(&run).await,
        Err(henk_store::StoreError::Corrupt { .. })
    ));
    assert!(store.run(&id("r-big")).await.unwrap().is_none());

    let event = InboundEvent {
        target: Some(u64::MAX),
        ..inbound("e-big", "2026-10-03T00:00:00Z")
    };
    assert!(matches!(
        store.record_event(&event).await,
        Err(henk_store::StoreError::Corrupt { .. })
    ));
    assert!(
        store
            .inbound_event(&event_id("e-big"))
            .await
            .unwrap()
            .is_none()
    );
}

async fn runs_are_listed_newest_first_by_filter_and_page(store: &dyn RunStore) {
    let plan = |id: &str| NewRun {
        kind: RunKind::Plan,
        repo: "o/other".into(),
        ..new_run(id)
    };
    for run in [new_run("r-1"), plan("r-2"), new_run("r-3"), new_run("r-4")] {
        store.create_run(&run).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    store
        .finish_run(&id("r-3"), RunStatus::Failed, None, Some("boom"))
        .await
        .unwrap();
    let ids = |runs: Vec<RunRecord>| {
        runs.into_iter()
            .map(|r| r.id.to_string())
            .collect::<Vec<_>>()
    };
    let all = RunFilter::default();

    assert_eq!(
        ids(store.list_runs(&all, Page::new(50, 0)).await.unwrap()),
        ["r-4", "r-3", "r-2", "r-1"]
    );
    assert_eq!(
        ids(store.list_runs(&all, Page::new(2, 0)).await.unwrap()),
        ["r-4", "r-3"]
    );
    assert_eq!(
        store.count_runs(&all).await.unwrap(),
        4,
        "the count spans every page"
    );
    assert_eq!(
        ids(store.list_runs(&all, Page::new(2, 2)).await.unwrap()),
        ["r-2", "r-1"]
    );
    assert!(
        store
            .list_runs(&all, Page::new(2, 4))
            .await
            .unwrap()
            .is_empty()
    );
    let failed = RunFilter {
        status: Some(RunStatus::Failed),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(store.list_runs(&failed, Page::new(50, 0)).await.unwrap()),
        ["r-3"]
    );
    let plans = RunFilter {
        kind: Some(RunKind::Plan),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(store.list_runs(&plans, Page::new(50, 0)).await.unwrap()),
        ["r-2"]
    );
    let running_reviews = RunFilter {
        kind: Some(RunKind::Review),
        status: Some(RunStatus::Running),
        platform: Some(Platform::GitHub),
        repo: Some("o/r".into()),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(store
            .list_runs(&running_reviews, Page::new(50, 0))
            .await
            .unwrap()),
        ["r-4", "r-1"]
    );
    assert_eq!(store.count_runs(&running_reviews).await.unwrap(), 2);
    assert_eq!(store.count_runs(&failed).await.unwrap(), 1);
    assert_eq!(store.count_runs(&plans).await.unwrap(), 1);
    let gitlab = RunFilter {
        platform: Some(Platform::GitLab),
        ..RunFilter::default()
    };
    assert!(
        store
            .list_runs(&gitlab, Page::new(50, 0))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.count_runs(&gitlab).await.unwrap(), 0);
    assert_eq!(Page::new(1000, 0).limit(), Page::MAX, "a page is capped");
    assert_eq!(Page::new(0, 0).limit(), 1);
}

async fn runs_page_by_keyset_and_filter_by_target_and_time(store: &dyn RunStore) {
    let on = |id: &str, target: u64| NewRun {
        target,
        ..new_run(id)
    };
    for run in [
        on("r-1", 7),
        on("r-2", 8),
        on("r-3", 7),
        on("r-4", 9),
        on("r-5", 7),
    ] {
        store.create_run(&run).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let ids = |runs: &[RunRecord]| runs.iter().map(|r| r.id.to_string()).collect::<Vec<_>>();
    let key = |run: &RunRecord| RunKey {
        started_at: run.started_at.clone(),
        id: run.id.to_string(),
    };

    let mut seen = Vec::new();
    let mut filter = RunFilter::default();
    loop {
        let page = store.list_runs(&filter, Page::new(2, 0)).await.unwrap();
        let Some(last) = page.last() else { break };
        filter.before = Some(key(last));
        seen.extend(ids(&page));
    }
    assert_eq!(
        seen,
        ["r-5", "r-4", "r-3", "r-2", "r-1"],
        "every run once, in order"
    );

    // A run started after the first page does not shift the next one.
    let first = store
        .list_runs(&RunFilter::default(), Page::new(2, 0))
        .await
        .unwrap();
    store.create_run(&on("r-6", 7)).await.unwrap();
    let next = RunFilter {
        before: Some(key(&first[1])),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(&store.list_runs(&next, Page::new(2, 0)).await.unwrap()),
        ["r-3", "r-2"]
    );
    assert_eq!(
        store.count_runs(&next).await.unwrap(),
        3,
        "the count takes the keyset too"
    );

    let sevens = RunFilter {
        target: Some(7),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(&store.list_runs(&sevens, Page::new(50, 0)).await.unwrap()),
        ["r-6", "r-5", "r-3", "r-1"]
    );
    let all = store
        .list_runs(&RunFilter::default(), Page::new(50, 0))
        .await
        .unwrap();
    let at = |id: &str| {
        all.iter()
            .find(|r| r.id.to_string() == id)
            .unwrap()
            .started_at
            .clone()
    };
    let window = RunFilter {
        since: Some(at("r-2")),
        until: Some(at("r-4")),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(&store.list_runs(&window, Page::new(50, 0)).await.unwrap()),
        ["r-3", "r-2"],
        "since is inclusive, until is not"
    );
    assert_eq!(store.count_runs(&window).await.unwrap(), 2);
}

async fn a_whole_second_bound_holds_for_a_run_started_within_that_second(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let started_at = store.run(&id("r-1")).await.unwrap().unwrap().started_at;
    // The second it started in, as the API passes a bound: "...T12:00:00Z".
    let second = format!("{}Z", started_at.get(..19).unwrap());
    let listed = |since: Option<String>, until: Option<String>| RunFilter {
        since,
        until,
        ..RunFilter::default()
    };
    let since = listed(Some(second.clone()), None);
    assert_eq!(
        store
            .list_runs(&since, Page::new(50, 0))
            .await
            .unwrap()
            .len(),
        1,
        "{started_at} is not before since={second}"
    );
    assert_eq!(store.count_runs(&since).await.unwrap(), 1);
    let until = listed(None, Some(second.clone()));
    assert!(
        store
            .list_runs(&until, Page::new(50, 0))
            .await
            .unwrap()
            .is_empty(),
        "{started_at} is not before until={second}"
    );
    assert_eq!(store.count_runs(&until).await.unwrap(), 0);
    let at = OffsetDateTime::parse(&second, &Rfc3339).unwrap();
    let next = (at + time::Duration::SECOND).format(&Rfc3339).unwrap();
    let window = listed(Some(second), Some(next));
    assert_eq!(store.count_runs(&window).await.unwrap(), 1);
}

async fn inbound_events_page_by_keyset_with_ties_on_the_time(store: &dyn RunStore) {
    for (id, at) in [
        ("e-a", "2026-10-03T00:00:01Z"),
        ("e-b", "2026-10-03T00:00:02Z"),
        ("e-c", "2026-10-03T00:00:02Z"),
        ("e-d", "2026-10-03T00:00:02Z"),
        ("e-e", "2026-10-03T00:00:03Z"),
    ] {
        store.record_event(&inbound(id, at)).await.unwrap();
    }
    let mut seen = Vec::new();
    let mut filter = EventFilter::default();
    loop {
        let page = store
            .list_inbound_events(&filter, Page::new(2, 0))
            .await
            .unwrap();
        let Some(last) = page.last() else { break };
        filter.before = Some(EventKey {
            received_at: last.event.received_at.clone(),
            id: last.event.id.to_string(),
        });
        seen.extend(page.iter().map(|e| e.event.id.to_string()));
    }
    assert_eq!(
        seen,
        ["e-e", "e-d", "e-c", "e-b", "e-a"],
        "a tie on the time is broken by the id, and no event is lost or repeated"
    );
}

async fn inbound_events_are_listed_with_their_outcomes(store: &dyn RunStore) {
    store.create_run(&new_run("r-7")).await.unwrap();
    store
        .record_event(&inbound("e-old", "2026-10-03T00:00:01Z"))
        .await
        .unwrap();
    store
        .record_event(&inbound("e-new", "2026-10-03T00:00:02Z"))
        .await
        .unwrap();
    let api = InboundEvent {
        source: "api".into(),
        kind: "plan_requested".into(),
        ..inbound("e-api", "2026-10-03T00:00:03Z")
    };
    store.record_event(&api).await.unwrap();
    store
        .record_outcome(&outcome("e-new", "review", Some("r-7")))
        .await
        .unwrap();
    store
        .record_outcome(&outcome("e-new", "mention", None))
        .await
        .unwrap();
    store
        .record_outcome(&outcome("e-old", "review", None))
        .await
        .unwrap();

    let listed = store
        .list_inbound_events(&EventFilter::default(), Page::new(50, 0))
        .await
        .unwrap();
    let ids: Vec<_> = listed.iter().map(|e| e.event.id.to_string()).collect();
    assert_eq!(ids, ["e-api", "e-new", "e-old"]);
    assert!(listed[0].outcomes.is_empty());
    let listeners: Vec<_> = listed[1]
        .outcomes
        .iter()
        .map(|o| o.listener.as_str())
        .collect();
    assert_eq!(
        listeners,
        ["review", "mention"],
        "each event gets its own, in order"
    );
    assert_eq!(listed[1].outcomes[0].run_id.as_deref(), Some("r-7"));
    assert_eq!(listed[2].outcomes.len(), 1);

    let webhooks = EventFilter {
        source: Some("github_webhook".into()),
        ..EventFilter::default()
    };
    let listed = store
        .list_inbound_events(&webhooks, Page::new(1, 1))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].event.id.to_string(), "e-old");
    let plans = EventFilter {
        kind: Some("plan_requested".into()),
        repo: Some("o/r".into()),
        ..EventFilter::default()
    };
    assert_eq!(
        store
            .list_inbound_events(&plans, Page::new(50, 0))
            .await
            .unwrap()
            .len(),
        1
    );
}

mod sqlite {
    use super::*;

    fn open() -> Arc<dyn RunStore> {
        Arc::new(SqliteStore::in_memory().unwrap())
    }

    macro_rules! tests {
        ($($scenario:ident),* $(,)?) => {
            $(
                #[tokio::test]
                async fn $scenario() {
                    super::$scenario(open().as_ref()).await;
                }
            )*
        };
    }

    for_each_scenario!(tests);
}

/// `PostgreSQL`, when `HENK_TEST_DATABASE_URL` names a server. Every test gets
/// its own schema, so tests run in parallel and leave nothing behind.
mod postgres {
    use super::*;

    const URL: &str = "HENK_TEST_DATABASE_URL";

    /// A store in a fresh schema; the schema is dropped with the guard.
    pub(super) struct Schema {
        pub(super) store: Arc<dyn RunStore>,
        pub(super) url: String,
        name: String,
        base: String,
    }

    impl Drop for Schema {
        fn drop(&mut self) {
            let (base, name) = (self.base.clone(), self.name.clone());
            // Drop runs outside the test's runtime: clean up on a thread.
            let _ = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    if let Ok(client) = plain_client(&base).await {
                        let _ = client
                            .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
                            .await;
                    }
                });
            })
            .join();
        }
    }

    async fn plain_client(url: &str) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        tokio::spawn(connection);
        Ok(client)
    }

    pub(super) async fn schema() -> Schema {
        let base = std::env::var(URL).unwrap_or_else(|_| panic!("{URL} is not set"));
        let name = format!(
            "henk_test_{}",
            OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .unsigned_abs()
                ^ u128::from(std::process::id())
        );
        plain_client(&base)
            .await
            .unwrap()
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{name}");
        let store = Arc::new(PgStore::connect(&url).await.unwrap());
        Schema {
            store,
            url,
            name,
            base,
        }
    }

    macro_rules! tests {
        ($($scenario:ident),* $(,)?) => {
            $(
                #[tokio::test]
                #[ignore = "needs HENK_TEST_DATABASE_URL"]
                async fn $scenario() {
                    let schema = schema().await;
                    super::$scenario(schema.store.as_ref()).await;
                }
            )*
        };
    }

    for_each_scenario!(tests);

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn migrating_again_changes_nothing_and_concurrent_starts_agree() {
        let schema = schema().await;
        schema.store.create_run(&new_run("r-kept")).await.unwrap();
        let (a, b) = tokio::join!(PgStore::connect(&schema.url), PgStore::connect(&schema.url));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(
            a.run(&id("r-kept")).await.unwrap().is_some(),
            "data survives"
        );
        assert!(b.run(&id("r-kept")).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn a_run_that_never_had_a_heartbeat_is_orphaned() {
        let schema = schema().await;
        schema.store.create_run(&new_run("r-silent")).await.unwrap();
        plain_client(&schema.url)
            .await
            .unwrap()
            .batch_execute("UPDATE runs SET heartbeat_at = NULL WHERE id = 'r-silent'")
            .await
            .unwrap();
        let an_hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let orphans = schema.store.orphaned_runs(an_hour_ago).await.unwrap();
        assert_eq!(orphans.len(), 1);
    }
}

#[tokio::test]
async fn a_bad_url_is_refused_without_quoting_it() {
    let error = PgStore::connect("postgres://henk:hunter2@[::1")
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("hunter2"), "{error}");
}

#[test]
fn a_url_is_described_without_its_credentials() {
    let place =
        henk_store::describe_url("postgres://henk:hunter2@db.internal:5433/henk?sslmode=require")
            .unwrap();
    assert_eq!(place, "db.internal:5433/henk");
}
