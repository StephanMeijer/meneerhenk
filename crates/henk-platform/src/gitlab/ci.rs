//! A merge request's CI on GitLab (#420): the jobs of its pipeline for a
//! commit, and a job's log. Through REST, so it needs the GitLab token.

use henk_domain::review::CommitSha;
use serde_json::Value;

use crate::ci::{CiConclusion, CiJob, CiJobId, CiLog, CiReader, CiStatus};
use crate::error::PlatformError;
use crate::gitlab::rest::path_segment;
use crate::gitlab::writer::{GitLabWriter, STATUS_NAME};
use crate::writer::ReviewTarget;

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A job's status as where it is and how it ended.
fn status(status: &str, allow_failure: bool) -> (CiStatus, Option<CiConclusion>) {
    match status {
        "running" => (CiStatus::InProgress, None),
        "success" => (CiStatus::Completed, Some(CiConclusion::Success)),
        "failed" if allow_failure => (CiStatus::Completed, Some(CiConclusion::AllowedFailure)),
        "failed" => (CiStatus::Completed, Some(CiConclusion::Failure)),
        "canceled" | "canceling" => (CiStatus::Completed, Some(CiConclusion::Cancelled)),
        // A manual job waits for a person, not for the pipeline: it will
        // not start on its own, so it is not "still running".
        "skipped" | "manual" => (CiStatus::Completed, Some(CiConclusion::Skipped)),
        // created, pending, preparing, scheduled, waiting_for_resource,
        // waiting_for_callback
        _ => (CiStatus::Queued, None),
    }
}

fn job(job: &Value, pipeline: u64) -> Option<CiJob> {
    let id = job.get("id").and_then(Value::as_u64)?;
    let name = text(job, "name")?;
    let allow_failure = job
        .get("allow_failure")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (status, conclusion) = status(
        job.get("status").and_then(Value::as_str).unwrap_or(""),
        allow_failure,
    );
    let workflow = match text(job, "stage") {
        Some(stage) => format!("pipeline {pipeline}, stage {stage}"),
        None => format!("pipeline {pipeline}"),
    };
    Some(CiJob {
        id: CiJobId(id.to_string()),
        name,
        workflow: Some(workflow),
        status,
        conclusion,
        started_at: text(job, "started_at"),
        completed_at: text(job, "finished_at"),
        url: text(job, "web_url"),
        has_log: true,
    })
}

fn job_id(job: &CiJobId) -> Result<u64, PlatformError> {
    job.0
        .parse()
        .map_err(|_| PlatformError::Unsupported(format!("{job} is not a GitLab job id")))
}

#[async_trait::async_trait]
impl CiReader for GitLabWriter {
    async fn jobs(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
    ) -> Result<Vec<CiJob>, PlatformError> {
        let rest = self.rest()?;
        let project = path_segment(&target.repo.path());
        // Newest first: the latest pipeline that ran on the commit.
        let pipelines = rest
            .get_all(&format!(
                "/projects/{project}/merge_requests/{}/pipelines",
                target.number
            ))
            .await?;
        let Some(pipeline) = pipelines
            .iter()
            .filter(|p| p.get("sha").and_then(Value::as_str) == Some(commit.as_str()))
            .filter_map(|p| p.get("id").and_then(Value::as_u64))
            .max()
        else {
            return Ok(Vec::new());
        };
        let jobs = rest
            .get_all(&format!(
                "/projects/{project}/pipelines/{pipeline}/jobs?include_retried=false"
            ))
            .await?;
        Ok(jobs
            .iter()
            .filter_map(|j| job(j, pipeline))
            .filter(|j| j.name != STATUS_NAME)
            .collect())
    }

    async fn job_log(
        &self,
        target: &ReviewTarget,
        job: &CiJobId,
        max_bytes: usize,
    ) -> Result<CiLog, PlatformError> {
        let id = job_id(job)?;
        let project = path_segment(&target.repo.path());
        self.rest()?
            .get_text_tail(&format!("/projects/{project}/jobs/{id}/trace"), max_bytes)
            .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn a_job_says_where_it_is_and_how_it_ended() {
        let failed = job(
            &json!({"id": 9, "name": "test", "stage": "test", "status": "failed"}),
            4,
        )
        .unwrap();
        assert_eq!(failed.conclusion, Some(CiConclusion::Failure));
        assert_eq!(failed.workflow.as_deref(), Some("pipeline 4, stage test"));
        let manual = job(&json!({"id": 10, "name": "deploy", "status": "manual"}), 4).unwrap();
        assert_eq!(
            (manual.status, manual.conclusion),
            (CiStatus::Completed, Some(CiConclusion::Skipped)),
            "a manual job waits for a person, not for the pipeline"
        );
        let pending = job(&json!({"id": 11, "name": "build", "status": "pending"}), 4).unwrap();
        assert_eq!(
            (pending.status, pending.conclusion),
            (CiStatus::Queued, None)
        );
    }

    #[test]
    fn a_failure_the_pipeline_allows_is_not_a_failure() {
        let allowed = job(
            &json!({"id": 12, "name": "lint", "status": "failed", "allow_failure": true}),
            4,
        )
        .unwrap();
        assert_eq!(allowed.conclusion, Some(CiConclusion::AllowedFailure));
        let blocking = job(
            &json!({"id": 13, "name": "test", "status": "failed", "allow_failure": false}),
            4,
        )
        .unwrap();
        assert_eq!(blocking.conclusion, Some(CiConclusion::Failure));
    }
}
