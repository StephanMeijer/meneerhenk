//! What Henk reads of a pull request's CI (#420): the checks and jobs on a
//! commit, a job's log and its annotations. Read-only, and through REST
//! rather than MCP, because the read-only MCP servers do not give CI.

use henk_domain::review::CommitSha;

use crate::error::PlatformError;
use crate::writer::ReviewTarget;

/// A job's id as the platform gives it: a GitHub check run's id, a GitHub
/// commit status as `status:<context>`, or a GitLab job's id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CiJobId(pub String);

impl std::fmt::Display for CiJobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    /// Waiting to start: queued, pending, or waiting for someone.
    Queued,
    /// Running.
    InProgress,
    /// Done; its conclusion says how.
    Completed,
}

impl CiStatus {
    /// The word for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in progress",
            Self::Completed => "completed",
        }
    }
}

/// How a completed job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiConclusion {
    /// It passed.
    Success,
    /// It failed.
    Failure,
    /// Someone or something cancelled it.
    Cancelled,
    /// It did not run.
    Skipped,
    /// It ran out of time.
    TimedOut,
    /// Neither passed nor failed: neutral, or a conclusion Henk does not know.
    Neutral,
}

impl CiConclusion {
    /// The word for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::TimedOut => "timed out",
            Self::Neutral => "neutral",
        }
    }
}

/// One check or job on a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiJob {
    /// Its id.
    pub id: CiJobId,
    /// Its name.
    pub name: String,
    /// The workflow, pipeline or app it belongs to, where known.
    pub workflow: Option<String>,
    /// Where it is.
    pub status: CiStatus,
    /// How it ended, once completed.
    pub conclusion: Option<CiConclusion>,
    /// When it started (RFC 3339, as the platform gives it).
    pub started_at: Option<String>,
    /// When it ended.
    pub completed_at: Option<String>,
    /// Where people read it.
    pub url: Option<String>,
    /// Whether Henk can read its log: GitHub Actions jobs and GitLab jobs,
    /// not another app's check or a commit status.
    pub has_log: bool,
}

/// A job's log, or the end of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiLog {
    /// The text: whole lines.
    pub text: String,
    /// Whether the start was left out to stay under the size asked for.
    pub cut: bool,
}

/// One note on a file and lines a check left, such as a compiler error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiAnnotation {
    /// The file.
    pub path: String,
    /// Its first line.
    pub start_line: u64,
    /// Its last line.
    pub end_line: u64,
    /// `notice`, `warning` or `failure`.
    pub level: String,
    /// Its title, if any.
    pub title: Option<String>,
    /// What it says.
    pub message: String,
}

/// Reads the CI of a pull request. Every call names the pull request; the
/// caller keeps it to the commit under review.
#[async_trait::async_trait]
pub trait CiReader: Send + Sync {
    /// The checks and jobs on `commit` of `target`, without Henk's own.
    async fn jobs(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
    ) -> Result<Vec<CiJob>, PlatformError>;

    /// The log of `job`: its last `max_bytes` at most, cut at a line.
    async fn job_log(
        &self,
        target: &ReviewTarget,
        job: &CiJobId,
        max_bytes: usize,
    ) -> Result<CiLog, PlatformError>;

    /// The annotations `job` left.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Unsupported`] where the platform has none.
    async fn annotations(
        &self,
        _target: &ReviewTarget,
        _job: &CiJobId,
    ) -> Result<Vec<CiAnnotation>, PlatformError> {
        Err(PlatformError::Unsupported(
            "this platform has no check annotations".to_owned(),
        ))
    }
}

/// Keeps the last `max` bytes of a body read in chunks, so a log of any size
/// never sits in memory whole.
#[derive(Debug)]
pub(crate) struct Tail {
    max: usize,
    bytes: Vec<u8>,
    cut: bool,
}

impl Tail {
    pub(crate) const fn new(max: usize) -> Self {
        Self {
            max,
            bytes: Vec::new(),
            cut: false,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        // Drain in bulk, not per chunk, so the copy stays linear.
        if self.bytes.len() > self.max.saturating_mul(2).max(64 * 1024) {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if self.bytes.len() > self.max {
            let over = self.bytes.len() - self.max;
            self.bytes.drain(..over);
            self.cut = true;
        }
    }

    /// The kept text, starting at a whole line when the start was cut.
    pub(crate) fn finish(mut self) -> CiLog {
        self.trim();
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.cut
            && let Some(newline) = text.find('\n')
        {
            text.drain(..=newline);
        }
        CiLog {
            text,
            cut: self.cut,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tail;

    #[test]
    fn a_tail_keeps_the_last_whole_lines() {
        let mut tail = Tail::new(10);
        for chunk in ["line one\n", "line two\n", "three\n"] {
            tail.push(chunk.as_bytes());
        }
        let log = tail.finish();
        assert!(log.cut);
        assert_eq!(log.text, "three\n");
    }

    #[test]
    fn a_short_log_is_whole() {
        let mut tail = Tail::new(1024);
        tail.push(b"a\nb\n");
        let log = tail.finish();
        assert!(!log.cut);
        assert_eq!(log.text, "a\nb\n");
    }
}
