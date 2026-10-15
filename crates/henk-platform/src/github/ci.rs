//! A pull request's CI on GitHub (#420): the check runs and commit statuses
//! on a commit, an Actions job's log, and a check run's annotations. Needs
//! the App's read access to Checks, Actions and Commit statuses.

use std::collections::HashMap;

use henk_domain::review::CommitSha;
use serde_json::Value;

use crate::ci::{CiAnnotation, CiConclusion, CiJob, CiJobId, CiLog, CiReader, CiStatus};
use crate::error::PlatformError;
use crate::github::writer::{CHECK_NAME, GitHubWriter};
use crate::writer::ReviewTarget;

/// The prefix of a commit status's id: it has a context, not a number.
const STATUS_PREFIX: &str = "status:";

fn text(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn check_status(status: &str) -> CiStatus {
    match status {
        "completed" => CiStatus::Completed,
        "in_progress" => CiStatus::InProgress,
        _ => CiStatus::Queued,
    }
}

fn check_conclusion(conclusion: &str) -> CiConclusion {
    match conclusion {
        "success" => CiConclusion::Success,
        "failure" | "action_required" | "startup_failure" | "stale" => CiConclusion::Failure,
        "cancelled" => CiConclusion::Cancelled,
        "skipped" => CiConclusion::Skipped,
        "timed_out" => CiConclusion::TimedOut,
        _ => CiConclusion::Neutral,
    }
}

/// A check run as a job; `workflows` names the Actions workflow of each
/// check suite, where Henk could read them.
fn check_run(run: &Value, workflows: &HashMap<u64, String>) -> Option<CiJob> {
    let id = run.get("id").and_then(Value::as_u64)?;
    let name = text(run, "/name")?;
    let actions = run.pointer("/app/slug").and_then(Value::as_str) == Some("github-actions");
    let workflow = run
        .pointer("/check_suite/id")
        .and_then(Value::as_u64)
        .and_then(|suite| workflows.get(&suite).cloned())
        .or_else(|| text(run, "/app/name"));
    let status = check_status(run.get("status").and_then(Value::as_str).unwrap_or(""));
    Some(CiJob {
        id: CiJobId(id.to_string()),
        name,
        workflow,
        status,
        conclusion: (status == CiStatus::Completed)
            .then(|| check_conclusion(run.get("conclusion").and_then(Value::as_str).unwrap_or(""))),
        started_at: text(run, "/started_at"),
        completed_at: text(run, "/completed_at"),
        url: text(run, "/html_url").or_else(|| text(run, "/details_url")),
        has_log: actions,
    })
}

/// A commit status, from CI outside GitHub, as a job. It has no log.
fn commit_status(status: &Value) -> Option<CiJob> {
    let context = text(status, "/context")?;
    let (state, conclusion) = match status.get("state").and_then(Value::as_str) {
        Some("success") => (CiStatus::Completed, Some(CiConclusion::Success)),
        Some("failure" | "error") => (CiStatus::Completed, Some(CiConclusion::Failure)),
        _ => (CiStatus::Queued, None),
    };
    Some(CiJob {
        id: CiJobId(format!("{STATUS_PREFIX}{context}")),
        name: context,
        workflow: text(status, "/description"),
        status: state,
        conclusion,
        started_at: text(status, "/created_at"),
        completed_at: conclusion.and_then(|_| text(status, "/updated_at")),
        url: text(status, "/target_url"),
        has_log: false,
    })
}

/// A check run's numeric id; a commit status has none.
fn check_run_id(job: &CiJobId) -> Result<u64, PlatformError> {
    if job.0.starts_with(STATUS_PREFIX) {
        return Err(PlatformError::Unsupported(
            "a commit status has no log or annotations on GitHub; follow its link".to_owned(),
        ));
    }
    job.0
        .parse()
        .map_err(|_| PlatformError::Unsupported(format!("{job} is not a GitHub check run id")))
}

impl GitHubWriter {
    /// The names of the Actions workflows that ran on `commit`, by check
    /// suite. Best effort: without Actions access the jobs are listed by
    /// their app's name.
    async fn workflows(&self, repo: &str, commit: &CommitSha) -> HashMap<u64, String> {
        let path = format!("/repos/{repo}/actions/runs?head_sha={}", commit.as_str());
        let runs = self
            .api()
            .get_all_under(&path, "workflow_runs")
            .await
            .unwrap_or_default();
        runs.iter()
            .filter_map(|run| {
                let suite = run.get("check_suite_id").and_then(Value::as_u64)?;
                Some((suite, text(run, "/name")?))
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl CiReader for GitHubWriter {
    async fn jobs(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
    ) -> Result<Vec<CiJob>, PlatformError> {
        let repo = target.repo.path();
        let sha = commit.as_str();
        let runs = self
            .api()
            .get_all_under(
                &format!("/repos/{repo}/commits/{sha}/check-runs?filter=latest"),
                "check_runs",
            )
            .await?;
        let workflows = if runs.is_empty() {
            HashMap::new()
        } else {
            self.workflows(&repo, commit).await
        };
        let statuses = self
            .api()
            .get_all_under(&format!("/repos/{repo}/commits/{sha}/status"), "statuses")
            .await?;
        Ok(runs
            .iter()
            .filter_map(|run| check_run(run, &workflows))
            .chain(statuses.iter().filter_map(commit_status))
            .filter(|job| job.name != CHECK_NAME)
            .collect())
    }

    async fn job_log(
        &self,
        target: &ReviewTarget,
        job: &CiJobId,
        max_bytes: usize,
    ) -> Result<CiLog, PlatformError> {
        let id = check_run_id(job)?;
        self.api()
            .get_text_tail(
                &format!("/repos/{}/actions/jobs/{id}/logs", target.repo.path()),
                max_bytes,
            )
            .await
    }

    async fn annotations(
        &self,
        target: &ReviewTarget,
        job: &CiJobId,
    ) -> Result<Vec<CiAnnotation>, PlatformError> {
        let id = check_run_id(job)?;
        let notes = self
            .api()
            .get_all(&format!(
                "/repos/{}/check-runs/{id}/annotations",
                target.repo.path()
            ))
            .await?;
        Ok(notes
            .iter()
            .map(|note| CiAnnotation {
                path: text(note, "/path").unwrap_or_default(),
                start_line: note.get("start_line").and_then(Value::as_u64).unwrap_or(0),
                end_line: note.get("end_line").and_then(Value::as_u64).unwrap_or(0),
                level: text(note, "/annotation_level").unwrap_or_else(|| "notice".to_owned()),
                title: text(note, "/title"),
                message: text(note, "/message").unwrap_or_default(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn a_check_run_takes_its_workflow_from_its_suite() {
        let workflows = HashMap::from([(7, "CI".to_owned())]);
        let job = check_run(
            &json!({"id": 1, "name": "test", "status": "completed", "conclusion": "failure",
                    "app": {"slug": "github-actions", "name": "GitHub Actions"},
                    "check_suite": {"id": 7}}),
            &workflows,
        )
        .unwrap();
        assert_eq!(job.workflow.as_deref(), Some("CI"));
        assert_eq!(job.conclusion, Some(CiConclusion::Failure));
        assert!(job.has_log);
        let other = check_run(
            &json!({"id": 2, "name": "codecov", "status": "queued", "conclusion": null,
                    "app": {"slug": "codecov", "name": "Codecov"}, "check_suite": {"id": 8}}),
            &workflows,
        )
        .unwrap();
        assert_eq!(other.workflow.as_deref(), Some("Codecov"));
        assert_eq!(other.conclusion, None);
        assert!(!other.has_log);
    }

    #[test]
    fn a_commit_status_has_no_log() {
        let job = commit_status(&json!({"context": "ci/jenkins", "state": "error"})).unwrap();
        assert_eq!(job.id.0, "status:ci/jenkins");
        assert_eq!(job.conclusion, Some(CiConclusion::Failure));
        assert!(matches!(
            check_run_id(&job.id),
            Err(PlatformError::Unsupported(_))
        ));
    }
}
