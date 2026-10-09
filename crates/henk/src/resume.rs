//! Resumes reviews a restart interrupted (#160). After the reaper's first
//! pass on start, Henk asks again for a review of every pull request whose
//! newest review ended interrupted: stopped with Henk while it ran or
//! waited for a slot, or reaped as an orphan. The request goes through the
//! bus like one from the API, so the allowlist and a closed pull request
//! refuse it there, and the review is of the pull request's current head.
//! A resumed review is a newer run of its pull request, so the interrupted
//! run is no longer the newest and a restart loop never resumes it twice.
//! A resumed review that was itself reaped is not resumed again: its
//! process died while it ran, perhaps because of it, and resuming it once
//! more could take Henk down on every start. Plans are not resumed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use henk_domain::allowlist::RepoRef;
use henk_domain::review::{ReviewTarget, ReviewTrigger};
use henk_domain::run::{RunId, RunKind};
use henk_events::{Event, EventBus, EventKind, EventSource};
use henk_store::{RunRecord, RunStatus};
use time::OffsetDateTime;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::app::App;
use crate::hooks::new_event;
use crate::liveness::REAPED;
use crate::review::Interrupted;

/// How far back a start looks for interrupted reviews. A pull request whose
/// newest review started before then is left alone: its next push or
/// review command reviews it.
pub(crate) const RESUME_WITHIN: Duration = Duration::from_hours(24);

/// Whether `run` is a review to resume: one that ended as interrupted,
/// stopped with Henk or reaped after its process died. A resumed review
/// that was reaped is not: the process died while it ran, and resuming it
/// again would bring a review that crashes Henk back on every start.
fn ended_interrupted(run: &RunRecord) -> bool {
    let resumed = ReviewTrigger::resumed_from(&run.trigger).is_some();
    run.kind == RunKind::Review
        && run.status == RunStatus::Failed
        && run
            .error
            .as_deref()
            .is_some_and(|error| error == Interrupted.to_string() || (error == REAPED && !resumed))
}

/// The reviews to resume: of each pull request whose newest review started
/// at or after `since`, that review, when it ended interrupted. A review
/// still running, on this process or another, is never one of them.
pub(crate) async fn interrupted_reviews(app: &App, since: OffsetDateTime) -> Vec<RunRecord> {
    match app.store.latest_reviews(since).await {
        Ok(latest) => latest.into_iter().filter(ended_interrupted).collect(),
        Err(error) => {
            warn!(%error, "could not list the reviews a restart interrupted");
            Vec::new()
        }
    }
}

/// The request that resumes `run`: a review of its pull request's current
/// head, for whoever asked for the interrupted one.
pub(crate) fn resume_event(run: &RunRecord) -> Option<Event> {
    let repo = match RepoRef::parse(run.platform, &run.repo) {
        Ok(repo) => repo,
        Err(error) => {
            warn!(%error, run = %run.id, repo = %run.repo, "an interrupted review names no valid repository");
            return None;
        }
    };
    Some(new_event(
        EventSource::Resume {
            interrupted: run.id.clone(),
        },
        EventKind::ReviewRequested {
            target: ReviewTarget {
                repo,
                number: run.target,
            },
            commit: None,
            requester: run.requester.clone(),
        },
        None,
    ))
}

/// Resumes interrupted reviews at most twice in a process's life (#160):
/// after the reaper's first pass, and after the first pass that comes
/// [`crate::liveness::STALE_AFTER`] after start. That later pass is the
/// first that can reap what a process that died just before this start
/// left, as its heartbeat was still fresh at start (#47). A review resumed
/// by the first is skipped by the second, also while it still waits for a
/// slot and has no run of its own.
#[derive(Debug)]
pub(crate) struct Resumer {
    bus: Arc<EventBus>,
    begun: Instant,
    stale_after: Duration,
    first_done: bool,
    done: bool,
    resumed: HashSet<RunId>,
}

impl Resumer {
    /// A resumer that publishes on `bus`, for a reaper that counts a run
    /// orphaned after `stale_after`.
    pub(crate) fn new(bus: Arc<EventBus>, stale_after: Duration) -> Self {
        Self {
            bus,
            begun: Instant::now(),
            stale_after,
            first_done: false,
            done: false,
            resumed: HashSet::new(),
        }
    }

    /// Called after every reaper pass, with when that pass began; resumes
    /// on the passes it is due. A pass counts by when it began, as that is
    /// when its cutoff was taken: one that began inside the window may not
    /// have reaped what the window is for. Returns how many reviews it
    /// asked for. Once Henk is told to stop it asks for nothing more: its
    /// own reviews are ending as interrupted then, and a request now would
    /// only be stopped too.
    pub(crate) async fn after_pass(&mut self, app: &App, began: Instant) -> usize {
        if app.shutdown.is_cancelled() {
            self.done = true;
        }
        if self.done {
            return 0;
        }
        let settled = began.saturating_duration_since(self.begun) >= self.stale_after;
        let due = !self.first_done || settled;
        self.first_done = true;
        self.done = settled;
        if !due {
            return 0;
        }
        let since = OffsetDateTime::now_utc() - RESUME_WITHIN;
        let mut asked = 0;
        for run in interrupted_reviews(app, since).await {
            if !self.resumed.insert(run.id.clone()) {
                continue;
            }
            let Some(event) = resume_event(&run) else {
                continue;
            };
            info!(run = %run.id, repo = %run.repo, number = run.target, event = %event.id, "resuming a review a restart interrupted");
            self.bus.publish(event);
            asked += 1;
        }
        if asked > 0 {
            info!(count = asked, "resumed interrupted reviews");
        }
        asked
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Mutex;

    use henk_domain::allowlist::Platform;
    use henk_events::bus::NoRecorder;
    use henk_events::{Handled, Listener};
    use henk_store::NewRun;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::Config;

    const MINIMAL: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
"#;

    /// Notes the run each resume names, and does nothing else.
    #[derive(Default)]
    struct Resumed(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Listener for Resumed {
        fn name(&self) -> &'static str {
            "resumed"
        }

        async fn handle(&self, event: Arc<Event>) -> Handled {
            if let EventSource::Resume { interrupted } = &event.source {
                self.0.lock().unwrap().push(interrupted.to_string());
            }
            Handled::Ignored("noted".to_owned())
        }
    }

    async fn app() -> Arc<App> {
        let settings = Config::parse(MINIMAL).unwrap().into_settings().unwrap();
        Arc::new(
            App::build(settings, Some(std::path::Path::new(":memory:")))
                .await
                .unwrap(),
        )
    }

    fn bus() -> (Arc<EventBus>, Arc<Resumed>) {
        let resumed = Arc::new(Resumed::default());
        let bus = EventBus::new(
            Arc::new(NoRecorder),
            vec![Arc::clone(&resumed) as Arc<dyn Listener>],
        );
        (Arc::new(bus), resumed)
    }

    /// Records a run of `kind` on pull request `target` and ends it with
    /// the status and error in `ended`; `None` leaves it running.
    async fn run(
        app: &App,
        id: &str,
        kind: RunKind,
        target: u64,
        ended: Option<(RunStatus, Option<&str>)>,
    ) {
        run_for(app, id, kind, target, "opened", ended).await;
    }

    /// As [`run`], started by `trigger`.
    async fn run_for(
        app: &App,
        id: &str,
        kind: RunKind,
        target: u64,
        trigger: &str,
        ended: Option<(RunStatus, Option<&str>)>,
    ) {
        let run = RunId::parse(id).unwrap();
        app.store
            .create_run(&NewRun {
                id: run.clone(),
                kind,
                platform: Platform::GitHub,
                repo: "docspec/app".to_owned(),
                target,
                commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                requester: None,
                trigger: trigger.to_owned(),
                link: format!("http://henk/runs/{id}"),
            })
            .await
            .unwrap();
        if let Some((status, error)) = ended {
            app.store
                .finish_run(&run, status, None, error)
                .await
                .unwrap();
        }
    }

    fn ids(runs: &[RunRecord]) -> Vec<&str> {
        runs.iter().map(|r| r.id.as_str()).collect()
    }

    fn an_hour_ago() -> OffsetDateTime {
        OffsetDateTime::now_utc() - time::Duration::hours(1)
    }

    async fn until(what: &str, ready: impl Fn() -> bool) {
        for _ in 0..500 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting until {what}");
    }

    const INTERRUPTED: Option<(RunStatus, Option<&str>)> =
        Some((RunStatus::Failed, Some("interrupted")));

    /// #160: only a pull request's newest review is resumed, and only when
    /// it ended interrupted, by a shutdown or the reaper. Plans, failures
    /// of another kind and running reviews are not.
    #[tokio::test]
    async fn only_a_newest_review_that_ended_interrupted_is_resumed() {
        let app = app().await;
        let reaped = Some((RunStatus::Failed, Some(REAPED)));
        run(&app, "r-1a", RunKind::Review, 1, INTERRUPTED).await;
        run(
            &app,
            "r-1b",
            RunKind::Review,
            1,
            Some((RunStatus::Finished, None)),
        )
        .await;
        run(&app, "r-2", RunKind::Review, 2, reaped).await;
        run(
            &app,
            "r-3",
            RunKind::Review,
            3,
            Some((RunStatus::Failed, Some("boom"))),
        )
        .await;
        run(&app, "r-4", RunKind::Review, 4, None).await;
        run(&app, "r-5", RunKind::Plan, 5, INTERRUPTED).await;
        run(&app, "r-6", RunKind::Review, 6, INTERRUPTED).await;

        let found = interrupted_reviews(&app, an_hour_ago()).await;
        assert_eq!(ids(&found), ["r-2", "r-6"]);
        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        assert!(
            interrupted_reviews(&app, later).await.is_empty(),
            "nothing from before the window"
        );
    }

    /// #160: a restart loop cannot multiply work. The resumed review is
    /// the newer run of its pull request, so the interrupted one is not
    /// resumed again; when the resumed one is interrupted too, it alone is.
    #[tokio::test]
    async fn a_resumed_review_takes_the_place_of_the_one_it_resumes() {
        let app = app().await;
        run(&app, "r-first", RunKind::Review, 7, INTERRUPTED).await;
        assert_eq!(
            ids(&interrupted_reviews(&app, an_hour_ago()).await),
            ["r-first"]
        );

        run(&app, "r-resumed", RunKind::Review, 7, None).await;
        assert!(interrupted_reviews(&app, an_hour_ago()).await.is_empty());

        app.store
            .finish_run(
                &RunId::parse("r-resumed").unwrap(),
                RunStatus::Failed,
                None,
                Some("interrupted"),
            )
            .await
            .unwrap();
        assert_eq!(
            ids(&interrupted_reviews(&app, an_hour_ago()).await),
            ["r-resumed"]
        );
    }

    /// #160: a resumed review that was reaped is not resumed again, so a
    /// review that takes the process down costs one more crash, not one
    /// on every start. One a shutdown stopped still is.
    #[tokio::test]
    async fn a_resumed_review_that_was_reaped_is_not_resumed_again() {
        let app = app().await;
        let reaped = Some((RunStatus::Failed, Some(REAPED)));
        let resumed = ReviewTrigger::Resumed(RunId::parse("r-before").unwrap()).words();
        run(&app, "r-crashed", RunKind::Review, 1, reaped).await;
        run_for(&app, "r-again", RunKind::Review, 2, &resumed, reaped).await;
        run_for(&app, "r-stopped", RunKind::Review, 3, &resumed, INTERRUPTED).await;

        let found = interrupted_reviews(&app, an_hour_ago()).await;
        assert_eq!(ids(&found), ["r-crashed", "r-stopped"]);
    }

    /// #160: the resumer asks after the first pass and once more when the
    /// staleness window has passed, never on every pass, and never twice
    /// for the same run.
    #[tokio::test]
    async fn resumes_after_the_first_pass_and_once_more_never_on_every_pass() {
        let app = app().await;
        let (bus, heard) = bus();
        run(&app, "r-a", RunKind::Review, 1, INTERRUPTED).await;
        let mut resumer = Resumer::new(bus, Duration::from_millis(100));

        assert_eq!(
            resumer.after_pass(&app, Instant::now()).await,
            1,
            "after the first pass"
        );
        assert_eq!(
            resumer.after_pass(&app, Instant::now()).await,
            0,
            "not on every pass"
        );
        run(&app, "r-b", RunKind::Review, 2, INTERRUPTED).await;
        assert_eq!(
            resumer.after_pass(&app, Instant::now()).await,
            0,
            "not before the window"
        );

        tokio::time::sleep(Duration::from_millis(110)).await;
        assert_eq!(
            resumer.after_pass(&app, Instant::now()).await,
            1,
            "once more, for what a later pass reaped; r-a was asked for already"
        );
        run(&app, "r-c", RunKind::Review, 3, INTERRUPTED).await;
        assert_eq!(
            resumer.after_pass(&app, Instant::now()).await,
            0,
            "and never again"
        );

        until("both resumes are delivered", || {
            heard.0.lock().unwrap().len() == 2
        })
        .await;
        let mut seen = heard.0.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["r-a", "r-b"]);
    }

    /// #160, #249: a review someone cancelled, from the dashboard or over
    /// MCP, ends `cancelled`, not interrupted, and is not resumed.
    #[tokio::test]
    async fn a_cancelled_review_is_not_resumed() {
        let app = app().await;
        let by_mcp = crate::review::CancelledBy("mcp:claude".to_owned()).to_string();
        let by_person = crate::review::CancelledBy("github:1234".to_owned()).to_string();
        run(
            &app,
            "r-mcp",
            RunKind::Review,
            1,
            Some((RunStatus::Cancelled, Some(by_mcp.as_str()))),
        )
        .await;
        run(
            &app,
            "r-dashboard",
            RunKind::Review,
            2,
            Some((RunStatus::Cancelled, Some(by_person.as_str()))),
        )
        .await;
        run(&app, "r-stopped", RunKind::Review, 3, INTERRUPTED).await;

        let found = interrupted_reviews(&app, an_hour_ago()).await;
        assert_eq!(ids(&found), ["r-stopped"]);
    }

    /// #160: once Henk is told to stop, the resumer asks for nothing,
    /// though a pass is due and a review ended interrupted: those are this
    /// process's own reviews ending in its shutdown grace.
    #[tokio::test]
    async fn nothing_is_resumed_once_henk_is_told_to_stop() {
        let app = app().await;
        let (bus, heard) = bus();
        run(&app, "r-own", RunKind::Review, 1, INTERRUPTED).await;
        let mut resumer = Resumer::new(bus, Duration::ZERO);

        app.shutdown.cancel();
        assert_eq!(resumer.after_pass(&app, Instant::now()).await, 0);
        assert_eq!(resumer.after_pass(&app, Instant::now()).await, 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(heard.0.lock().unwrap().is_empty());
    }

    /// #160, #47: a process that died just before this start left its run
    /// with a fresh heartbeat. A later pass of the serving reaper closes it,
    /// and it is resumed then, once.
    #[tokio::test]
    async fn a_run_reaped_after_a_quick_restart_is_resumed_once() {
        let app = app().await;
        run(&app, "r-crashed", RunKind::Review, 7, None).await;
        let (bus, resumed) = bus();
        let cancel = CancellationToken::new();
        let stale_after = Duration::from_millis(100);
        let reaper = tokio::spawn(crate::liveness::reap_every(
            Arc::clone(&app),
            cancel.clone(),
            Duration::from_millis(10),
            stale_after,
            Some(Resumer::new(bus, stale_after)),
        ));
        until("the crashed run is resumed", || {
            !resumed.0.lock().unwrap().is_empty()
        })
        .await;
        // Many more passes, and nothing more is asked for.
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        reaper.await.unwrap();

        assert_eq!(*resumed.0.lock().unwrap(), ["r-crashed"]);
        let record = app
            .store
            .run(&RunId::parse("r-crashed").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.error.as_deref(), Some(REAPED));
    }
}
