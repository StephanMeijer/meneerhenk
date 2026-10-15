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
    DraftDecision, DraftRecord, DraftVerdict, EventFacets, EventFilter, EventKey, FindingAction,
    InboundEvent, LOOP_FINDING_KIND, MAX_PAYLOAD_BYTES, NewRun, OutcomeFilter, OutcomeRecord,
    PLAN_POINT_KIND, Page, PgStore, PruneCounts, REQUIREMENT_POINT_KIND, RoundRecord, RunFilter,
    RunKey, RunRecord, RunStatus, RunStore, SessionRole, SessionStatus, SqliteStore, Stage,
    StageState, StageWrite, ThinStore, TokenUsage, ToolCallFilter, ToolCallKey, ToolCallRecord,
    TranscriptRecord, UNKNOWN_TOOL,
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
            a_superseded_run_names_the_run_that_replaced_it,
            sessions_that_timed_out_or_did_not_finish_say_so,
            stages_come_back_in_order_and_keep_when_they_started,
            a_ticket_run_keeps_its_kind_and_its_stages_in_pipeline_order,
            a_ticket_run_knows_the_runs_it_started,
            days_count_runs_findings_and_drafts,
            event_facets_are_the_sources_and_kinds_seen,
            sessions_and_stages_of_many_runs_come_in_one_read,
            session_endings_are_the_ended_reviews_newest_first,
            a_heartbeat_moves_and_a_check_id_is_kept,
            only_running_runs_with_a_stale_heartbeat_are_orphaned,
            the_newest_review_of_each_pull_request_is_read_for_resuming,
            a_resume_is_claimed_once,
            dropping_running_sessions_leaves_finished_ones_alone,
            sessions_findings_and_events_attach_to_a_run,
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
            loop_findings_keep_their_verdicts,
            a_loop_run_keeps_why_it_stopped,
            tool_calls_are_tallied_and_listed_across_runs_by_filter,
            unknown_tools_count_as_one_and_are_listed_by_their_own_names,
            a_session_s_calls_count_under_its_stored_role,
            a_session_keeps_what_the_cache_served,
            a_pairing_s_rounds_come_back_in_the_order_they_started,
        );
    };
}

/// The last schema version with a `lanes` table, per backend (#324).
const SQLITE_LANE_ERA: usize = 15;
const POSTGRES_LANE_ERA: usize = 11;

/// Runs as Henk wrote them before sessions had a role (#324): a review
/// with two lanes and a fact-check, an address run, a review loop whose
/// executor was named `fixer`, and a plan. The same SQL for both backends.
const LANE_ERA: &str = "
INSERT INTO runs (id, kind, platform, repo, target, trigger, status, started_at, finished_at, link)
VALUES
 ('r-lanes', 'review', 'github', 'o/r', 7, 'opened', 'finished', '2026-10-01T10:00:00Z', '2026-10-01T10:30:00Z', 'l'),
 ('r-address', 'address', 'github', 'o/r', 7, 'dashboard', 'finished', '2026-10-02T10:00:00Z', '2026-10-02T10:30:00Z', 'l'),
 ('r-loop', 'review', 'github', 'o/r', 7, 'synchronize', 'finished', '2026-10-03T10:00:00Z', '2026-10-03T10:30:00Z', 'l'),
 ('r-plan', 'plan', 'github', 'o/r', 8, 'labeled', 'finished', '2026-10-04T10:00:00Z', '2026-10-04T10:30:00Z', 'l');
UPDATE runs SET loop_stop = 'clean', loop_rounds = 2 WHERE id = 'r-loop';
INSERT INTO lanes (run_id, name, model, status, turns, input_tokens, output_tokens, started_at, finished_at, error, cache_read_tokens, cache_write_tokens)
VALUES
 ('r-lanes', 'lane-a', 'm-a', 'finished', 3, 100, 10, '2026-10-01T10:01:00Z', '2026-10-01T10:10:00Z', NULL, 0, 0),
 ('r-lanes', 'lane-b', 'm-b', 'dropped', 2, 50, 5, '2026-10-01T10:01:00Z', '2026-10-01T10:05:00Z', 'gave up', 0, 0),
 ('r-lanes', 'check-1', 'm-c', 'timed_out', 4, 70, 7, '2026-10-01T10:11:00Z', '2026-10-01T10:20:00Z', NULL, 0, 0),
 ('r-address', 'address', 'm-d', 'finished', 6, 300, 30, '2026-10-02T10:01:00Z', '2026-10-02T10:20:00Z', NULL, 0, 0),
 ('r-loop', 'reviewer', 'm-r', 'finished', 5, 100, 10, '2026-10-03T10:01:00Z', '2026-10-03T10:20:00Z', NULL, 80, 20),
 ('r-loop', 'fixer', 'm-f', 'finished', 4, 90, 9, '2026-10-03T10:02:00Z', '2026-10-03T10:20:00Z', NULL, 0, 0),
 ('r-plan', 'planner', 'm-p', 'finished', 2, 40, 4, '2026-10-04T10:01:00Z', '2026-10-04T10:20:00Z', NULL, 0, 0);
INSERT INTO stages (run_id, stage, state, started_at, ended_at, detail)
VALUES
 ('r-lanes', 'lanes', 'done', '2026-10-01T10:01:00Z', '2026-10-01T10:10:00Z', '1 of 2 lanes finished'),
 ('r-lanes', 'fact_check', 'done', '2026-10-01T10:11:00Z', '2026-10-01T10:20:00Z', '1 draft checked'),
 ('r-loop', 'lanes', 'done', '2026-10-03T10:01:00Z', '2026-10-03T10:20:00Z', '2 rounds');
INSERT INTO drafts (run_id, draft, lane, model, kind, path, line, target, body, created_at)
VALUES ('r-lanes', 'd1', 'lane-a', 'm-a', 'finding', 'a.rs', 4, '', 'x is never set.', '2026-10-01T10:05:00Z');
INSERT INTO findings (run_id, lane, path, line, comment_id, action, created_at)
VALUES ('r-lanes', 'lane-a', 'a.rs', 4, 'c-1', 'posted', '2026-10-01T10:21:00Z');
INSERT INTO tool_calls (run_id, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms, at)
VALUES
 ('r-lanes', 'lane-a', 'm-a', 1, 'read_file', 'henk', 'ok', '{}', 2, 10, 1, '2026-10-01T10:02:00Z'),
 ('r-lanes', 'check-1', 'm-c', 1, 'read_file', 'henk', 'ok', '{}', 2, 10, 1, '2026-10-01T10:12:00Z'),
 ('r-address', 'address', 'm-d', 1, 'edit_file', 'henk', 'ok', '{}', 2, 10, 1, '2026-10-02T10:02:00Z'),
 ('r-loop', 'reviewer', 'm-r', 1, 'read_file', 'henk', 'ok', '{}', 2, 10, 1, '2026-10-03T10:02:00Z'),
 ('r-loop', 'fixer', 'm-f', 1, 'edit_file', 'henk', 'ok', '{}', 2, 10, 1, '2026-10-03T10:03:00Z'),
 ('r-plan', 'planner', 'm-p', 1, 'bash', 'henk', 'ok', '{}', 2, 10, 1, '2026-10-04T10:02:00Z');
";

/// After migrating, the runs of [`LANE_ERA`] read back in full: every
/// session with the role its name gave it, drafts and findings by
/// session, the old stages, and tool calls by role. A new session is
/// written as before.
async fn a_lane_era_database_reads_back_in_full(store: &dyn RunStore) {
    let sessions = |run: &'static str| async move {
        store
            .sessions(&id(run))
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.name, s.role, s.status))
            .collect::<Vec<_>>()
    };
    let row = |name: &str, role, status| (name.to_owned(), role, status);
    assert_eq!(
        sessions("r-lanes").await,
        [
            row("check-1", SessionRole::Check, SessionStatus::TimedOut),
            row("lane-a", SessionRole::Lane, SessionStatus::Finished),
            row("lane-b", SessionRole::Lane, SessionStatus::DidNotFinish),
        ]
    );
    assert_eq!(
        sessions("r-address").await,
        [row(
            "address",
            SessionRole::Address,
            SessionStatus::Finished
        )]
    );
    assert_eq!(
        sessions("r-loop").await,
        [
            row("fixer", SessionRole::Executor, SessionStatus::Finished),
            row("reviewer", SessionRole::Reviewer, SessionStatus::Finished),
        ]
    );
    assert_eq!(
        sessions("r-plan").await,
        [row(
            "planner",
            SessionRole::Planner,
            SessionStatus::Finished
        )]
    );
    let reviewer = store.sessions(&id("r-loop")).await.unwrap().pop().unwrap();
    assert_eq!(
        (
            reviewer.turns,
            reviewer.input_tokens,
            reviewer.cache_read_tokens,
            reviewer.cache_write_tokens
        ),
        (5, 100, 80, 20),
        "counts and cache kept"
    );

    let drafts = store.drafts(&id("r-lanes")).await.unwrap();
    assert_eq!(
        (drafts[0].draft.as_str(), drafts[0].session.as_str()),
        ("d1", "lane-a")
    );
    let findings = store.findings(&id("r-lanes")).await.unwrap();
    assert_eq!(
        (
            findings[0].session.as_str(),
            findings[0].comment_id.as_str()
        ),
        ("lane-a", "c-1")
    );
    let stages: Vec<_> = store
        .stages(&id("r-lanes"))
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.stage)
        .collect();
    assert_eq!(stages, [Stage::Lanes, Stage::FactCheck]);

    let runs = [id("r-lanes"), id("r-address"), id("r-loop"), id("r-plan")];
    let dots = store.sessions_of(&runs).await.unwrap();
    assert_eq!(dots.len(), 7);
    assert!(
        dots.iter()
            .any(|(run, name, role, _)| run.as_str() == "r-loop"
                && name == "fixer"
                && *role == SessionRole::Executor),
        "{dots:?}"
    );
    let endings: Vec<_> = store
        .session_endings(None, 30)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.run_id.as_str().to_owned(), e.name, e.role))
        .collect();
    let ending = |run: &str, name: &str, role| (run.to_owned(), name.to_owned(), role);
    assert_eq!(
        endings,
        [
            ending("r-loop", "fixer", SessionRole::Executor),
            ending("r-loop", "reviewer", SessionRole::Reviewer),
            ending("r-lanes", "check-1", SessionRole::Check),
            ending("r-lanes", "lane-a", SessionRole::Lane),
            ending("r-lanes", "lane-b", SessionRole::Lane),
        ],
        "reviews only, newest first"
    );

    for (role, session) in [
        (SessionRole::Lane, "lane-a"),
        (SessionRole::Check, "check-1"),
        (SessionRole::Address, "address"),
        (SessionRole::Executor, "fixer"),
        (SessionRole::Reviewer, "reviewer"),
        (SessionRole::Planner, "planner"),
    ] {
        let filter = ToolCallFilter {
            role: Some(role),
            ..ToolCallFilter::default()
        };
        let listed: Vec<_> = store
            .list_tool_calls(&filter, Page::new(50, 0))
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.call.session)
            .collect();
        assert_eq!(listed, [session], "{role}");
        let usage = store.tool_usage(&filter).await.unwrap();
        assert_eq!(
            usage
                .iter()
                .map(|u| (u.session.as_str(), u.tally.calls))
                .collect::<Vec<_>>(),
            [(role.as_str(), 1)],
            "{role}"
        );
    }

    store
        .start_session(&id("r-plan"), "reviewer", SessionRole::Reviewer, "m-r")
        .await
        .unwrap();
    assert_eq!(
        sessions("r-plan").await[1],
        row("reviewer", SessionRole::Reviewer, SessionStatus::Running),
        "a new session is written with its role"
    );
}

fn draft(number: &str, session: &str, row: u32, body: &str) -> DraftRecord {
    DraftRecord {
        at: String::new(),
        draft: number.into(),
        session: session.into(),
        model: "opus".into(),
        kind: "finding".into(),
        path: "src/a.rs".into(),
        line: row,
        target: String::new(),
        body: body.into(),
        decision: None,
    }
}

/// A session's calls count under the role stored with it (#324), not one
/// read from its name: `x` ran as the reviewer, and the `fixer` of a run
/// before #323 was migrated to the executor.
async fn a_session_s_calls_count_under_its_stored_role(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let sessions = [
        ("reviewer", SessionRole::Reviewer),
        ("x", SessionRole::Reviewer),
        ("executor", SessionRole::Executor),
        ("fixer", SessionRole::Executor),
        ("lane-a", SessionRole::Lane),
    ];
    for (turn, (session, role)) in (1u32..).zip(sessions) {
        store
            .start_session(&id("r-1"), session, role, "opus")
            .await
            .unwrap();
        store
            .record_tool_call(
                &id("r-1"),
                &ToolCallRecord {
                    at: format!("2026-10-07T10:0{turn}:00Z"),
                    session: session.into(),
                    model: "opus".into(),
                    turn,
                    tool: "read_file".into(),
                    origin: "henk".into(),
                    outcome: "ok".into(),
                    arguments: "{}".into(),
                    arguments_len: 2,
                    result_chars: 10,
                    elapsed_ms: 1,
                },
            )
            .await
            .unwrap();
    }
    for (role, calls) in [
        (SessionRole::Reviewer, 2),
        (SessionRole::Executor, 2),
        (SessionRole::Lane, 1),
    ] {
        let usage = store
            .tool_usage(&ToolCallFilter {
                role: Some(role),
                ..ToolCallFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(
            usage
                .into_iter()
                .map(|u| (u.session, u.tally.calls))
                .collect::<Vec<_>>(),
            [(role.as_str().to_owned(), calls)],
            "{role}"
        );
        let listed = store
            .list_tool_calls(
                &ToolCallFilter {
                    role: Some(role),
                    ..ToolCallFilter::default()
                },
                Page::new(50, 0),
            )
            .await
            .unwrap();
        assert_eq!(u64::try_from(listed.len()).unwrap(), calls, "{role}");
    }
}

async fn tool_calls_are_tallied_and_listed_across_runs_by_filter(store: &dyn RunStore) {
    for run in ["r-1", "r-3"] {
        store.create_run(&new_run(run)).await.unwrap();
    }
    store
        .create_run(&NewRun {
            kind: RunKind::Plan,
            ..new_run("r-2")
        })
        .await
        .unwrap();
    for (run, session, role) in [
        ("r-1", "lane-a", SessionRole::Lane),
        ("r-1", "check-1", SessionRole::Check),
        ("r-2", "lane-b", SessionRole::Lane),
        ("r-2", "planner", SessionRole::Planner),
        ("r-3", "address", SessionRole::Address),
        ("r-3", "lane-a", SessionRole::Lane),
    ] {
        store
            .start_session(&id(run), session, role, "m")
            .await
            .unwrap();
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
            role: Some(SessionRole::Check),
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
    let lane_usage = store
        .tool_usage(&ToolCallFilter {
            role: Some(SessionRole::Lane),
            since: Some("2026-10-07T10:02:00Z".into()),
            until: Some("2026-10-07T10:06:00Z".into()),
            ..ToolCallFilter::default()
        })
        .await
        .unwrap();
    let mut lane_usage = tallies(lane_usage);
    lane_usage.sort();
    assert_eq!(
        lane_usage,
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
            role: Some(SessionRole::Planner),
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
    assert_eq!(first[0].kind, RunKind::Review);
    let planner = store
        .list_tool_calls(
            &ToolCallFilter {
                role: Some(SessionRole::Planner),
                ..ToolCallFilter::default()
            },
            Page::new(50, 0),
        )
        .await
        .unwrap()
        .into_iter()
        .map(|c| (c.run_id, c.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        planner,
        [("r-2".to_owned(), RunKind::Plan)],
        "the run's kind"
    );

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

/// A review loop's run keeps why it stopped and after how many rounds
/// (#286); any other run has neither.
async fn a_loop_run_keeps_why_it_stopped(store: &dyn RunStore) {
    for id in ["r-loop", "r-lanes"] {
        store.create_run(&new_run(id)).await.unwrap();
    }
    let looped = RunId::parse("r-loop").unwrap();
    store
        .end_loop(&looped, "repeating_finding", 2)
        .await
        .unwrap();
    let run = store.run(&looped).await.unwrap().unwrap();
    assert_eq!(
        (run.loop_stop.as_deref(), run.loop_rounds),
        (Some("repeating_finding"), Some(2))
    );
    let listed = store
        .list_runs(&RunFilter::default(), Page::new(50, 0))
        .await
        .unwrap();
    let other = listed.iter().find(|r| r.id.as_str() == "r-lanes").unwrap();
    assert_eq!(
        (other.loop_stop.as_deref(), other.loop_rounds),
        (None, None)
    );
}

/// A review loop's findings (#285) keep the executor's verdicts, and stay out
/// of the overview's drafts per day, as the points of a plan review (#328) and
/// of a requirements review (#535) do.
async fn loop_findings_keep_their_verdicts(store: &dyn RunStore) {
    let run = new_run("r-loop");
    store.create_run(&run).await.unwrap();
    store
        .record_draft(&run.id, &draft("d1", "lane-a", 4, "x is never set."))
        .await
        .unwrap();
    let verdicts = [
        DraftVerdict::Fixed,
        DraftVerdict::Rejected,
        DraftVerdict::WontFix,
        DraftVerdict::Unsettled,
        DraftVerdict::Taken,
    ];
    for (n, verdict) in verdicts.into_iter().enumerate() {
        let id = format!("f{}", n + 1);
        let mut finding = draft(&id, "reviewer", 2, "x must be 3");
        LOOP_FINDING_KIND.clone_into(&mut finding.kind);
        store.record_draft(&run.id, &finding).await.unwrap();
        let decision = DraftDecision {
            at: String::new(),
            verdict,
            checker: "executor".into(),
            reason: "why".into(),
            same_as: String::new(),
            comment_id: String::new(),
        };
        store.decide_draft(&run.id, &id, &decision).await.unwrap();
    }
    let mut point = draft("p1", "reviewer", 0, "step 2 misses the migration");
    PLAN_POINT_KIND.clone_into(&mut point.kind);
    store.record_draft(&run.id, &point).await.unwrap();
    let mut requirement = draft("p2", "reviewer", 0, "criterion 2 is not met");
    REQUIREMENT_POINT_KIND.clone_into(&mut requirement.kind);
    store.record_draft(&run.id, &requirement).await.unwrap();
    let stored: Vec<_> = store
        .drafts(&run.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|d| d.decision.map(|decision| decision.verdict))
        .collect();
    assert_eq!(stored, verdicts, "every loop verdict reads back");

    let yesterday = OffsetDateTime::now_utc() - time::Duration::days(1);
    let days = store.daily_stats(yesterday).await.unwrap();
    assert_eq!(
        days.iter().map(|d| d.drafts).sum::<u64>(),
        1,
        "the overview's drafts per day leave loop findings, plan points and requirement points out too"
    );
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

async fn unknown_tools_count_as_one_and_are_listed_by_their_own_names(store: &dyn RunStore) {
    let run = new_run("r-unknown");
    store.create_run(&run).await.unwrap();
    for (turn, (tool, origin, outcome)) in (1u32..).zip([
        ("nope", "", "unknown_tool"),
        ("Review complete. End. [cut]", "", "refused_repeat"),
        ("read_file", "workspace", "ok"),
    ]) {
        store
            .record_tool_call(
                &run.id,
                &ToolCallRecord {
                    turn,
                    origin: origin.into(),
                    ..tool_call("reviewer", "m", tool, outcome, 1)
                },
            )
            .await
            .unwrap();
    }
    let mut usage: Vec<(String, u64, u64, u64)> = store
        .tool_usage(&ToolCallFilter::default())
        .await
        .unwrap()
        .into_iter()
        .map(|u| (u.tool, u.tally.calls, u.tally.refusals, u.tally.other))
        .collect();
    usage.sort();
    assert_eq!(
        usage,
        [
            (UNKNOWN_TOOL.to_owned(), 2, 1, 1),
            ("read_file".to_owned(), 1, 0, 0)
        ]
    );
    let unknown = ToolCallFilter {
        tool: Some(UNKNOWN_TOOL.into()),
        ..ToolCallFilter::default()
    };
    let mut listed: Vec<String> = store
        .list_tool_calls(&unknown, Page::new(50, 0))
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.call.tool)
        .collect();
    listed.sort();
    assert_eq!(listed, ["Review complete. End. [cut]", "nope"]);
    let usage = store.tool_usage(&unknown).await.unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].tally.calls, 2);
    let by_own_name = ToolCallFilter {
        tool: Some("nope".into()),
        ..ToolCallFilter::default()
    };
    assert!(
        store
            .list_tool_calls(&by_own_name, Page::new(50, 0))
            .await
            .unwrap()
            .is_empty(),
        "an unknown tool is found as the one it counts under"
    );
}

async fn tool_calls_are_kept_in_order_and_add_up_across_runs(store: &dyn RunStore) {
    let before = OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let (one, two) = (new_run("r-tools-1"), new_run("r-tools-2"));
    store.create_run(&one).await.unwrap();
    store.create_run(&two).await.unwrap();
    for (run, session, role) in [
        (&one.id, "lane-a", SessionRole::Lane),
        (&one.id, "check-lane-a-1", SessionRole::Check),
        (&two.id, "lane-b", SessionRole::Lane),
    ] {
        store.start_session(run, session, role, "m").await.unwrap();
    }
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

async fn a_superseded_run_names_the_run_that_replaced_it(store: &dyn RunStore) {
    store.create_run(&new_run("r-old")).await.unwrap();
    store.create_run(&new_run("r-new")).await.unwrap();
    store
        .supersede_run(
            &id("r-old"),
            &id("r-new"),
            "superseded by a review of a newer commit",
        )
        .await
        .unwrap();
    let old = store.run(&id("r-old")).await.unwrap().unwrap();
    assert_eq!(old.status, RunStatus::Superseded);
    assert_eq!(old.superseded_by, Some(id("r-new")));
    assert_eq!(
        old.error.as_deref(),
        Some("superseded by a review of a newer commit")
    );
    assert!(old.finished_at.is_some());
    let new = store.run(&id("r-new")).await.unwrap().unwrap();
    assert_eq!(new.superseded_by, None);
    assert_eq!(new.status, RunStatus::Running);

    let superseded = RunFilter {
        status: Some(RunStatus::Superseded),
        ..RunFilter::default()
    };
    let listed = store
        .list_runs(&superseded, Page::new(50, 0))
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|r| r.id.to_string()).collect::<Vec<_>>(),
        ["r-old"]
    );
    assert_eq!(listed[0].superseded_by, Some(id("r-new")));
}

/// #374: each round of a pairing is kept with its role, its handover and
/// its turns, in the order the rounds started.
async fn a_pairing_s_rounds_come_back_in_the_order_they_started(store: &dyn RunStore) {
    store.create_run(&new_run("r-rounds")).await.unwrap();
    store.create_run(&new_run("r-none")).await.unwrap();
    let run = id("r-rounds");
    let round = |session: &str, role, round, at: &str, first_turn, handover: &str| RoundRecord {
        session: session.into(),
        role,
        round,
        started_at: at.into(),
        ended_at: None,
        first_turn,
        turns: 0,
        handover: handover.into(),
        handover_len: 9000,
    };
    // Recorded out of order: they come back in the order they started.
    for record in [
        round(
            "reviewer",
            SessionRole::Reviewer,
            2,
            "2026-10-07T10:02:00.5Z",
            6,
            "Round 2.",
        ),
        round(
            "reviewer",
            SessionRole::Reviewer,
            1,
            "2026-10-07T10:00:00Z",
            1,
            "Review this.",
        ),
        round(
            "executor",
            SessionRole::Executor,
            1,
            "2026-10-07T10:01:00Z",
            1,
            "Round 1. The findings.",
        ),
    ] {
        store.start_round(&run, &record).await.unwrap();
    }
    store.end_round(&run, "reviewer", 1, 5).await.unwrap();
    store.end_round(&run, "executor", 1, 3).await.unwrap();

    let rounds = store.rounds(&run).await.unwrap();
    assert_eq!(
        rounds
            .iter()
            .map(|r| (r.session.as_str(), r.role, r.round, r.first_turn, r.turns))
            .collect::<Vec<_>>(),
        [
            ("reviewer", SessionRole::Reviewer, 1, 1, 5),
            ("executor", SessionRole::Executor, 1, 1, 3),
            ("reviewer", SessionRole::Reviewer, 2, 6, 0),
        ]
    );
    assert!(rounds[0].ended_at.is_some() && rounds[1].ended_at.is_some());
    assert_eq!(rounds[2].ended_at, None, "still going");
    assert_eq!(
        (rounds[1].handover.as_str(), rounds[1].handover_len),
        ("Round 1. The findings.", 9000)
    );
    assert!(
        OffsetDateTime::parse(&rounds[0].started_at, &Rfc3339).is_ok(),
        "{}",
        rounds[0].started_at
    );
    assert!(store.rounds(&id("r-none")).await.unwrap().is_empty());
}

/// #330: a session keeps its cache reads and writes beside its tokens in,
/// which stay the whole prompt; a session that used no cache reads 0.
async fn a_session_keeps_what_the_cache_served(store: &dyn RunStore) {
    store.create_run(&new_run("r-cache")).await.unwrap();
    for (session, role) in [
        ("reviewer", SessionRole::Reviewer),
        ("executor", SessionRole::Executor),
    ] {
        store
            .start_session(&id("r-cache"), session, role, "m")
            .await
            .unwrap();
    }
    let cached = TokenUsage {
        input: 10_000,
        output: 300,
        cache_read: 8_000,
        cache_write: 1_500,
        reasoning: 120,
    };
    store
        .finish_session(
            &id("r-cache"),
            "reviewer",
            SessionStatus::Finished,
            4,
            cached,
            None,
        )
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-cache"),
            "executor",
            SessionStatus::Finished,
            2,
            TokenUsage::new(500, 50),
            None,
        )
        .await
        .unwrap();
    let sessions = store.sessions(&id("r-cache")).await.unwrap();
    let read = |name: &str| {
        let session = sessions.iter().find(|s| s.name == name).unwrap();
        (
            session.input_tokens,
            session.output_tokens,
            session.cache_read_tokens,
            session.cache_write_tokens,
            session.reasoning_tokens,
        )
    };
    assert_eq!(
        sessions.iter().map(|s| s.role).collect::<Vec<_>>(),
        [SessionRole::Executor, SessionRole::Reviewer],
        "by name, each with the role it started in"
    );
    assert_eq!(read("reviewer"), (10_000, 300, 8_000, 1_500, 120));
    assert_eq!(read("executor"), (500, 50, 0, 0, 0));
}

async fn sessions_that_timed_out_or_did_not_finish_say_so(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    for lane in ["a", "b", "c"] {
        store
            .start_session(&id("r-1"), lane, SessionRole::Lane, "m")
            .await
            .unwrap();
    }
    store
        .finish_session(
            &id("r-1"),
            "a",
            SessionStatus::Finished,
            9,
            TokenUsage::new(1, 1),
            None,
        )
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-1"),
            "b",
            SessionStatus::TimedOut,
            30,
            TokenUsage::new(1, 1),
            None,
        )
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-1"),
            "c",
            SessionStatus::DidNotFinish,
            2,
            TokenUsage::new(1, 1),
            Some("rate limited"),
        )
        .await
        .unwrap();
    let lanes = store.sessions(&id("r-1")).await.unwrap();
    let statuses: Vec<_> = lanes.iter().map(|l| (l.name.as_str(), l.status)).collect();
    assert_eq!(
        statuses,
        [
            ("a", SessionStatus::Finished),
            ("b", SessionStatus::TimedOut),
            ("c", SessionStatus::DidNotFinish),
        ]
    );
    assert_eq!(lanes[2].error.as_deref(), Some("rate limited"));
}

/// #536: a ticket run reads back as one, and its plan, execute and review
/// stages come back between `started` and `done`, whatever order they were
/// written in.
async fn a_ticket_run_keeps_its_kind_and_its_stages_in_pipeline_order(store: &dyn RunStore) {
    let mut ticket = new_run("r-ticket");
    ticket.kind = RunKind::Ticket;
    ticket.commit = None;
    store.create_run(&ticket).await.unwrap();
    let run = id("r-ticket");
    assert_eq!(
        store.run(&run).await.unwrap().unwrap().kind,
        RunKind::Ticket
    );
    for stage in [
        Stage::Done,
        Stage::Review,
        Stage::Requested,
        Stage::Plan,
        Stage::Execute,
        Stage::Started,
    ] {
        store
            .stage(&run, &StageWrite::now(stage, StageState::Done, "x"))
            .await
            .unwrap();
    }
    let stages = store.stages(&run).await.unwrap();
    assert_eq!(
        stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
        [
            Stage::Requested,
            Stage::Started,
            Stage::Plan,
            Stage::Execute,
            Stage::Review,
            Stage::Done
        ]
    );
    let found = store
        .list_runs(
            &RunFilter {
                kind: Some(RunKind::Ticket),
                ..RunFilter::default()
            },
            Page::new(10, 0),
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "the kind filter finds it");
}

/// #536: a ticket run links the runs it started, one per stage, whether or
/// not the run exists yet, and reads them back in the order of the stages.
async fn a_ticket_run_knows_the_runs_it_started(store: &dyn RunStore) {
    let mut ticket = new_run("r-ticket");
    ticket.kind = RunKind::Ticket;
    store.create_run(&ticket).await.unwrap();
    let parent = id("r-ticket");
    assert!(store.children(&parent).await.unwrap().is_empty());

    // Linked as they start, the review's run not stored yet.
    store
        .link_child(&parent, Stage::Review, &id("r-review"))
        .await
        .unwrap();
    store
        .link_child(&parent, Stage::Plan, &id("r-plan"))
        .await
        .unwrap();
    store
        .link_child(&parent, Stage::Execute, &id("r-execute"))
        .await
        .unwrap();
    assert_eq!(
        store.children(&parent).await.unwrap(),
        [
            (Stage::Plan, id("r-plan")),
            (Stage::Execute, id("r-execute")),
            (Stage::Review, id("r-review")),
        ],
        "in the order of the stages, not as linked"
    );

    // A stage linked again names the run that replaced it.
    store
        .link_child(&parent, Stage::Review, &id("r-review-2"))
        .await
        .unwrap();
    let children = store.children(&parent).await.unwrap();
    assert_eq!(children.len(), 3);
    assert_eq!(children[2], (Stage::Review, id("r-review-2")));

    // Another run's children are its own, and a run that is none has none.
    let mut other = new_run("r-other");
    other.kind = RunKind::Ticket;
    store.create_run(&other).await.unwrap();
    assert!(store.children(&id("r-other")).await.unwrap().is_empty());
    assert!(store.children(&id("r-nobody")).await.unwrap().is_empty());

    // A link needs its parent to be a run.
    assert!(
        store
            .link_child(&id("r-nobody"), Stage::Plan, &id("r-plan"))
            .await
            .is_err()
    );
}

async fn stages_come_back_in_order_and_keep_when_they_started(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let run = id("r-1");
    let submitted = OffsetDateTime::now_utc() - time::Duration::seconds(30);
    let at = |text: &str| OffsetDateTime::parse(text, &Rfc3339).unwrap();
    store
        .stage(
            &run,
            &StageWrite {
                stage: Stage::Queued,
                state: StageState::Done,
                detail: "waited for a review slot".into(),
                started_at: Some(submitted),
                ended_at: None,
            },
        )
        .await
        .unwrap();
    store
        .stage(
            &run,
            &StageWrite::now(Stage::Rounds, StageState::Running, ""),
        )
        .await
        .unwrap();
    store
        .stage(&run, &StageWrite::now(Stage::Diff, StageState::Running, ""))
        .await
        .unwrap();
    let first = store.stages(&run).await.unwrap();
    let diff_started = first
        .iter()
        .find(|s| s.stage == Stage::Diff)
        .unwrap()
        .started_at
        .clone();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store
        .stage(
            &run,
            &StageWrite::now(Stage::Diff, StageState::Done, "12 files, +340 -25"),
        )
        .await
        .unwrap();

    let stages = store.stages(&run).await.unwrap();
    assert_eq!(
        stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
        [Stage::Queued, Stage::Diff, Stage::Rounds],
        "in the order the stages come, not as written"
    );
    let queued = &stages[0];
    assert_eq!(queued.state, StageState::Done);
    assert!((at(&queued.started_at) - submitted).abs() < time::Duration::seconds(1));
    assert!(queued.ended_at.is_some(), "an ended stage says when");
    let diff = &stages[1];
    assert_eq!(diff.state, StageState::Done);
    assert_eq!(diff.detail, "12 files, +340 -25");
    assert_eq!(
        at(&diff.started_at),
        at(&diff_started),
        "the first write keeps its start"
    );
    assert!(at(diff.ended_at.as_deref().unwrap()) >= at(&diff.started_at));
    assert_eq!(stages[2].ended_at, None, "a running stage has not ended");

    store
        .fail_running_stages(&run, "interrupted")
        .await
        .unwrap();
    let lanes = store.stages(&run).await.unwrap().pop().unwrap();
    assert_eq!(
        (lanes.state, lanes.detail.as_str()),
        (StageState::Failed, "interrupted")
    );
    assert!(lanes.ended_at.is_some());
    assert_eq!(store.stages(&id("r-none")).await.unwrap(), []);

    store
        .start_session(&run, "a", SessionRole::Lane, "m")
        .await
        .unwrap();
    let lane = &store.sessions(&run).await.unwrap()[0];
    assert!(OffsetDateTime::parse(&lane.started_at, &Rfc3339).is_ok());
    assert_eq!(lane.finished_at, None);
    store
        .finish_session(
            &run,
            "a",
            SessionStatus::Finished,
            1,
            TokenUsage::new(1, 1),
            None,
        )
        .await
        .unwrap();
    assert!(store.sessions(&run).await.unwrap()[0].finished_at.is_some());
}

async fn event_facets_are_the_sources_and_kinds_seen(store: &dyn RunStore) {
    assert_eq!(store.event_facets().await.unwrap(), EventFacets::default());
    for (id, source, kind) in [
        ("e-1", "github_webhook", "pull_request"),
        ("e-2", "dashboard", "review_requested"),
        ("e-3", "github_webhook", "issue_comment"),
        ("e-4", "api", "pull_request"),
    ] {
        store
            .record_event(&InboundEvent {
                source: source.into(),
                kind: kind.into(),
                ..inbound(id, "2026-10-07T10:00:00Z")
            })
            .await
            .unwrap();
    }
    let facets = store.event_facets().await.unwrap();
    assert_eq!(facets.sources, ["api", "dashboard", "github_webhook"]);
    assert_eq!(
        facets.kinds,
        ["issue_comment", "pull_request", "review_requested"]
    );
}

async fn days_count_runs_findings_and_drafts(store: &dyn RunStore) {
    for (run, status) in [
        ("r-1", Some(RunStatus::Finished)),
        ("r-2", Some(RunStatus::Failed)),
        ("r-3", Some(RunStatus::Cancelled)),
        ("r-4", Some(RunStatus::Superseded)),
        ("r-5", None),
    ] {
        store.create_run(&new_run(run)).await.unwrap();
        if let Some(status) = status {
            store
                .finish_run(&id(run), status, None, None)
                .await
                .unwrap();
        }
    }
    for (action, comment) in [
        (FindingAction::Posted, "c1"),
        (FindingAction::Unverified, "c2"),
        (FindingAction::Improved, "c3"),
        (FindingAction::Rejected, "c4"),
    ] {
        store
            .record_finding(&id("r-1"), "a", "x.rs", 1, comment, action)
            .await
            .unwrap();
    }
    for draft in ["d1", "d2", "d3"] {
        store
            .record_draft(
                &id("r-1"),
                &DraftRecord {
                    at: String::new(),
                    draft: draft.into(),
                    session: "a".into(),
                    model: "m".into(),
                    kind: "finding".into(),
                    path: "x.rs".into(),
                    line: 1,
                    target: String::new(),
                    body: "Wrong.".into(),
                    decision: None,
                },
            )
            .await
            .unwrap();
    }
    let yesterday = OffsetDateTime::now_utc() - time::Duration::days(1);
    let days = store.daily_stats(yesterday).await.unwrap();
    // One UTC day, or two if the test ran across midnight.
    assert!(!days.is_empty() && days.len() <= 2, "{days:?}");
    assert!(
        days.iter()
            .all(|d| d.day.len() == 10 && d.day.as_bytes()[4] == b'-')
    );
    let sum = |f: fn(&henk_store::DayCounts) -> u64| days.iter().map(f).sum::<u64>();
    assert_eq!(sum(|d| d.runs), 5);
    assert_eq!(
        (sum(|d| d.finished), sum(|d| d.failed)),
        (1, 1),
        "cancelled, superseded and running runs are in neither"
    );
    assert_eq!(sum(|d| d.findings_posted), 2, "posted and unverified");
    assert_eq!(sum(|d| d.drafts), 3);
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert!(store.daily_stats(later).await.unwrap().is_empty());
}

async fn session_endings_are_the_ended_reviews_newest_first(store: &dyn RunStore) {
    // r-1 and r-2 are ended reviews, r-3 was cancelled, r-4 runs, r-5 was
    // superseded, r-6 is a plan; each has a lane-a, and r-2 a lane-b and a
    // check-1 too.
    for (run, status, kind) in [
        ("r-1", Some(RunStatus::Finished), RunKind::Review),
        ("r-2", Some(RunStatus::Failed), RunKind::Review),
        ("r-3", Some(RunStatus::Cancelled), RunKind::Review),
        ("r-4", None, RunKind::Review),
        ("r-5", Some(RunStatus::Superseded), RunKind::Review),
        ("r-6", Some(RunStatus::Finished), RunKind::Plan),
    ] {
        store
            .create_run(&NewRun {
                kind,
                ..new_run(run)
            })
            .await
            .unwrap();
        store
            .start_session(&id(run), "lane-a", SessionRole::Lane, "m-1")
            .await
            .unwrap();
        store
            .finish_session(
                &id(run),
                "lane-a",
                SessionStatus::Finished,
                1,
                TokenUsage::new(1, 1),
                None,
            )
            .await
            .unwrap();
        if let Some(status) = status {
            store
                .finish_run(&id(run), status, None, None)
                .await
                .unwrap();
        }
    }
    store
        .start_session(&id("r-2"), "lane-b", SessionRole::Lane, "m-2")
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-2"),
            "lane-b",
            SessionStatus::DidNotFinish,
            1,
            TokenUsage::new(1, 1),
            Some("rate limited by the model endpoint"),
        )
        .await
        .unwrap();
    store
        .start_session(&id("r-2"), "check-1", SessionRole::Check, "m-3")
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-2"),
            "check-1",
            SessionStatus::TimedOut,
            1,
            TokenUsage::new(1, 1),
            None,
        )
        .await
        .unwrap();

    let all = store.session_endings(None, 30).await.unwrap();
    let seen: Vec<_> = all
        .iter()
        .map(|e| {
            (
                e.run_id.as_str(),
                e.name.as_str(),
                e.model.as_str(),
                e.status,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("r-2", "check-1", "m-3", SessionStatus::TimedOut),
            ("r-2", "lane-a", "m-1", SessionStatus::Finished),
            ("r-2", "lane-b", "m-2", SessionStatus::DidNotFinish),
            ("r-1", "lane-a", "m-1", SessionStatus::Finished),
        ],
        "running, cancelled, superseded and plan runs are left out"
    );
    let dropped = all.iter().find(|e| e.name == "lane-b").unwrap();
    assert_eq!(
        dropped.error.as_deref(),
        Some("rate limited by the model endpoint")
    );
    assert!(OffsetDateTime::parse(&dropped.started_at, &Rfc3339).is_ok());

    let newest_two: Vec<_> = store
        .session_endings(None, 2)
        .await
        .unwrap()
        .iter()
        .map(|e| e.run_id.as_str().to_owned())
        .collect();
    assert_eq!(
        newest_two,
        ["r-2", "r-2", "r-2", "r-1"],
        "two reviews, not two lanes"
    );

    let hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
    assert_eq!(
        store.session_endings(Some(hour_ago), 30).await.unwrap(),
        all
    );
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert!(
        store
            .session_endings(Some(later), 30)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn sessions_and_stages_of_many_runs_come_in_one_read(store: &dyn RunStore) {
    for run in ["r-1", "r-2", "r-3"] {
        store.create_run(&new_run(run)).await.unwrap();
    }
    store
        .start_session(&id("r-1"), "b", SessionRole::Lane, "m")
        .await
        .unwrap();
    store
        .start_session(&id("r-1"), "a", SessionRole::Lane, "m")
        .await
        .unwrap();
    store
        .start_session(&id("r-2"), "a", SessionRole::Lane, "m")
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-2"),
            "a",
            SessionStatus::TimedOut,
            1,
            TokenUsage::new(1, 1),
            None,
        )
        .await
        .unwrap();
    store
        .stage(
            &id("r-1"),
            &StageWrite::now(Stage::Rounds, StageState::Running, ""),
        )
        .await
        .unwrap();
    store
        .stage(
            &id("r-1"),
            &StageWrite::now(Stage::Diff, StageState::Done, ""),
        )
        .await
        .unwrap();

    let lanes = store
        .sessions_of(&[id("r-1"), id("r-2"), id("r-3")])
        .await
        .unwrap();
    let lanes: Vec<_> = lanes
        .iter()
        .map(|(run, name, role, status)| (run.as_str(), name.as_str(), *role, *status))
        .collect();
    assert_eq!(
        lanes,
        [
            ("r-1", "a", SessionRole::Lane, SessionStatus::Running),
            ("r-1", "b", SessionRole::Lane, SessionStatus::Running),
            ("r-2", "a", SessionRole::Lane, SessionStatus::TimedOut),
        ]
    );
    let stages = store.stages_of(&[id("r-1"), id("r-2")]).await.unwrap();
    let stages: Vec<_> = stages
        .iter()
        .map(|(run, stage, state)| (run.as_str(), *stage, *state))
        .collect();
    assert_eq!(
        stages,
        [
            ("r-1", Stage::Diff, StageState::Done),
            ("r-1", Stage::Rounds, StageState::Running)
        ],
        "each run's stages in the order they come"
    );
    assert!(store.sessions_of(&[]).await.unwrap().is_empty());
    assert!(store.stages_of(&[]).await.unwrap().is_empty());
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

/// #160: a start reads the newest review of each pull request to resume
/// the ones a restart interrupted. A later review of the same pull request
/// takes the place of an earlier one; plans and other pull requests do not.
async fn the_newest_review_of_each_pull_request_is_read_for_resuming(store: &dyn RunStore) {
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    let run = |id: &str, kind: RunKind, repo: &str, target: u64| NewRun {
        kind,
        repo: repo.into(),
        target,
        ..new_run(id)
    };
    for (id, kind, repo, target) in [
        ("r-a", RunKind::Review, "o/r", 7),
        ("r-b", RunKind::Review, "o/r", 7),
        ("r-c", RunKind::Review, "o/r", 8),
        ("r-d", RunKind::Review, "o/r", 9),
        ("r-e", RunKind::Plan, "o/r", 10),
        ("r-f", RunKind::Plan, "o/r", 8),
        ("r-g", RunKind::Review, "o/s", 7),
    ] {
        store
            .create_run(&run(id, kind, repo, target))
            .await
            .unwrap();
    }
    for interrupted in ["r-a", "r-c"] {
        store
            .finish_run(
                &id(interrupted),
                RunStatus::Failed,
                None,
                Some("interrupted"),
            )
            .await
            .unwrap();
    }
    store
        .finish_run(&id("r-b"), RunStatus::Finished, Some("done"), None)
        .await
        .unwrap();

    let latest = store.latest_reviews(before).await.unwrap();
    let ids = latest.iter().map(|r| r.id.to_string()).collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["r-b", "r-c", "r-d", "r-g"],
        "one review per pull request, the newest, in any state; oldest first"
    );
    let c = latest.iter().find(|r| r.id.as_str() == "r-c").unwrap();
    assert_eq!(c.status, RunStatus::Failed);
    assert_eq!(c.error.as_deref(), Some("interrupted"));

    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert!(
        store.latest_reviews(later).await.unwrap().is_empty(),
        "reviews that started before the window are left out"
    );
}

/// #160: of two processes that read the same interrupted review, one
/// claims its resume. The claim needs the run failed with the error it was
/// read with, and changes that error, so a second claim fails.
async fn a_resume_is_claimed_once(store: &dyn RunStore) {
    store.create_run(&new_run("r-running")).await.unwrap();
    store.create_run(&new_run("r-stopped")).await.unwrap();
    store
        .finish_run(
            &id("r-stopped"),
            RunStatus::Failed,
            None,
            Some("interrupted"),
        )
        .await
        .unwrap();
    let resumed = "interrupted, resumed after a restart";

    assert!(
        !store
            .claim_resume(&id("r-running"), "interrupted", resumed)
            .await
            .unwrap(),
        "a running run is not claimed"
    );
    assert!(
        !store
            .claim_resume(&id("r-stopped"), "boom", resumed)
            .await
            .unwrap(),
        "a run that ended with another error is not claimed"
    );
    assert!(
        store
            .claim_resume(&id("r-stopped"), "interrupted", resumed)
            .await
            .unwrap()
    );
    assert!(
        !store
            .claim_resume(&id("r-stopped"), "interrupted", resumed)
            .await
            .unwrap(),
        "a second claim fails"
    );
    let record = store.run(&id("r-stopped")).await.unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);
    assert_eq!(record.error.as_deref(), Some(resumed));
    assert_eq!(
        store.run(&id("r-running")).await.unwrap().unwrap().status,
        RunStatus::Running
    );
}

async fn dropping_running_sessions_leaves_finished_ones_alone(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    store
        .start_session(&id("r-1"), "a", SessionRole::Lane, "m")
        .await
        .unwrap();
    store
        .start_session(&id("r-1"), "b", SessionRole::Lane, "m")
        .await
        .unwrap();
    store
        .finish_session(
            &id("r-1"),
            "a",
            SessionStatus::Finished,
            3,
            TokenUsage::new(1, 1),
            None,
        )
        .await
        .unwrap();
    store
        .drop_running_sessions(&id("r-1"), "interrupted")
        .await
        .unwrap();
    let lanes = store.sessions(&id("r-1")).await.unwrap();
    let a = lanes.iter().find(|l| l.name == "a").unwrap();
    let b = lanes.iter().find(|l| l.name == "b").unwrap();
    assert_eq!(a.status, SessionStatus::Finished);
    assert_eq!(a.error, None);
    assert_eq!(b.status, SessionStatus::DidNotFinish);
    assert_eq!(b.error.as_deref(), Some("interrupted"));
}

async fn sessions_findings_and_events_attach_to_a_run(store: &dyn RunStore) {
    let run = new_run("r-2");
    store.create_run(&run).await.unwrap();
    store
        .start_session(&run.id, "b", SessionRole::Lane, "model-y")
        .await
        .unwrap();
    store
        .start_session(&run.id, "a", SessionRole::Lane, "model-x")
        .await
        .unwrap();
    store
        .finish_session(
            &run.id,
            "a",
            SessionStatus::Finished,
            4,
            TokenUsage::new(1000, 200),
            None,
        )
        .await
        .unwrap();
    store
        .finish_session(
            &run.id,
            "b",
            SessionStatus::DidNotFinish,
            1,
            TokenUsage::new(10, 0),
            Some("rate limited"),
        )
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

    let lanes = store.sessions(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    assert_eq!(lanes[0].name, "a", "lanes come back by name");
    assert_eq!(lanes[0].model, "model-x");
    assert_eq!(lanes[0].status, SessionStatus::Finished);
    assert_eq!(
        (
            lanes[0].turns,
            lanes[0].input_tokens,
            lanes[0].output_tokens
        ),
        (4, 1000, 200)
    );
    assert_eq!(lanes[1].error.as_deref(), Some("rate limited"));

    let events = store.events(&run.id).await.unwrap();
    let messages: Vec<_> = events.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(messages, ["started", "slow"], "timeline is oldest first");
    assert_eq!(events[1].level, "warn");

    let findings = store.findings(&run.id).await.unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].session, "a");
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

/// Runs started in one second list, page and go stale in time order, though
/// their stored fractions differ in length (#216). `start` sets a run's
/// `started_at` to an RFC 3339 time, which a scenario cannot do through the
/// store, so each backend runs this with its own.
async fn runs_in_one_second_list_by_time(store: &dyn RunStore, start: impl AsyncFn(&str, &str)) {
    for (run, at) in [
        ("r-a", "2026-10-07T12:00:00Z"),
        ("r-b", "2026-10-07T12:00:00.4Z"),
        ("r-c", "2026-10-07T12:00:00.45Z"),
    ] {
        store.create_run(&new_run(run)).await.unwrap();
        start(run, at).await;
    }
    let ids = |runs: &[RunRecord]| runs.iter().map(|r| r.id.to_string()).collect::<Vec<_>>();

    let all = store
        .list_runs(&RunFilter::default(), Page::new(50, 0))
        .await
        .unwrap();
    assert_eq!(ids(&all), ["r-c", "r-b", "r-a"], "newest first by time");

    let mut seen = Vec::new();
    let mut filter = RunFilter::default();
    loop {
        let page = store.list_runs(&filter, Page::new(1, 0)).await.unwrap();
        let Some(last) = page.last() else { break };
        let left = store.count_runs(&filter).await.unwrap();
        assert_eq!(left, 3 - seen.len() as u64, "the count takes the keyset");
        filter.before = Some(RunKey {
            started_at: last.started_at.clone(),
            id: last.id.to_string(),
        });
        seen.extend(ids(&page));
    }
    assert_eq!(seen, ["r-c", "r-b", "r-a"], "every run once, in order");

    // Every heartbeat is older than an hour from now: all three are stale.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert_eq!(
        ids(&store.orphaned_runs(later).await.unwrap()),
        ["r-a", "r-b", "r-c"],
        "stale runs oldest first by time"
    );
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
    use std::sync::atomic::{AtomicU64, Ordering};

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

    /// Numbers this process's database files, so no two tests share one.
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    /// A database file under the temp directory, removed with the guard,
    /// for a test that writes to the database behind the store's back.
    struct File(std::path::PathBuf);

    impl File {
        fn new() -> Self {
            let n = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("henk-store-{}-{n}.db", std::process::id())))
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.0.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[tokio::test]
    async fn a_lane_era_database_reads_back_in_full_after_migrating() {
        let file = File::new();
        drop(SqliteStore::open_at(&file.0, SQLITE_LANE_ERA).unwrap());
        rusqlite::Connection::open(&file.0)
            .unwrap()
            .execute_batch(LANE_ERA)
            .unwrap();
        let store = SqliteStore::open(&file.0).unwrap();
        super::a_lane_era_database_reads_back_in_full(&store).await;
    }

    #[tokio::test]
    async fn an_invented_tool_name_from_before_is_cut_by_the_migration() {
        let file = File::new();
        let before = SqliteStore::open_at(&file.0, 18).unwrap();
        let run = super::new_run("r-prose");
        before.create_run(&run).await.unwrap();
        let prose = "Review complete. End. ".repeat(200);
        for tool in [prose.as_str(), "read_file"] {
            before
                .record_tool_call(
                    &run.id,
                    &super::tool_call("reviewer", "m", tool, "unknown_tool", 1),
                )
                .await
                .unwrap();
        }
        drop(before);
        let store = SqliteStore::open(&file.0).unwrap();
        let tools: Vec<String> = store
            .tool_calls(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.tool)
            .collect();
        assert_eq!(
            tools,
            [format!("{} [cut]", &prose[..58]), "read_file".to_owned()]
        );
    }

    #[tokio::test]
    async fn runs_in_one_second_list_by_time() {
        let file = File::new();
        let store = SqliteStore::open(&file.0).unwrap();
        let raw = rusqlite::Connection::open(&file.0).unwrap();
        // Stored as written: the text whose order is not the time's.
        super::runs_in_one_second_list_by_time(&store, async |run, at| {
            raw.execute(
                "UPDATE runs SET started_at = ?2 WHERE id = ?1",
                rusqlite::params![run, at],
            )
            .unwrap();
        })
        .await;
    }
}

/// `PostgreSQL`, when `HENK_TEST_DATABASE_URL` names a server. Every test gets
/// its own schema, so tests run in parallel and leave nothing behind.
mod postgres {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const URL: &str = "HENK_TEST_DATABASE_URL";

    /// Numbers this process's schemas, so no two tests share one: tests
    /// are threads of one process and can read the clock at the same
    /// instant (#164).
    static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

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
        schema_at(usize::MAX).await
    }

    /// A store in a fresh schema migrated only up to `version`.
    pub(super) async fn schema_at(version: usize) -> Schema {
        schema_with(async |url| {
            Arc::new(PgStore::connect_at(url, version).await.unwrap()) as Arc<dyn RunStore>
        })
        .await
    }

    /// The thin store in a fresh schema migrated only up to its step `version`.
    pub(super) async fn thin_schema_at(version: usize) -> Schema {
        schema_with(async |url| {
            Arc::new(ThinStore::connect_at(url, version).await.unwrap()) as Arc<dyn RunStore>
        })
        .await
    }

    async fn schema_with(open: impl AsyncFnOnce(&str) -> Arc<dyn RunStore>) -> Schema {
        let base = std::env::var(URL).unwrap_or_else(|_| panic!("{URL} is not set"));
        // The process id and counter keep tests apart, within one run and
        // across runs at once; the clock keeps a schema a crashed run left
        // behind under a recycled process id from getting in the way (#164).
        let name = format!(
            "henk_test_{}_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
            OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .unsigned_abs()
        );
        plain_client(&base)
            .await
            .unwrap()
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{name}");
        let store = open(&url).await;
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
    async fn a_lane_era_database_reads_back_in_full_after_migrating() {
        let schema = schema_at(POSTGRES_LANE_ERA).await;
        plain_client(&schema.url)
            .await
            .unwrap()
            .batch_execute(LANE_ERA)
            .await
            .unwrap();
        let store = PgStore::connect(&schema.url).await.unwrap();
        super::a_lane_era_database_reads_back_in_full(&store).await;
    }

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

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn runs_in_one_second_list_by_time() {
        let schema = schema().await;
        let client = plain_client(&schema.url).await.unwrap();
        super::runs_in_one_second_list_by_time(schema.store.as_ref(), async |run, at| {
            client
                .execute(
                    "UPDATE runs SET started_at = $2::text::timestamptz WHERE id = $1",
                    &[&run, &at],
                )
                .await
                .unwrap();
        })
        .await;
    }
}

/// The thin store on `SQLite`: every scenario the other `SQLite` store passes.
mod thin_sqlite {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn open() -> Arc<dyn RunStore> {
        Arc::new(ThinStore::in_memory().unwrap())
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

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    /// A database file under the temp directory, removed with the guard.
    struct File(std::path::PathBuf);

    impl File {
        fn new() -> Self {
            let n = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("henk-thin-{}-{n}.db", std::process::id())))
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.0.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[tokio::test]
    async fn a_lane_era_database_reads_back_in_full_after_migrating() {
        let file = File::new();
        drop(ThinStore::open_at(&file.0, SQLITE_LANE_ERA).unwrap());
        rusqlite::Connection::open(&file.0)
            .unwrap()
            .execute_batch(LANE_ERA)
            .unwrap();
        let store = ThinStore::open(&file.0).unwrap();
        super::a_lane_era_database_reads_back_in_full(&store).await;
    }

    #[tokio::test]
    async fn a_lane_era_database_the_old_store_made_reads_back_in_full_after_migrating() {
        let file = File::new();
        drop(SqliteStore::open_at(&file.0, SQLITE_LANE_ERA).unwrap());
        rusqlite::Connection::open(&file.0)
            .unwrap()
            .execute_batch(LANE_ERA)
            .unwrap();
        let store = ThinStore::open(&file.0).unwrap();
        super::a_lane_era_database_reads_back_in_full(&store).await;
    }

    #[tokio::test]
    async fn an_invented_tool_name_from_before_is_cut_by_the_migration() {
        let file = File::new();
        let before = ThinStore::open_at(&file.0, 18).unwrap();
        let run = super::new_run("r-prose");
        before.create_run(&run).await.unwrap();
        let prose = "Review complete. End. ".repeat(200);
        for tool in [prose.as_str(), "read_file"] {
            before
                .record_tool_call(
                    &run.id,
                    &super::tool_call("reviewer", "m", tool, "unknown_tool", 1),
                )
                .await
                .unwrap();
        }
        drop(before);
        let store = ThinStore::open(&file.0).unwrap();
        let tools: Vec<String> = store
            .tool_calls(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.tool)
            .collect();
        assert_eq!(
            tools,
            [format!("{} [cut]", &prose[..58]), "read_file".to_owned()]
        );
    }

    #[tokio::test]
    async fn runs_in_one_second_list_by_time() {
        let file = File::new();
        let store = ThinStore::open(&file.0).unwrap();
        let raw = rusqlite::Connection::open(&file.0).unwrap();
        // Stored as written: the text whose order is not the time's.
        super::runs_in_one_second_list_by_time(&store, async |run, at| {
            raw.execute(
                "UPDATE runs SET started_at = ?2 WHERE id = ?1",
                rusqlite::params![run, at],
            )
            .unwrap();
        })
        .await;
    }

    /// Eight tasks write fifty runs each into one file, through one store
    /// and through two stores on the same file (two processes' worth of
    /// connections): none is refused with "database is locked".
    #[tokio::test]
    async fn concurrent_writers_are_not_refused_as_locked() {
        let file = File::new();
        let stores = [
            Arc::new(ThinStore::open(&file.0).unwrap()),
            Arc::new(ThinStore::open(&file.0).unwrap()),
        ];
        let mut tasks = Vec::new();
        for task in 0..8 {
            let store = Arc::clone(&stores[task % 2]);
            tasks.push(tokio::spawn(async move {
                for n in 0..50 {
                    let run = super::new_run(&format!("r-{task}-{n}"));
                    store.create_run(&run).await?;
                    store.event(&run.id, "info", "written").await?;
                }
                Ok::<_, henk_store::StoreError>(())
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let count = stores[0].count_runs(&RunFilter::default()).await.unwrap();
        assert_eq!(count, 400);
    }

    /// Reads and writes at once: the readers see whole rows, never an error.
    #[tokio::test]
    async fn readers_and_writers_share_a_file() {
        let file = File::new();
        let store = Arc::new(ThinStore::open(&file.0).unwrap());
        let writer = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                for n in 0..100 {
                    store
                        .create_run(&super::new_run(&format!("r-w-{n}")))
                        .await
                        .unwrap();
                }
            })
        };
        let mut readers = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            readers.push(tokio::spawn(async move {
                for _ in 0..50 {
                    store
                        .list_runs(&RunFilter::default(), Page::new(20, 0))
                        .await
                        .unwrap();
                }
            }));
        }
        writer.await.unwrap();
        for reader in readers {
            reader.await.unwrap();
        }
        assert_eq!(store.count_runs(&RunFilter::default()).await.unwrap(), 100);
    }
}

/// The thin store on `PostgreSQL`, when `HENK_TEST_DATABASE_URL` names a server.
mod thin_postgres {
    use super::postgres::{Schema, thin_schema_at};
    use super::*;

    async fn schema() -> Schema {
        thin_schema_at(usize::MAX).await
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
    async fn a_lane_era_database_reads_back_in_full_after_migrating() {
        // Step 15 is the last with a lanes table: PostgreSQL's 11.
        let schema = thin_schema_at(15).await;
        let client = {
            let (client, connection) = tokio_postgres::connect(&schema.url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(connection);
            client
        };
        client.batch_execute(LANE_ERA).await.unwrap();
        let store = ThinStore::connect(&schema.url).await.unwrap();
        super::a_lane_era_database_reads_back_in_full(&store).await;
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn migrating_again_changes_nothing_and_concurrent_starts_agree() {
        let schema = schema().await;
        schema.store.create_run(&new_run("r-kept")).await.unwrap();
        let (a, b) = tokio::join!(
            ThinStore::connect(&schema.url),
            ThinStore::connect(&schema.url)
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(
            a.run(&id("r-kept")).await.unwrap().is_some(),
            "data survives"
        );
        assert!(b.run(&id("r-kept")).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn concurrent_writers_all_succeed() {
        let schema = schema().await;
        let store = Arc::new(ThinStore::connect(&schema.url).await.unwrap());
        let mut tasks = Vec::new();
        for task in 0..8 {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                for n in 0..50 {
                    store
                        .create_run(&super::new_run(&format!("r-{task}-{n}")))
                        .await?;
                }
                Ok::<_, henk_store::StoreError>(())
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(store.count_runs(&RunFilter::default()).await.unwrap(), 400);
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
