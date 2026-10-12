//! Code review (§3): triggers, lanes, outcomes and what the platform shows.

use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};

use crate::allowlist::{Platform, RepoRef};
use crate::identity::Requester;
use crate::marker::ModelId;
use crate::run::RunId;
use crate::{GITHUB_HANDLE, GITLAB_HANDLE};

/// Why a commit sha was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("commit sha {0:?} must be 40 lowercase hex characters")]
pub struct CommitShaError(String);

/// A full git commit sha.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

impl CommitSha {
    /// Validates a full sha-1 commit id. Uppercase hex is accepted and lowered.
    ///
    /// # Errors
    ///
    /// Returns [`CommitShaError`] unless the value is exactly 40 hex digits.
    pub fn parse(value: &str) -> Result<Self, CommitShaError> {
        let value = value.trim();
        if value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(CommitShaError(value.to_owned()))
        }
    }

    /// The sha as lowercase hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first seven characters, as git shows them.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0.get(..7).unwrap_or(&self.0)
    }
}

impl TryFrom<String> for CommitSha {
    type Error = CommitShaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<CommitSha> for String {
    fn from(sha: CommitSha) -> Self {
        sha.0
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewTarget {
    /// Repository.
    pub repo: RepoRef,
    /// Number (GitLab: iid).
    pub number: u64,
}

impl ReviewTarget {
    /// The platform.
    #[must_use]
    pub fn platform(&self) -> Platform {
        self.repo.platform()
    }
}

/// Henk's handle on a platform, as it appears in a mention.
#[must_use]
pub const fn handle(platform: Platform) -> &'static str {
    match platform {
        Platform::GitHub => GITHUB_HANDLE,
        Platform::GitLab => GITLAB_HANDLE,
    }
}

/// Whether a comment is a review request (§3.1): its whole text is
/// `@meneer-henk review` (GitLab: `@meneerhenk review`).
///
/// Surrounding whitespace is ignored and the handle is compared without
/// regard to case, as the platforms do. Anything more in the text makes it a
/// mention (§3.4), not a request.
#[must_use]
pub fn is_review_request(platform: Platform, body: &str) -> bool {
    let mut words = body.split_whitespace();
    let (Some(first), Some(second), None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    first
        .strip_prefix('@')
        .is_some_and(|login| login.eq_ignore_ascii_case(handle(platform)))
        && second == "review"
}

/// Whether a comment mentions Henk on a platform (§3.4).
#[must_use]
pub fn mentions_henk(platform: Platform, body: &str) -> bool {
    let wanted = handle(platform);
    body.split(|c: char| {
        c.is_whitespace() || matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | '(' | ')')
    })
    .filter_map(|word| word.strip_prefix('@'))
    .any(|login| login.eq_ignore_ascii_case(wanted))
}

/// What started a review (§3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTrigger {
    /// The pull/merge request was opened or reopened.
    Opened,
    /// New commits were pushed to an open pull/merge request.
    NewCommits,
    /// A person posted a review command on the pull/merge request.
    Command,
    /// A colleague asked in Discord.
    Discord(Requester),
    /// A request through the CLI or the HTTP API, on behalf of a Team Lead.
    Api(Requester),
    /// Henk started again after a restart interrupted this run, on the pull
    /// request's current head (#160).
    Resumed(RunId),
}

/// How a resumed run's trigger starts, before the run it resumes.
const RESUMED_AFTER: &str = "resumed after interrupted run ";

impl ReviewTrigger {
    /// What the run record says started it.
    #[must_use]
    pub fn words(&self) -> String {
        match self {
            Self::Opened => "opened".to_owned(),
            Self::NewCommits => "new commits".to_owned(),
            Self::Command => "review command".to_owned(),
            Self::Discord(_) => "discord".to_owned(),
            Self::Api(_) => "requested".to_owned(),
            Self::Resumed(interrupted) => format!("{RESUMED_AFTER}{interrupted}"),
        }
    }

    /// The run a record's trigger says it resumed, when [`Self::words`]
    /// wrote it for [`Self::Resumed`]; `None` for any other trigger.
    #[must_use]
    pub fn resumed_from(words: &str) -> Option<RunId> {
        words
            .strip_prefix(RESUMED_AFTER)
            .and_then(|run| RunId::parse(run).ok())
    }
}

/// One independent reviewer inside a review (§1.1, §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaneName(String);

impl LaneName {
    /// Names a lane. The name is what the summary shows when the lane does
    /// not finish; it must not reveal the model (§3.2, open question 4).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LaneName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A configured lane: a name the summary may show, and the model behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneSpec {
    /// Shown when the lane does not finish. Must not name the model (§3.2).
    pub name: LaneName,
    /// The configured model id this lane runs on.
    pub model: ModelId,
    /// The skills this lane may load.
    #[serde(default)]
    pub skills: Vec<crate::skill::SkillName>,
}

/// How one lane ended (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneOutcome {
    /// The lane read the change and posted what it found.
    Finished,
    /// The lane reached its time limit. What it posted stands; it posts
    /// nothing more. The review completes on it like on a finished lane.
    Stopped,
    /// The lane failed after retries or hung, and was dropped.
    Dropped,
}

/// One lane's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneResult {
    /// Which lane.
    pub lane: LaneName,
    /// How it ended.
    pub outcome: LaneOutcome,
}

/// The conclusion of the GitHub check "Meneer Henk" (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConclusion {
    /// No issues found.
    Success,
    /// Issues found. Advisory; never blocks a merge (§8.2).
    Neutral,
    /// The review did not complete. Henk's failure, not the code's.
    Failure,
}

/// The GitLab commit status "Meneer Henk" (§3.3). It is always `success`
/// so it can never block a pipeline (§8.2); the count goes in the description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatus {
    /// The status description.
    pub description: String,
}

/// The outcome of one review of one commit (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// The commit that was reviewed.
    pub commit: CommitSha,
    /// Every lane and how it ended.
    pub lanes: Vec<LaneResult>,
    /// The number of Henk's distinct findings still open on the diff at this
    /// commit, including findings from earlier reviews whose line is still
    /// in the diff.
    pub open_findings: usize,
    /// Every changed file was left out by `review.ignore`, so no lane ran
    /// (§3.2). Such a review is complete.
    pub nothing_to_review: bool,
    /// Why the review stopped before it ended, if it did. A stopped review
    /// is not complete (§3.3).
    pub stopped: Option<Stopped>,
    /// Why it was not reviewed, with [`Stopped::NotReviewed`] (#262): Henk's
    /// own words, such as "pull request #7 is a draft". With
    /// [`Stopped::Cancelled`], where the cancel came from, such as "over
    /// MCP" (#293).
    pub not_reviewed: Option<String>,
    /// A review loop (#284) ran out of rounds with the reviewer's last
    /// findings still open. They are not on the pull request, so
    /// `open_findings` does not count them, and the review is no pass.
    pub findings_left: bool,
}

/// Why a review stopped before it ended. They exclude each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// A newer commit arrived; the review of the newer commit stands
    /// (§3.3). Not Henk's failure.
    Superseded,
    /// Henk was stopped: Ctrl-C, a shutdown, or a process that died (§3.3).
    Interrupted,
    /// A person cancelled the review from the dashboard (#69). Not Henk's
    /// failure.
    Cancelled,
    /// It waited for a slot with its check queued, and once it had one the
    /// pull request could not be reviewed: closed, a draft, or the
    /// configuration no longer allows it (#262). Not Henk's failure.
    NotReviewed,
}

impl ReviewOutcome {
    /// A review that stopped before it ended, for `why`.
    fn stopped_as(commit: CommitSha, why: Stopped, not_reviewed: Option<String>) -> Self {
        Self {
            commit,
            lanes: Vec::new(),
            open_findings: 0,
            nothing_to_review: false,
            stopped: Some(why),
            not_reviewed,
            findings_left: false,
        }
    }

    /// The outcome of a review stopped because a newer commit arrived.
    #[must_use]
    pub fn superseded(commit: CommitSha) -> Self {
        Self::stopped_as(commit, Stopped::Superseded, None)
    }

    /// The outcome of a review a person or an MCP client cancelled; `via`
    /// says where from ("from the dashboard", "over MCP"), or is empty.
    #[must_use]
    pub fn cancelled(commit: CommitSha, via: &str) -> Self {
        let via = (!via.is_empty()).then(|| via.to_owned());
        Self::stopped_as(commit, Stopped::Cancelled, via)
    }

    /// The outcome of a review Henk was stopped in the middle of.
    #[must_use]
    pub fn interrupted(commit: CommitSha) -> Self {
        Self::stopped_as(commit, Stopped::Interrupted, None)
    }

    /// The outcome of a review that waited for a slot and then could not
    /// run, for `reason` in Henk's own words (#262).
    #[must_use]
    pub fn not_reviewed(commit: CommitSha, reason: impl Into<String>) -> Self {
        Self::stopped_as(commit, Stopped::NotReviewed, Some(reason.into()))
    }

    /// Whether the review completed: at least one lane finished, or there
    /// was nothing to review.
    #[must_use]
    pub fn completed(&self) -> bool {
        self.stopped.is_none()
            && (self.nothing_to_review
                || self.lanes.iter().any(|lane| {
                    matches!(lane.outcome, LaneOutcome::Finished | LaneOutcome::Stopped)
                }))
    }

    /// The lanes that reached their time limit, in order.
    pub fn stopped_lanes(&self) -> impl Iterator<Item = &LaneName> + '_ {
        self.lanes
            .iter()
            .filter(|lane| lane.outcome == LaneOutcome::Stopped)
            .map(|lane| &lane.lane)
    }

    /// The lanes that were dropped, in order.
    pub fn dropped_lanes(&self) -> impl Iterator<Item = &LaneName> + '_ {
        self.lanes
            .iter()
            .filter(|lane| lane.outcome == LaneOutcome::Dropped)
            .map(|lane| &lane.lane)
    }

    /// The GitHub check conclusion.
    #[must_use]
    pub fn check_conclusion(&self) -> CheckConclusion {
        if matches!(
            self.stopped,
            Some(Stopped::Superseded | Stopped::Cancelled | Stopped::NotReviewed)
        ) {
            // Stopped on purpose: neither a pass nor Henk's failure (§8.2).
            CheckConclusion::Neutral
        } else if !self.completed() {
            CheckConclusion::Failure
        } else if self.open_findings == 0 && !self.findings_left {
            CheckConclusion::Success
        } else {
            CheckConclusion::Neutral
        }
    }

    /// The GitLab commit status.
    #[must_use]
    pub fn commit_status(&self) -> CommitStatus {
        CommitStatus {
            description: self.headline(),
        }
    }

    /// The first line of the summary: the count, or that the review did not complete.
    #[must_use]
    pub fn headline(&self) -> String {
        match self.stopped {
            Some(Stopped::Superseded) => return "Superseded by a newer commit.".to_owned(),
            Some(Stopped::Interrupted) => return "Review interrupted.".to_owned(),
            Some(Stopped::Cancelled) => {
                return match &self.not_reviewed {
                    Some(via) => format!("Cancelled {via}."),
                    None => "Cancelled.".to_owned(),
                };
            }
            Some(Stopped::NotReviewed) => {
                let why = self.not_reviewed.as_deref().unwrap_or("it could not run");
                return format!("Not reviewed: {why}.");
            }
            None => {}
        }
        if !self.completed() {
            return "Review did not complete.".to_owned();
        }
        if self.nothing_to_review && self.open_findings == 0 {
            return "Nothing to review: every changed file is in review.ignore.".to_owned();
        }
        match self.open_findings {
            0 if self.findings_left => "The review loop left findings open.".to_owned(),
            0 => "No issues found.".to_owned(),
            1 => "1 issue found.".to_owned(),
            n => format!("{n} issues found."),
        }
    }

    /// The text of the summary comment or note (§3.3), without the run link
    /// and hidden marker, which the platform layer adds.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = self.headline();
        let dropped: Vec<&str> = self.dropped_lanes().map(LaneName::as_str).collect();
        match dropped.as_slice() {
            [] => {}
            [one] => {
                let _ = write!(text, " Lane {one} did not finish.");
            }
            many => {
                let _ = write!(text, " Lanes {} did not finish.", many.join(", "));
            }
        }
        let stopped: Vec<&str> = self.stopped_lanes().map(LaneName::as_str).collect();
        match stopped.as_slice() {
            [] => {}
            [one] => {
                let _ = write!(text, " Lane {one} stopped at the time limit.");
            }
            many => {
                let _ = write!(
                    text,
                    " Lanes {} stopped at the time limit.",
                    many.join(", ")
                );
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// #160: a resumed review's record names the run it resumes.
    #[test]
    fn a_resumed_review_names_the_interrupted_run() {
        let run = RunId::parse("r-20261006-6b344328").unwrap();
        assert_eq!(
            ReviewTrigger::Resumed(run).words(),
            "resumed after interrupted run r-20261006-6b344328"
        );
        assert_eq!(ReviewTrigger::NewCommits.words(), "new commits");
    }

    /// #160: a record's trigger reads back as the run it resumed, and only
    /// a resumed one does.
    #[test]
    fn a_resumed_trigger_reads_back_as_its_run() {
        let run = RunId::parse("r-20261006-6b344328").unwrap();
        let words = ReviewTrigger::Resumed(run.clone()).words();
        assert_eq!(ReviewTrigger::resumed_from(&words), Some(run));
        for other in [
            "opened",
            "requested",
            "new commits",
            "resumed after interrupted run ",
        ] {
            assert_eq!(ReviewTrigger::resumed_from(other), None, "{other}");
        }
    }

    fn outcome(lanes: &[(&str, LaneOutcome)], open_findings: usize) -> ReviewOutcome {
        ReviewOutcome {
            commit: CommitSha::parse(SHA).unwrap_or_else(|e| panic!("{e}")),
            lanes: lanes
                .iter()
                .map(|(name, outcome)| LaneResult {
                    lane: LaneName::new(*name),
                    outcome: *outcome,
                })
                .collect(),
            open_findings,
            nothing_to_review: false,
            stopped: None,
            not_reviewed: None,
            findings_left: false,
        }
    }

    #[test]
    fn sha_is_validated_and_lowered() {
        let sha = CommitSha::parse(&SHA.to_ascii_uppercase()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(sha.as_str(), SHA);
        assert_eq!(sha.short(), "0123456");
        assert!(CommitSha::parse("abc").is_err());
        assert!(CommitSha::parse("g123456789abcdef0123456789abcdef01234567").is_err());
    }

    #[test]
    fn review_request_is_the_whole_text() {
        assert!(is_review_request(Platform::GitHub, "@meneer-henk review"));
        assert!(is_review_request(
            Platform::GitHub,
            "  @Meneer-Henk review\n"
        ));
        assert!(is_review_request(Platform::GitLab, "@meneerhenk review"));
        assert!(
            !is_review_request(Platform::GitLab, "@meneer-henk review"),
            "wrong handle"
        );
        assert!(!is_review_request(
            Platform::GitHub,
            "@meneer-henk review please"
        ));
        assert!(!is_review_request(Platform::GitHub, "@meneer-henk"));
        assert!(!is_review_request(Platform::GitHub, "review"));
    }

    #[test]
    fn mentions_are_detected_by_handle() {
        assert!(mentions_henk(
            Platform::GitHub,
            "Thanks @meneer-henk, good catch."
        ));
        assert!(mentions_henk(Platform::GitLab, "@meneerhenk?"));
        assert!(!mentions_henk(
            Platform::GitHub,
            "meneer-henk without an at sign"
        ));
        assert!(!mentions_henk(Platform::GitHub, "@meneer-henkie"));
    }

    #[test]
    fn no_findings_is_success() {
        let outcome = outcome(
            &[("a", LaneOutcome::Finished), ("b", LaneOutcome::Finished)],
            0,
        );
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Success);
        assert_eq!(outcome.summary(), "No issues found.");
    }

    #[test]
    fn findings_a_review_loop_left_open_are_neutral_not_a_pass() {
        let mut left = outcome(
            &[
                ("fixer", LaneOutcome::Finished),
                ("reviewer", LaneOutcome::Finished),
            ],
            0,
        );
        left.findings_left = true;
        assert!(left.completed());
        assert_eq!(left.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(left.summary(), "The review loop left findings open.");
        assert!(crate::text::is_in_style(&left.summary()));
        left.open_findings = 2;
        assert_eq!(left.summary(), "2 issues found.");
        assert_eq!(left.check_conclusion(), CheckConclusion::Neutral);
    }

    #[test]
    fn findings_are_neutral_never_failure() {
        let outcome = outcome(&[("a", LaneOutcome::Finished)], 1);
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(outcome.summary(), "1 issue found.");
        assert_eq!(outcome.commit_status().description, "1 issue found.");
    }

    #[test]
    fn partial_success_is_success_and_names_dropped_lanes() {
        let outcome = outcome(
            &[
                ("a", LaneOutcome::Finished),
                ("b", LaneOutcome::Dropped),
                ("c", LaneOutcome::Dropped),
            ],
            3,
        );
        assert!(outcome.completed());
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(
            outcome.summary(),
            "3 issues found. Lanes b, c did not finish."
        );

        let one = outcome_one_dropped();
        assert_eq!(one.summary(), "No issues found. Lane b did not finish.");
    }

    fn outcome_one_dropped() -> ReviewOutcome {
        outcome(
            &[("a", LaneOutcome::Finished), ("b", LaneOutcome::Dropped)],
            0,
        )
    }

    #[test]
    fn an_interrupted_review_did_not_complete_and_says_so() {
        let interrupted = ReviewOutcome::interrupted(outcome(&[], 0).commit);
        assert!(!interrupted.completed());
        assert_eq!(interrupted.check_conclusion(), CheckConclusion::Failure);
        assert_eq!(interrupted.headline(), "Review interrupted.");
        assert!(crate::text::is_in_style(&interrupted.summary()));
        let partway = ReviewOutcome {
            stopped: Some(Stopped::Interrupted),
            ..outcome(&[("a", LaneOutcome::Finished)], 0)
        };
        assert!(
            !partway.completed(),
            "a finished lane does not make it complete"
        );
    }

    #[test]
    fn a_superseded_review_is_neutral_and_says_why() {
        let superseded = ReviewOutcome::superseded(outcome(&[], 0).commit);
        assert!(!superseded.completed());
        assert_eq!(superseded.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(superseded.headline(), "Superseded by a newer commit.");
        assert!(crate::text::is_in_style(&superseded.summary()));
    }

    #[test]
    fn a_cancelled_review_is_neutral_and_says_so() {
        let cancelled = ReviewOutcome::cancelled(outcome(&[], 0).commit, "from the dashboard");
        assert!(!cancelled.completed());
        assert_eq!(cancelled.stopped, Some(Stopped::Cancelled));
        assert_eq!(cancelled.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(cancelled.headline(), "Cancelled from the dashboard.");
        assert_eq!(
            cancelled.commit_status().description,
            "Cancelled from the dashboard."
        );
        assert!(crate::text::is_in_style(&cancelled.summary()));
        let over_mcp = ReviewOutcome::cancelled(outcome(&[], 0).commit, "over MCP");
        assert_eq!(over_mcp.headline(), "Cancelled over MCP.");
        let unsaid = ReviewOutcome::cancelled(outcome(&[], 0).commit, "");
        assert_eq!(unsaid.headline(), "Cancelled.");
    }

    #[test]
    fn a_review_that_could_not_run_after_waiting_is_neutral_and_says_why() {
        let draft =
            ReviewOutcome::not_reviewed(outcome(&[], 0).commit, "pull request #7 is a draft");
        assert!(!draft.completed());
        assert_eq!(draft.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(
            draft.headline(),
            "Not reviewed: pull request #7 is a draft."
        );
        assert!(crate::text::is_in_style(&draft.summary()));
    }

    #[test]
    fn nothing_to_review_is_a_complete_review_without_lanes() {
        let skipped = ReviewOutcome {
            nothing_to_review: true,
            ..outcome(&[], 0)
        };
        assert!(skipped.completed());
        assert_eq!(skipped.check_conclusion(), CheckConclusion::Success);
        assert_eq!(
            skipped.summary(),
            "Nothing to review: every changed file is in review.ignore."
        );
        // Earlier findings still on the diff are still counted (§3.3).
        let open = ReviewOutcome {
            nothing_to_review: true,
            ..outcome(&[], 2)
        };
        assert_eq!(open.summary(), "2 issues found.");
        assert_eq!(open.check_conclusion(), CheckConclusion::Neutral);
        assert!(
            !outcome(&[], 0).completed(),
            "no lanes and something to review"
        );
    }

    #[test]
    fn no_lane_finished_is_henks_failure() {
        let stopped = outcome(&[("a", LaneOutcome::Stopped)], 2);
        assert!(stopped.completed());
        assert_eq!(stopped.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(
            stopped.summary(),
            "2 issues found. Lane a stopped at the time limit."
        );
        let mixed = outcome(
            &[
                ("a", LaneOutcome::Stopped),
                ("b", LaneOutcome::Dropped),
                ("c", LaneOutcome::Stopped),
            ],
            0,
        );
        assert_eq!(mixed.check_conclusion(), CheckConclusion::Success);
        assert_eq!(
            mixed.summary(),
            "No issues found. Lane b did not finish. Lanes a, c stopped at the time limit."
        );
        let outcome = outcome(&[("a", LaneOutcome::Dropped)], 0);
        assert!(!outcome.completed());
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Failure);
        assert_eq!(
            outcome.summary(),
            "Review did not complete. Lane a did not finish."
        );
    }
}
