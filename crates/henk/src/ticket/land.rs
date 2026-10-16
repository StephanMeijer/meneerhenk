//! The land stage of a ticket run (#637): after the review of the pull
//! request passed, Henk keeps it landable until the platform would merge it.
//!
//! The stage looks at the pull request again and again. Each look reads
//! what the platform says (can it merge, what the checks say), hands that
//! to [`decide`], and does the one thing it answers: rebase when the base
//! moved and the branch must catch up, review the head a rebase left,
//! wait, or end. It runs the rebases and the reviews itself and waits for
//! them, so it knows how each ended; nothing is read back from the store.
//!
//! * **A person wins.** A head that is not one the stage knows (a person's
//!   push), a merge, a close or a draft ends the stage well and pushes
//!   nothing. The heads it knows are the one the execute run opened, the
//!   ones its rebases left, and the ones the review loop pushed, which the
//!   loop checked itself (they are in [`crate::app::App::own_pushes`]).
//! * **Bounded.** The stage has a time of its own, a limit on rebases, and
//!   never rebases a head twice while the platform catches up.
//! * **Never "ready" by guessing.** It is ready when the platform says the
//!   pull request can merge, the head's review passed and its checks are
//!   green.
//! * **Failing checks are fixed, a few times.** A check that ended in failure
//!   has the executor read it and fix the code (`fix_ci.rs`, #583); the
//!   stage reviews the fix like any head and waits for the new checks. A fix
//!   that pushes nothing, a failure the executor calls not about the code,
//!   checks that were cancelled or timed out, and the limit all stop the
//!   stage, with a comment saying which. A fix run that finds no check
//!   failing (the checks changed since the stage read them) tried nothing:
//!   it costs no try, and the stage looks again after a nap.
//!
//! The stage ends in "ready to merge"; merging stays a person's.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::anyhow;
use henk_domain::land::{Action, CiState, End, Facts, Ledger, Limits as Tries, MergeState, decide};
use henk_domain::queue::Decision;
use henk_domain::review::CommitSha;
use henk_platform::{CiJob, ReviewTarget};
use henk_store::{Stage, StageState};
use tokio::time::Instant;
use tracing::{info, warn};

use super::{Ending, Land, Reviewed, Step, Stepped, Ticket, from_error, reviewed};
use crate::execute::OpenedPullRequest;
use crate::fix_ci::FixOutcome;
use crate::review::ReviewRequest;
use crate::stages;

/// How long a head may have no checks before it counts as having none:
/// they appear a moment after a push.
const CI_GRACE: Duration = Duration::from_mins(2);

/// Reads of the platform that may fail in a row before the stage gives up.
const MAX_READ_FAILURES: u32 = 5;

/// Looks at an unchanged pull request before the stage looks less often.
const QUIET_LOOKS: u32 = 10;

/// How often it looks then.
const QUIET_POLL: Duration = Duration::from_mins(5);

/// What the stage knows as it goes.
struct Watch {
    ledger: Ledger,
    /// Heads that are Henk's own.
    known: HashSet<CommitSha>,
    /// A review passed since the last look: a head the review loop pushed
    /// is then taken as reviewed, since the loop checked it.
    after_pass: bool,
    /// The head the last look saw, and since when.
    head_since: Option<(CommitSha, Instant)>,
    /// Reads of the platform that failed in a row.
    failures: u32,
    /// What the last look waited for, and how many looks in a row.
    waited_for: Option<(String, u32)>,
}

impl Ticket<'_> {
    /// Keeps `pull` landable. `reviewed` is the commit whose review passed,
    /// when one did; how the stage ends.
    pub(super) async fn land(
        &self,
        land: Land,
        pull: &OpenedPullRequest,
        reviewed: Option<CommitSha>,
    ) -> Ending {
        let deadline = Instant::now() + land.timeout;
        let target = &pull.target;
        let tries = Tries {
            max_rebases: land.max_rebases,
            max_ci_fixes: land.max_ci_fixes,
        };
        let mut known = HashSet::from([pull.commit.clone()]);
        known.extend(reviewed.clone());
        let mut watch = Watch {
            after_pass: reviewed.is_some(),
            ledger: Ledger {
                reviewed,
                ..Ledger::default()
            },
            known,
            head_since: None,
            failures: 0,
            waited_for: None,
        };
        self.mark_land(
            StageState::Running,
            format!("keeping pull request #{} landable", target.number),
        )
        .await;
        loop {
            if let Some(ending) = self.halted_until(Step::Land, deadline) {
                return self.ended(Step::Land, ending).await;
            }
            let facts = match self.look(target, &mut watch).await {
                Ok(facts) => facts,
                Err(error) => {
                    watch.failures += 1;
                    warn!(%error, run = %self.run, failures = watch.failures, "could not look at the pull request");
                    if watch.failures >= MAX_READ_FAILURES {
                        let why = format!("the platform could not be read: {error:#}");
                        return self.stuck(why).await;
                    }
                    self.nap(land.poll, deadline).await;
                    continue;
                }
            };
            watch.failures = 0;
            let head = facts.standing.head.clone();
            let action = decide(&facts, &watch.ledger, &tries);
            // Counted after `decide`, so the look just made is one the
            // platform had to catch up in, not one it is judged by.
            if watch.ledger.rebased.is_some() {
                watch.ledger.rebase_looks += 1;
            }
            match action {
                Action::Ready => return self.ended(Step::Land, Ending::Ready).await,
                Action::End(End::HandedOver(why)) => {
                    return self.ended(Step::Land, Ending::HandedOver(why)).await;
                }
                Action::End(End::Stuck(why)) => return self.stuck(why).await,
                Action::FixCi(names) => {
                    watch.waited_for = None;
                    if let Some(ending) = self
                        .fix_ci(land, deadline, target, (&head, &names), &mut watch)
                        .await
                    {
                        return ending;
                    }
                }
                Action::Wait(why) => {
                    self.waiting(&mut watch, &why, &head).await;
                    let quiet = watch
                        .waited_for
                        .as_ref()
                        .is_some_and(|(_, looks)| *looks >= QUIET_LOOKS);
                    self.nap(
                        if quiet {
                            land.poll.max(QUIET_POLL)
                        } else {
                            land.poll
                        },
                        deadline,
                    )
                    .await;
                }
                Action::Rebase => {
                    watch.waited_for = None;
                    if let Some(ending) =
                        self.rebase(land, deadline, target, &head, &mut watch).await
                    {
                        return ending;
                    }
                }
                Action::Review => {
                    watch.waited_for = None;
                    if let Some(ending) = self
                        .review_head(land, deadline, target, &head, &mut watch)
                        .await
                    {
                        return ending;
                    }
                }
            }
        }
    }

    /// One look: the platform's standing, and the checks when they matter.
    async fn look(&self, target: &ReviewTarget, watch: &mut Watch) -> anyhow::Result<Facts> {
        let standing = self.stages.standing(target).await?;
        let head = standing.head.clone();
        if watch
            .head_since
            .as_ref()
            .is_none_or(|(seen, _)| *seen != head)
        {
            watch.head_since = Some((head.clone(), Instant::now()));
        }
        if watch.after_pass {
            watch.after_pass = false;
            if self.app.own_pushes.has(head.as_str())
                && watch.ledger.reviewed.as_ref() != Some(&head)
            {
                // The review loop pushed it, and checked it in its rounds.
                watch.known.insert(head.clone());
                watch.ledger.reviewed = Some(head.clone());
            }
        }
        // A head Henk pushed from another run, such as a fix asked for by a
        // person, is his own whoever started that run.
        if self.app.own_pushes.has(head.as_str()) {
            watch.known.insert(head.clone());
        }
        let needs_rebase = matches!(
            standing.state,
            MergeState::NeedsRebase | MergeState::Conflicts
        );
        // The checks only matter for a head that is open, reviewed and not
        // about to change; `decide` reads nothing else of them.
        let ci = if standing.open && !needs_rebase && watch.ledger.reviewed.as_ref() == Some(&head)
        {
            let age = watch
                .head_since
                .as_ref()
                .map_or(Duration::ZERO, |(_, since)| since.elapsed());
            ci_state(&self.stages.ci_jobs(target, &head).await?, age)
        } else {
            CiState::None
        };
        let executor = self.app.settings.roles.executor.as_ref();
        let rebases_allowed = executor.is_some_and(|executor| executor.rebases(&target.repo));
        let fixes_allowed = executor.is_some_and(|executor| executor.fixes_ci(&target.repo));
        Ok(Facts {
            head_is_ours: watch.known.contains(&head),
            standing,
            ci,
            rebases_allowed,
            fixes_allowed,
        })
    }

    /// Rebases the pull request and waits for it. `None` to look again, or
    /// the ending.
    async fn rebase(
        &self,
        land: Land,
        deadline: Instant,
        target: &ReviewTarget,
        head: &CommitSha,
        watch: &mut Watch,
    ) -> Option<Ending> {
        let (decision, run, ended) = self.stages.rebase(
            target.clone(),
            self.trigger(),
            self.request.requester.clone(),
        );
        if matches!(decision, Decision::Join | Decision::After) {
            // Another rebase of this pull request is under way: its result
            // shows in the next look.
            self.nap(land.poll, deadline).await;
            return None;
        }
        self.link_land_child(&run).await;
        self.mark_land(
            StageState::Running,
            format!("rebasing onto the base: run {run}"),
        )
        .await;
        let work = async move {
            ended
                .await
                .unwrap_or_else(|_| Err(anyhow!("the rebase ended without a report")))
        };
        match self.step_until(Step::Land, &run, deadline, work).await {
            Stepped::Stopped(ending) => Some(self.ended(Step::Land, ending).await),
            Stepped::Returned(Err(error)) => {
                Some(self.ended(Step::Land, from_error(Step::Land, &error)).await)
            }
            Stepped::Returned(Ok(report)) => {
                watch.ledger.rebases += 1;
                let left = report.pushed.clone().unwrap_or_else(|| head.clone());
                self.note(&match &report.pushed {
                    Some(pushed) => format!(
                        "land: rebased the pull request, run {run}: head {} is now {}",
                        head.short(),
                        pushed.short()
                    ),
                    None => format!(
                        "land: rebase run {run} found the pull request up to date at {}",
                        head.short()
                    ),
                })
                .await;
                watch.known.insert(left.clone());
                watch.ledger.rebased = Some(left);
                watch.ledger.rebase_looks = 0;
                None
            }
        }
    }

    /// Has the executor fix the failing checks `failing` of `head` and waits
    /// for it. `None` to look again, or the ending.
    ///
    /// The limit is on real tries: a fix that pushed counts. A fix that
    /// found no fix or no fault in the code ends the stage. A fix that
    /// pushed has its head reviewed on the next look, and its checks
    /// read after that. One that pushed nothing ends the stage, since the
    /// same head would only be tried again. The exception is a run that found
    /// nothing failing (the checks changed since the stage read them): it
    /// counts for nothing, and the stage looks again after a nap, until its
    /// own time is up.
    async fn fix_ci(
        &self,
        land: Land,
        deadline: Instant,
        target: &ReviewTarget,
        (head, failing): (&CommitSha, &[String]),
        watch: &mut Watch,
    ) -> Option<Ending> {
        let (decision, run, ended) = self.stages.fix_ci(
            target.clone(),
            self.trigger(),
            self.request.requester.clone(),
        );
        if matches!(decision, Decision::Join | Decision::After) {
            // Another run is changing this pull request's head: a rebase, or
            // a fix somebody asked for. Its result shows in the next look.
            self.nap(land.poll, deadline).await;
            return None;
        }
        self.link_land_child(&run).await;
        self.mark_land(
            StageState::Running,
            format!("fixing failing checks ({}): run {run}", failing.join(", ")),
        )
        .await;
        let work = async move {
            ended
                .await
                .unwrap_or_else(|_| Err(anyhow!("the fix ended without a report")))
        };
        match self.step_until(Step::Land, &run, deadline, work).await {
            Stepped::Stopped(ending) => Some(self.ended(Step::Land, ending).await),
            Stepped::Returned(Err(error)) => {
                Some(self.ended(Step::Land, from_error(Step::Land, &error)).await)
            }
            Stepped::Returned(Ok(report)) => {
                let named = failing.join(", ");
                match report.outcome {
                    FixOutcome::Pushed {
                        head: fixed,
                        commits,
                        ..
                    } => {
                        watch.ledger.ci_fixes += 1;
                        self.note(&format!(
                            "land: fixed the failing checks ({named}), run {run}: {commits} commit(s), head {} is now {}",
                            head.short(),
                            fixed.short()
                        ))
                        .await;
                        watch.known.insert(fixed);
                        None
                    }
                    FixOutcome::Idle(why) => {
                        self.note(&format!(
                            "land: fix run {run} found nothing to fix on {}: {why}",
                            head.short()
                        ))
                        .await;
                        // The fix run read the checks differently from the
                        // stage (a re-run, a lagging platform): nothing was
                        // tried, so no try is spent. Look again after a nap.
                        self.nap(land.poll, deadline).await;
                        None
                    }
                    FixOutcome::NotCode { reason, .. } => Some(
                        self.stuck(format!(
                            "checks failed: {named}; they do not look like a problem in the code: {reason}"
                        ))
                        .await,
                    ),
                    FixOutcome::NoFix { why, .. } => Some(
                        self.stuck(format!("checks failed: {named}; I could not fix them: {why}"))
                            .await,
                    ),
                }
            }
        }
    }

    /// Reviews `head` and waits for it. `None` to look again, or the
    /// ending.
    async fn review_head(
        &self,
        land: Land,
        deadline: Instant,
        target: &ReviewTarget,
        head: &CommitSha,
        watch: &mut Watch,
    ) -> Option<Ending> {
        let request = ReviewRequest {
            target: target.clone(),
            commit: Some(head.clone()),
            trigger: self.trigger(),
            requester: self.request.requester.clone(),
            acknowledge: None,
            run: None,
            submitted_at: None,
            queued_check: None,
        };
        let (decision, run, ended) = self.stages.review(request, head.clone());
        if matches!(decision, Decision::Join | Decision::After) {
            // A review of this pull request is under way or waits for a
            // rebase: another run's. Look again once it has had its time.
            self.nap(land.poll, deadline).await;
            return None;
        }
        self.link_land_child(&run).await;
        self.mark_land(
            StageState::Running,
            format!("reviewing {}: run {run}", head.short()),
        )
        .await;
        let work = async move {
            ended
                .await
                .unwrap_or_else(|_| Err(anyhow!("the review ended without a report")))
        };
        match self.step_until(Step::Land, &run, deadline, work).await {
            Stepped::Stopped(ending) => Some(self.ended(Step::Land, ending).await),
            Stepped::Returned(result) => match reviewed(result) {
                Reviewed::Passed(commit) => {
                    self.note(&format!(
                        "land: the review of {} passed, run {run}",
                        commit.short()
                    ))
                    .await;
                    watch.known.insert(commit.clone());
                    watch.ledger.reviewed = Some(commit);
                    watch.after_pass = true;
                    None
                }
                // A newer commit or a rebase replaced it: the next look
                // sees what the head is now.
                Reviewed::Replaced(_) => None,
                Reviewed::Ended(ending) => Some(self.ended(Step::Land, ending).await),
            },
        }
    }

    /// Records what the stage waits for, when it changed, and counts the
    /// looks that waited for the same thing.
    async fn waiting(&self, watch: &mut Watch, why: &str, head: &CommitSha) {
        let same = watch
            .waited_for
            .as_ref()
            .is_some_and(|(before, _)| before == why);
        if same {
            if let Some((_, looks)) = &mut watch.waited_for {
                *looks += 1;
            }
            return;
        }
        watch.waited_for = Some((why.to_owned(), 1));
        self.mark_land(
            StageState::Running,
            format!("{}: waiting: {why}", head.short()),
        )
        .await;
    }

    /// Ends the stage as one that cannot go on.
    async fn stuck(&self, why: String) -> Ending {
        self.ended(
            Step::Land,
            Ending::Stopped {
                step: Step::Land,
                why,
            },
        )
        .await
    }

    /// Sleeps `how_long`, or until the stage has to stop.
    async fn nap(&self, how_long: Duration, deadline: Instant) {
        let until = (Instant::now() + how_long).min(deadline);
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => {}
            () = tokio::time::sleep_until(until) => {}
        }
    }

    /// Adds a line to the ticket run's timeline: the stage names only the
    /// latest run it started, so this is where each rebase and review is
    /// kept.
    async fn note(&self, text: &str) {
        info!(run = %self.run, "{text}");
        if let Err(error) = self.app.store.event(&self.run, "info", text).await {
            warn!(%error, run = %self.run, "could not record the event");
        }
    }

    /// Records where the stage stands.
    async fn mark_land(&self, state: StageState, detail: String) {
        stages::mark(&*self.app.store, &self.run, Stage::Land, state, detail).await;
    }

    /// `child` is a run of the land stage: linked to it.
    async fn link_land_child(&self, child: &henk_domain::run::RunId) {
        if let Err(error) = self
            .app
            .store
            .link_child(&self.run, Stage::Land, child)
            .await
        {
            warn!(%error, run = %self.run, child = %child, "could not link the run to the land stage");
        }
    }
}

/// What the checks on a head say. A head just pushed has none for a moment,
/// so no jobs count as pending until `age` passes [`CI_GRACE`]. A failed job
/// is red, and is sent to the fixer. A cancelled or timed out job is
/// interrupted, not red: it counts only once nothing is still running, since
/// a newer run of the same workflow may follow it.
fn ci_state(jobs: &[CiJob], age: Duration) -> CiState {
    if jobs.is_empty() {
        return if age < CI_GRACE {
            CiState::Pending
        } else {
            CiState::None
        };
    }
    let names = |wanted: fn(&CiJob) -> bool| -> Vec<String> {
        jobs.iter()
            .filter(|job| wanted(job))
            .map(|job| job.name.clone())
            .collect()
    };
    let failed = names(CiJob::failed);
    if !failed.is_empty() {
        return CiState::Red(failed);
    }
    if jobs.iter().any(|job| !job.finished()) {
        return CiState::Pending;
    }
    // Only now: a check cancelled by a newer run of the same workflow is
    // followed by that run's checks, which are pending until they end.
    let interrupted = names(CiJob::interrupted);
    if !interrupted.is_empty() {
        return CiState::Interrupted(interrupted);
    }
    CiState::Green
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::too_many_lines
    )]

    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use henk_domain::land::{REBASE_LAG_LOOKS, Standing};
    use henk_domain::review::ReviewOutcome;
    use henk_domain::run::RunId;
    use henk_platform::{CiConclusion, CiJobId, CiStatus};
    use henk_store::RunStatus;
    use tokio::sync::oneshot;

    use super::super::tests::{
        CONFIG, TICKET, comments, converged, fixture, fixture_on, opened, record, request, sha,
        stage_states,
    };
    use super::super::{Limits, Stages, TicketReport, run_ticket};
    use super::*;
    use crate::execute::{ExecuteReport, ExecuteRequest};
    use crate::fix_ci::FixCiReport;
    use crate::issue_runs::RunFailed;
    use crate::plan::{PlanReport, PlanRequest};
    use crate::rebase::RebaseReport;
    use crate::review::{CancelledBy, ReviewReport, Superseded};

    fn job(name: &str, status: CiStatus, conclusion: Option<CiConclusion>) -> CiJob {
        CiJob {
            id: CiJobId(name.to_owned()),
            name: name.to_owned(),
            workflow: None,
            status,
            conclusion,
            started_at: None,
            completed_at: None,
            url: None,
            has_log: false,
        }
    }

    #[test]
    fn checks_come_to_one_state() {
        use CiConclusion::{
            AllowedFailure, Cancelled, Failure, Neutral, Skipped, Success, TimedOut,
        };
        let done = |name, conclusion| job(name, CiStatus::Completed, Some(conclusion));
        let old = CI_GRACE * 2;

        assert_eq!(
            ci_state(
                &[done("a", Success), done("b", Skipped), done("c", Neutral)],
                old
            ),
            CiState::Green
        );
        assert_eq!(
            ci_state(&[done("a", Success), done("b", AllowedFailure)], old),
            CiState::Green,
            "a failure the pipeline allows is not red"
        );
        assert_eq!(
            ci_state(&[done("ok", Success), done("x", Failure)], old),
            CiState::Red(vec!["x".to_owned()])
        );
        // Not the code's fault, so not red: a person looks.
        assert_eq!(
            ci_state(
                &[
                    done("ok", Success),
                    done("y", Cancelled),
                    done("z", TimedOut)
                ],
                old
            ),
            CiState::Interrupted(vec!["y".to_owned(), "z".to_owned()])
        );
        // A failure is fixed even beside a cancelled check, and a cancelled
        // check beside a running one is not given up on: the newer run that
        // cancelled it is still going.
        assert_eq!(
            ci_state(&[done("y", Cancelled), done("x", Failure)], old),
            CiState::Red(vec!["x".to_owned()])
        );
        assert_eq!(
            ci_state(
                &[done("y", Cancelled), job("b", CiStatus::Queued, None)],
                old
            ),
            CiState::Pending
        );
        assert_eq!(
            ci_state(
                &[done("a", Success), job("b", CiStatus::InProgress, None)],
                old
            ),
            CiState::Pending
        );
        assert_eq!(
            ci_state(&[job("b", CiStatus::Queued, None)], old),
            CiState::Pending
        );
        // Red wins over pending: no reason to wait to say it failed.
        assert_eq!(
            ci_state(
                &[done("a", Failure), job("b", CiStatus::InProgress, None)],
                old
            ),
            CiState::Red(vec!["a".to_owned()])
        );
    }

    #[test]
    fn a_head_with_no_checks_waits_for_them_to_appear_and_then_has_none() {
        assert_eq!(ci_state(&[], Duration::ZERO), CiState::Pending);
        assert_eq!(ci_state(&[], CI_GRACE), CiState::None);
    }

    /// The head after `n` rebases: the head the execute run opened, then
    /// others.
    fn head(n: usize) -> CommitSha {
        if n == 0 {
            sha()
        } else {
            CommitSha::parse(&format!("{n:040x}")).unwrap()
        }
    }

    /// How a scripted review ends.
    enum Review {
        /// Passes at the commit it was asked about.
        Passes,
        /// Finds a problem that stays open.
        Finds,
        /// A newer commit replaced it.
        Replaced,
    }

    /// How a scripted fix of failing checks ends.
    enum Fix {
        /// Pushes a commit: the head moves.
        Pushes,
        /// Ends with this outcome and pushes nothing.
        Ends(FixOutcome),
        /// Joins a run that is under way: nobody here waits for it.
        Joins,
        /// Fails.
        Fails(&'static str),
    }

    /// A pull request whose head moves with each rebase and each fix, and
    /// the runs the land stage asks for.
    struct Landing {
        /// How many rebases made the head.
        rebased: Mutex<usize>,
        /// What the platform says of the head after `n` rebases.
        state: Box<dyn Fn(usize) -> MergeState + Send + Sync>,
        /// Reads of the platform that fail before one works.
        unreadable: Mutex<u32>,
        reads: Mutex<u32>,
        jobs: Mutex<Vec<CiJob>>,
        reviews: Mutex<VecDeque<Review>>,
        rebases: Mutex<VecDeque<anyhow::Result<()>>>,
        /// How the fixes of failing checks end, in order.
        fixes: Mutex<VecDeque<Fix>>,
        /// The checks on the head each pushed fix leaves, in order; green
        /// when there are none left.
        checks_after_fix: Mutex<VecDeque<Vec<CiJob>>>,
        /// Somebody else pushes a commit at this read of the platform: the
        /// head moves and its checks are green.
        pushed_at_read: Mutex<Option<u32>>,
        calls: Mutex<Vec<String>>,
        reviewed: Mutex<Vec<CommitSha>>,
        merged: Mutex<bool>,
        /// The rebase never ends until it is stopped.
        hang_rebase: Mutex<bool>,
        held: Mutex<Option<oneshot::Sender<anyhow::Result<RebaseReport>>>>,
        stopped: Mutex<Vec<String>>,
    }

    impl Landing {
        fn new(state: impl Fn(usize) -> MergeState + Send + Sync + 'static) -> Self {
            Self {
                rebased: Mutex::new(0),
                state: Box::new(state),
                unreadable: Mutex::new(0),
                reads: Mutex::new(0),
                jobs: Mutex::new(vec![green()]),
                reviews: Mutex::new(VecDeque::new()),
                rebases: Mutex::new(VecDeque::new()),
                fixes: Mutex::new(VecDeque::new()),
                checks_after_fix: Mutex::new(VecDeque::new()),
                pushed_at_read: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                reviewed: Mutex::new(Vec::new()),
                merged: Mutex::new(false),
                hang_rebase: Mutex::new(false),
                held: Mutex::new(None),
                stopped: Mutex::new(Vec::new()),
            }
        }

        fn mergeable() -> Self {
            Self::new(|_| MergeState::Mergeable)
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn reviewed(&self) -> Vec<CommitSha> {
            self.reviewed.lock().unwrap().clone()
        }

        fn with_fix(self, fix: Fix) -> Self {
            self.fixes.lock().unwrap().push_back(fix);
            self
        }

        fn fixes_asked(&self) -> usize {
            self.calls().iter().filter(|c| *c == "fix_ci").count()
        }

        fn with_review(self, review: Review) -> Self {
            self.reviews.lock().unwrap().push_back(review);
            self
        }
    }

    fn green() -> CiJob {
        job("test", CiStatus::Completed, Some(CiConclusion::Success))
    }

    fn red(name: &str) -> CiJob {
        job(name, CiStatus::Completed, Some(CiConclusion::Failure))
    }

    #[async_trait::async_trait]
    impl Stages for Landing {
        async fn plan(&self, _: PlanRequest) -> anyhow::Result<PlanReport> {
            self.calls.lock().unwrap().push("plan".to_owned());
            Ok(converged())
        }

        async fn execute(&self, _: ExecuteRequest) -> anyhow::Result<ExecuteReport> {
            self.calls.lock().unwrap().push("execute".to_owned());
            Ok(opened())
        }

        fn review(
            &self,
            request: ReviewRequest,
            commit: CommitSha,
        ) -> (
            Decision,
            RunId,
            oneshot::Receiver<anyhow::Result<ReviewReport>>,
        ) {
            let mut calls = self.calls.lock().unwrap();
            calls.push("review".to_owned());
            let run = RunId::parse(format!("r-review-{}", calls.len())).unwrap();
            assert_eq!(request.commit.as_ref(), Some(&commit));
            self.reviewed.lock().unwrap().push(commit.clone());
            let result = match self
                .reviews
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Review::Passes)
            {
                Review::Replaced => Err(Superseded.into()),
                other => Ok(ReviewReport {
                    run: run.clone(),
                    outcome: Some(ReviewOutcome {
                        commit,
                        sessions: Vec::new(),
                        open_findings: usize::from(matches!(other, Review::Finds)),
                        nothing_to_review: true,
                        stopped: None,
                        not_reviewed: None,
                        findings_left: false,
                        report: None,
                    }),
                    summary: None,
                }),
            };
            let (tell, ended) = oneshot::channel();
            let _ = tell.send(result);
            (Decision::Start, run, ended)
        }

        fn rebase(
            &self,
            _: ReviewTarget,
            trigger: String,
            _: Option<String>,
        ) -> (
            Decision,
            RunId,
            oneshot::Receiver<anyhow::Result<RebaseReport>>,
        ) {
            let mut calls = self.calls.lock().unwrap();
            calls.push("rebase".to_owned());
            assert!(trigger.contains(TICKET), "{trigger}");
            let run = RunId::parse(format!("r-rebase-{}", calls.len())).unwrap();
            if *self.hang_rebase.lock().unwrap() {
                let (tell, ended) = oneshot::channel();
                *self.held.lock().unwrap() = Some(tell);
                return (Decision::Start, run, ended);
            }
            let result = if let Some(Err(error)) = self.rebases.lock().unwrap().pop_front() {
                Err(error)
            } else {
                let mut rebased = self.rebased.lock().unwrap();
                *rebased += 1;
                Ok(RebaseReport {
                    run: run.clone(),
                    target: opened().pull.target,
                    pushed: Some(head(*rebased)),
                })
            };
            let (tell, ended) = oneshot::channel();
            let _ = tell.send(result);
            (Decision::Start, run, ended)
        }

        fn fix_ci(
            &self,
            _: ReviewTarget,
            trigger: String,
            _: Option<String>,
        ) -> (
            Decision,
            RunId,
            oneshot::Receiver<anyhow::Result<FixCiReport>>,
        ) {
            let mut calls = self.calls.lock().unwrap();
            calls.push("fix_ci".to_owned());
            assert!(trigger.contains(TICKET), "{trigger}");
            let run = RunId::parse(format!("r-fix-{}", calls.len())).unwrap();
            let (tell, ended) = oneshot::channel();
            let fix = self
                .fixes
                .lock()
                .unwrap()
                .pop_front()
                .expect("a fix is scripted");
            let report = |outcome| FixCiReport {
                run: run.clone(),
                target: opened().pull.target,
                outcome,
            };
            let checks = vec!["clippy".to_owned()];
            match fix {
                Fix::Joins => return (Decision::Join, run, ended),
                Fix::Fails(why) => {
                    let _ = tell.send(Err(anyhow!(why)));
                }
                Fix::Ends(outcome) => {
                    let _ = tell.send(Ok(report(outcome)));
                }
                Fix::Pushes => {
                    let mut moved = self.rebased.lock().unwrap();
                    *moved += 1;
                    let after = self
                        .checks_after_fix
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| vec![green()]);
                    *self.jobs.lock().unwrap() = after;
                    let _ = tell.send(Ok(report(FixOutcome::Pushed {
                        checks,
                        summary: "It returns 3 now.".to_owned(),
                        head: head(*moved),
                        commits: 1,
                    })));
                }
            }
            (Decision::Start, run, ended)
        }

        async fn standing(&self, _: &ReviewTarget) -> anyhow::Result<Standing> {
            let read = {
                let mut reads = self.reads.lock().unwrap();
                *reads += 1;
                *reads
            };
            if *self.pushed_at_read.lock().unwrap() == Some(read) {
                *self.rebased.lock().unwrap() = 1;
                *self.jobs.lock().unwrap() = vec![green()];
            }
            {
                let mut unreadable = self.unreadable.lock().unwrap();
                if *unreadable > 0 {
                    *unreadable -= 1;
                    return Err(anyhow!("GitHub is down"));
                }
            }
            let rebased = *self.rebased.lock().unwrap();
            let merged = *self.merged.lock().unwrap();
            Ok(Standing {
                open: !merged,
                merged,
                head: head(rebased),
                state: (self.state)(rebased),
            })
        }

        async fn ci_jobs(&self, _: &ReviewTarget, _: &CommitSha) -> anyhow::Result<Vec<CiJob>> {
            Ok(self.jobs.lock().unwrap().clone())
        }

        fn stop(&self, run: &RunId, by: String) -> bool {
            self.stopped.lock().unwrap().push(format!("{run} by {by}"));
            if let Some(tell) = self.held.lock().unwrap().take() {
                let _ = tell.send(Err(CancelledBy(by).into()));
            }
            true
        }
    }

    fn with_land(land: Land) -> Limits {
        Limits {
            run: Duration::from_mins(1),
            grace: Duration::from_millis(100),
            land: Some(land),
        }
    }

    fn quick() -> Land {
        Land {
            timeout: Duration::from_secs(30),
            poll: Duration::from_millis(5),
            max_rebases: 3,
            max_ci_fixes: 3,
        }
    }

    async fn go(f: &super::super::tests::Fixture, stages: &Landing, land: Land) -> TicketReport {
        let cancel = f.app.shutdown.child_token();
        let _registered = f
            .app
            .cancels
            .register(RunId::parse(TICKET).unwrap(), cancel.clone());
        run_ticket(&f.app, stages, request(), cancel, with_land(land))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_pull_request_that_can_merge_is_ready_without_anything_done_to_it() {
        let f = fixture();
        let stages = Landing::mergeable();
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready);
        assert!(report.ending.is_well());
        assert_eq!(stages.calls(), ["plan", "execute", "review"]);
        assert!(comments(&f).is_empty(), "{:?}", comments(&f));
        let record = record(&f).await;
        assert_eq!(record.status, RunStatus::Finished);
        assert_eq!(
            record.summary.as_deref(),
            Some("pull request #12 is ready to merge: https://github.com/o/r/pull/12")
        );
        let rows = stage_states(&f).await;
        assert!(
            rows.contains(&(Stage::Review, StageState::Done)),
            "{rows:?}"
        );
        assert!(rows.contains(&(Stage::Land, StageState::Done)), "{rows:?}");
    }

    #[tokio::test]
    async fn a_branch_that_must_catch_up_is_rebased_reviewed_again_and_then_ready() {
        let f = fixture();
        let stages = Landing::new(|rebased| {
            if rebased == 0 {
                MergeState::NeedsRebase
            } else {
                MergeState::Mergeable
            }
        });
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready, "{:?}", report.ending);
        assert_eq!(
            stages.calls(),
            ["plan", "execute", "review", "rebase", "review"]
        );
        assert_eq!(
            stages.reviewed(),
            [head(0), head(1)],
            "the rebased head is reviewed, not the old one"
        );
        // The stage names the latest run it started; the timeline keeps
        // each.
        let children = f
            .app
            .store
            .children(&RunId::parse(TICKET).unwrap())
            .await
            .unwrap();
        let land: Vec<String> = children
            .iter()
            .filter(|(stage, _)| *stage == Stage::Land)
            .map(|(_, run)| run.to_string())
            .collect();
        assert_eq!(land.len(), 1, "{children:?}");
        assert!(land[0].starts_with("r-review-"), "{land:?}");
        let events: Vec<String> = f
            .app
            .store
            .events(&RunId::parse(TICKET).unwrap())
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.message)
            .filter(|message| message.starts_with("land: "))
            .collect();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events[0].starts_with("land: rebased the pull request, run r-rebase-"),
            "{events:?}"
        );
        assert!(events[1].contains("the review of 0000"), "{events:?}");
        assert!(comments(&f).is_empty());
    }

    #[tokio::test]
    async fn a_conflict_is_rebased_like_a_branch_that_must_catch_up() {
        let f = fixture();
        let stages = Landing::new(|rebased| {
            if rebased == 0 {
                MergeState::Conflicts
            } else {
                MergeState::Mergeable
            }
        });
        let report = go(&f, &stages, quick()).await;
        assert_eq!(report.ending, Ending::Ready);
        assert!(stages.calls().contains(&"rebase".to_owned()));
    }

    #[tokio::test]
    async fn a_push_by_somebody_else_ends_the_stage_well_with_nothing_done() {
        let f = fixture();
        // A head the stage does not know, and Henk did not push.
        let stages = Landing::new(|_| MergeState::NeedsRebase);
        *stages.rebased.lock().unwrap() = 7;
        let report = go(&f, &stages, quick()).await;

        assert_eq!(
            report.ending,
            Ending::HandedOver("somebody else pushed to its branch".to_owned())
        );
        assert_eq!(stages.calls(), ["plan", "execute", "review"], "no rebase");
        assert!(comments(&f).is_empty());
        assert_eq!(record(&f).await.status, RunStatus::Finished);
    }

    #[tokio::test]
    async fn a_head_the_review_loop_pushed_counts_as_reviewed_and_is_not_reviewed_again() {
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.rebased.lock().unwrap() = 1;
        // The loop pushed it in the run that reviewed the pull request.
        f.app.own_pushes.record(head(1).as_str());
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready);
        assert_eq!(stages.calls(), ["plan", "execute", "review"], "one review");
    }

    #[tokio::test]
    async fn a_merge_or_a_close_by_a_person_ends_the_stage_well() {
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.merged.lock().unwrap() = true;
        let report = go(&f, &stages, quick()).await;
        assert_eq!(
            report.ending,
            Ending::HandedOver("it was merged".to_owned())
        );
        assert!(comments(&f).is_empty());
    }

    async fn land_events(f: &super::super::tests::Fixture) -> Vec<String> {
        f.app
            .store
            .events(&RunId::parse(TICKET).unwrap())
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.message)
            .filter(|message| message.starts_with("land: "))
            .collect()
    }

    /// #583: failing checks have the executor fix the code; the fixed head
    /// is reviewed like any other and its checks decide.
    #[tokio::test]
    async fn failing_checks_are_fixed_the_fix_reviewed_and_the_stage_ends_ready() {
        let f = fixture();
        let stages = Landing::mergeable().with_fix(Fix::Pushes);
        *stages.jobs.lock().unwrap() = vec![green(), red("clippy")];
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready, "{:?}", report.ending);
        assert_eq!(
            stages.calls(),
            ["plan", "execute", "review", "fix_ci", "review"]
        );
        assert_eq!(
            stages.reviewed(),
            [head(0), head(1)],
            "the fixed head is reviewed, not the old one"
        );
        let events = land_events(&f).await;
        assert!(
            events[0].starts_with("land: fixed the failing checks (clippy), run r-fix-"),
            "{events:?}"
        );
        assert!(events[0].contains("1 commit(s)"), "{events:?}");
        assert!(comments(&f).is_empty(), "{:?}", comments(&f));
        assert_eq!(record(&f).await.status, RunStatus::Finished);
    }

    /// A fix that pushes nothing, or a failure the executor says is not the
    /// code's, ends the stage at once, since the same head would only be
    /// tried again; the comment says why.
    #[tokio::test]
    async fn a_fix_that_pushes_nothing_stops_the_stage_with_the_reason() {
        let checks = vec!["clippy".to_owned()];
        for (outcome, says) in [
            (
                FixOutcome::NoFix {
                    checks: checks.clone(),
                    why: "the executor ended without proposing a fix".to_owned(),
                },
                "checks failed: clippy; I could not fix them: the executor ended without proposing a fix",
            ),
            (
                FixOutcome::NotCode {
                    checks: checks.clone(),
                    reason: "the runner was lost.".to_owned(),
                },
                "checks failed: clippy; they do not look like a problem in the code: the runner was lost.",
            ),
        ] {
            let f = fixture();
            let stages = Landing::mergeable().with_fix(Fix::Ends(outcome));
            *stages.jobs.lock().unwrap() = vec![green(), red("clippy")];
            let report = go(&f, &stages, quick()).await;

            assert!(
                matches!(&report.ending, Ending::Stopped { step: Step::Land, why } if why.contains(says)),
                "{:?}",
                report.ending
            );
            assert_eq!(stages.fixes_asked(), 1);
            assert_eq!(stages.reviewed(), [head(0)], "nothing new to review");
            let posted = comments(&f);
            assert_eq!(posted.len(), 1, "{posted:?}");
            assert!(posted[0].contains(says), "{}", posted[0]);
            assert_eq!(record(&f).await.status, RunStatus::Failed);
        }
    }

    #[tokio::test]
    async fn fixes_stop_at_the_limit_and_the_comment_counts_them() {
        let f = fixture();
        // Every fix pushes, and the checks stay red.
        let stages = Landing::mergeable()
            .with_fix(Fix::Pushes)
            .with_fix(Fix::Pushes);
        *stages.jobs.lock().unwrap() = vec![red("clippy")];
        stages
            .checks_after_fix
            .lock()
            .unwrap()
            .extend([vec![red("clippy")], vec![red("clippy")]]);
        let two = Land {
            max_ci_fixes: 2,
            ..quick()
        };
        let report = go(&f, &stages, two).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "checks failed after 2 fixes: clippy".to_owned()
            }
        );
        assert_eq!(
            stages.calls(),
            [
                "plan", "execute", "review", "fix_ci", "review", "fix_ci", "review"
            ]
        );
        assert_eq!(stages.reviewed(), [head(0), head(1), head(2)]);
        assert_eq!(comments(&f).len(), 1);
    }

    #[tokio::test]
    async fn cancelled_or_timed_out_checks_stop_the_stage_without_a_fix() {
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.jobs.lock().unwrap() = vec![
            green(),
            job("test", CiStatus::Completed, Some(CiConclusion::Cancelled)),
        ];
        let report = go(&f, &stages, quick()).await;

        assert!(
            matches!(&report.ending, Ending::Stopped { step: Step::Land, why } if why.starts_with("checks did not finish: test;")),
            "{:?}",
            report.ending
        );
        assert_eq!(stages.fixes_asked(), 0);
    }

    #[tokio::test]
    async fn a_repository_that_turned_fixing_off_is_not_fixed() {
        let off = format!("{CONFIG}[roles.executor.repositories.\"o/r\"]\nfix_ci = false\n");
        let f = fixture_on(&off);
        let stages = Landing::mergeable();
        *stages.jobs.lock().unwrap() = vec![red("clippy")];
        let report = go(&f, &stages, quick()).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "checks failed: clippy, and fixing checks is off for this repository"
                    .to_owned()
            }
        );
        assert_eq!(stages.fixes_asked(), 0);
    }

    /// A fix asked for by a person is under way, or a rebase: the stage does
    /// not wait for it, looks again, and the try does not count.
    #[tokio::test]
    async fn a_fix_that_joins_another_run_is_looked_at_again_and_costs_no_try() {
        let f = fixture();
        let stages = Landing::mergeable()
            .with_fix(Fix::Joins)
            .with_fix(Fix::Joins)
            .with_fix(Fix::Pushes);
        *stages.jobs.lock().unwrap() = vec![red("clippy")];
        let one = Land {
            max_ci_fixes: 1,
            ..quick()
        };
        let report = go(&f, &stages, one).await;

        assert_eq!(report.ending, Ending::Ready, "{:?}", report.ending);
        assert_eq!(stages.fixes_asked(), 3);
    }

    /// A fix run that finds nothing failing disagrees with the stage's read:
    /// it fixed nothing, so it costs no try, and the stage looks again.
    #[tokio::test]
    async fn a_fix_that_finds_nothing_failing_costs_no_try_and_the_stage_looks_again() {
        let f = fixture();
        let idle = || Fix::Ends(FixOutcome::Idle("no check fails".to_owned()));
        let stages = Landing::mergeable()
            .with_fix(idle())
            .with_fix(idle())
            .with_fix(Fix::Pushes);
        *stages.jobs.lock().unwrap() = vec![red("clippy")];
        let one = Land {
            max_ci_fixes: 1,
            ..quick()
        };
        let report = go(&f, &stages, one).await;

        assert_eq!(report.ending, Ending::Ready, "{:?}", report.ending);
        assert_eq!(stages.fixes_asked(), 3);
        let events = land_events(&f).await;
        assert!(events[0].contains("found nothing to fix"), "{events:?}");
    }

    #[tokio::test]
    async fn a_fix_that_fails_ends_the_stage_with_what_went_wrong() {
        let f = fixture();
        let stages =
            Landing::mergeable().with_fix(Fix::Fails("the branch moved to abc while I worked"));
        *stages.jobs.lock().unwrap() = vec![red("clippy")];
        let report = go(&f, &stages, quick()).await;

        assert!(
            matches!(&report.ending, Ending::Stopped { step: Step::Land, why } if why.contains("the branch moved to abc")),
            "{:?}",
            report.ending
        );
        assert_eq!(stages.fixes_asked(), 1);
    }

    /// A head Henk pushed from a run somebody else started, such as
    /// `@meneer-henk fix ci`, is his own: the stage reviews it and goes on
    /// instead of handing the pull request over.
    #[tokio::test]
    async fn a_head_henk_pushed_for_somebody_else_is_still_his() {
        // The checks run on the first look; at the second the head has moved.
        let waiting = || {
            let stages = Landing::mergeable();
            *stages.jobs.lock().unwrap() = vec![job("test", CiStatus::InProgress, None)];
            *stages.pushed_at_read.lock().unwrap() = Some(2);
            stages
        };
        let f = fixture();
        let stages = waiting();
        f.app.own_pushes.record(head(1).as_str());
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready, "{:?}", report.ending);
        assert_eq!(stages.reviewed(), [head(0), head(1)]);

        let g = fixture();
        let stages = waiting();
        let report = go(&g, &stages, quick()).await;
        assert_eq!(
            report.ending,
            Ending::HandedOver("somebody else pushed to its branch".to_owned()),
            "a push nobody recorded is a person's"
        );
    }

    #[tokio::test]
    async fn failing_checks_stop_the_stage_with_one_comment_naming_them() {
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.jobs.lock().unwrap() = vec![green(), red("clippy")];
        let never_fix = Land {
            max_ci_fixes: 0,
            ..quick()
        };
        let report = go(&f, &stages, never_fix).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "checks failed: clippy".to_owned()
            }
        );
        assert_eq!(stages.fixes_asked(), 0);
        let posted = comments(&f);
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert!(
            posted[0].starts_with(
                "I stopped the autonomous run during keeping the pull request landable: checks failed: clippy."
            ),
            "{}",
            posted[0]
        );
        assert!(
            posted[0].contains("Pull request #12 is open"),
            "{}",
            posted[0]
        );
        assert_eq!(record(&f).await.status, RunStatus::Failed);
        let rows = stage_states(&f).await;
        assert!(
            rows.contains(&(Stage::Land, StageState::Failed)),
            "{rows:?}"
        );
    }

    #[tokio::test]
    async fn running_checks_are_waited_for_not_called_ready() {
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.jobs.lock().unwrap() = vec![job("test", CiStatus::InProgress, None)];
        let (report, ()) = tokio::join!(go(&f, &stages, quick()), async {
            // Many looks, none of them ready.
            for _ in 0..200 {
                if *stages.reads.lock().unwrap() >= 5 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(*stages.reads.lock().unwrap() >= 5, "it kept looking");
            *stages.jobs.lock().unwrap() = vec![green()];
        });
        assert_eq!(report.ending, Ending::Ready);
    }

    #[tokio::test]
    async fn a_rebase_that_fails_stops_the_stage_and_says_why() {
        let f = fixture();
        let stages = Landing::new(|_| MergeState::Conflicts);
        stages.rebases.lock().unwrap().push_back(Err(RunFailed {
            what: "the rebase",
            reason: "src/a.rs conflicts and the executor could not resolve it".to_owned(),
        }
        .into()));
        let report = go(&f, &stages, quick()).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "src/a.rs conflicts and the executor could not resolve it".to_owned()
            }
        );
        let posted = comments(&f);
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert!(posted[0].contains("src/a.rs conflicts"), "{}", posted[0]);
    }

    #[tokio::test]
    async fn the_rebases_are_limited_and_the_stage_says_so() {
        let f = fixture();
        // The base keeps moving: whatever the head, it must catch up.
        let stages = Landing::new(|_| MergeState::NeedsRebase);
        let land = Land {
            max_rebases: 2,
            ..quick()
        };
        let report = go(&f, &stages, land).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "it needed a rebase again after 2 rebases".to_owned()
            }
        );
        let rebases = stages.calls().iter().filter(|c| *c == "rebase").count();
        assert_eq!(rebases, 2);
    }

    #[tokio::test]
    async fn a_platform_that_is_slow_to_see_a_rebase_gets_the_looks_to_catch_up() {
        let f = fixture();
        // After the rebase the platform says "must catch up" for as many
        // looks as it has to, then sees the new head.
        let looks = AtomicUsize::new(0);
        let lag = usize::try_from(REBASE_LAG_LOOKS).unwrap();
        let stages = Landing::new(move |rebased| {
            let behind = rebased == 0 || looks.fetch_add(1, Ordering::SeqCst) < lag;
            if behind {
                MergeState::NeedsRebase
            } else {
                MergeState::Mergeable
            }
        });
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready, "{:?}", report.ending);
        let rebases = stages.calls().iter().filter(|c| *c == "rebase").count();
        assert_eq!(rebases, 1, "the rebased head was rebased again");
    }

    #[tokio::test]
    async fn a_review_that_does_not_pass_stops_the_stage() {
        let f = fixture();
        let stages = Landing::new(|rebased| {
            if rebased == 0 {
                MergeState::NeedsRebase
            } else {
                MergeState::Mergeable
            }
        })
        .with_review(Review::Passes)
        .with_review(Review::Finds);
        let report = go(&f, &stages, quick()).await;

        let Ending::Stopped { step, why } = &report.ending else {
            panic!("{:?}", report.ending);
        };
        assert_eq!(*step, Step::Review);
        assert!(why.contains("1 issue found"), "{why}");
        assert_eq!(comments(&f).len(), 1);
        let rows = stage_states(&f).await;
        assert!(
            rows.contains(&(Stage::Land, StageState::Failed)),
            "{rows:?}"
        );
    }

    #[tokio::test]
    async fn a_review_a_newer_commit_replaced_goes_on_with_the_head_as_it_is() {
        let f = fixture();
        let stages = Landing::mergeable().with_review(Review::Replaced);
        let report = go(&f, &stages, quick()).await;

        assert_eq!(report.ending, Ending::Ready);
        assert_eq!(
            stages.calls(),
            ["plan", "execute", "review", "review"],
            "the stage reviews the head itself"
        );
    }

    #[tokio::test]
    async fn the_stage_has_a_time_of_its_own() {
        let f = fixture();
        // The platform wants a review nobody gives.
        let stages = Landing::new(|_| MergeState::Blocked("a review is required".to_owned()));
        let land = Land {
            timeout: Duration::from_secs(1),
            ..quick()
        };
        let report = go(&f, &stages, land).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "the time limit of 1s for keeping the pull request landable was reached"
                    .to_owned()
            }
        );
        assert_eq!(comments(&f).len(), 1);
    }

    #[tokio::test]
    async fn a_rebase_that_outlasts_the_stage_is_stopped_at_the_stage_s_time() {
        let f = fixture();
        // The whole run has a minute; the stage has one second.
        let stages = Landing::new(|_| MergeState::NeedsRebase);
        *stages.hang_rebase.lock().unwrap() = true;
        let land = Land {
            timeout: Duration::from_secs(1),
            ..quick()
        };
        let began = std::time::Instant::now();
        let report = go(&f, &stages, land).await;

        assert!(
            began.elapsed() < Duration::from_secs(20),
            "the stage's second ended it, not the run's minute"
        );
        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Land,
                why: "the time limit of 1s for keeping the pull request landable was reached"
                    .to_owned()
            }
        );
        let stopped = stages.stopped.lock().unwrap().clone();
        assert_eq!(stopped.len(), 1, "{stopped:?}");
        assert!(stopped[0].ends_with(" by ticket:time-limit"), "{stopped:?}");
    }

    #[tokio::test]
    async fn a_person_cancelling_the_run_ends_the_stage_cancelled() {
        let f = fixture();
        let stages = Landing::new(|_| MergeState::Unknown);
        let ticket = RunId::parse(TICKET).unwrap();
        let (report, ()) = tokio::join!(go(&f, &stages, quick()), async {
            for _ in 0..400 {
                if *stages.reads.lock().unwrap() >= 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(f.app.cancels.cancel(&ticket, "github:1234".to_owned()));
        });
        assert_eq!(report.ending, Ending::Cancelled("github:1234".to_owned()));
        assert_eq!(record(&f).await.status, RunStatus::Cancelled);
    }

    #[tokio::test]
    async fn a_platform_that_cannot_be_read_is_tried_again_and_then_given_up_on() {
        // A hiccup is looked past.
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.unreadable.lock().unwrap() = 2;
        let report = go(&f, &stages, quick()).await;
        assert_eq!(report.ending, Ending::Ready);

        // A platform that stays down ends the stage.
        let f = fixture();
        let stages = Landing::mergeable();
        *stages.unreadable.lock().unwrap() = 100;
        let report = go(&f, &stages, quick()).await;
        let Ending::Stopped { step, why } = &report.ending else {
            panic!("{:?}", report.ending);
        };
        assert_eq!(*step, Step::Land);
        assert!(why.starts_with("the platform could not be read"), "{why}");
        assert_eq!(*stages.reads.lock().unwrap(), MAX_READ_FAILURES);
    }

    #[tokio::test]
    async fn without_the_land_stage_a_passed_review_ends_the_run_as_before() {
        let f = fixture();
        let stages = Landing::mergeable();
        let cancel = f.app.shutdown.child_token();
        let _registered = f
            .app
            .cancels
            .register(RunId::parse(TICKET).unwrap(), cancel.clone());
        let limits = Limits {
            land: None,
            ..with_land(quick())
        };
        let report = run_ticket(&f.app, &stages, request(), cancel, limits)
            .await
            .unwrap();
        assert_eq!(report.ending, Ending::Passed);
        assert_eq!(*stages.reads.lock().unwrap(), 0, "the platform is not read");
        let rows = stage_states(&f).await;
        assert!(
            !rows.iter().any(|(stage, _)| *stage == Stage::Land),
            "{rows:?}"
        );
    }
}
