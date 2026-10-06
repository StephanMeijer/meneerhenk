//! GitHub issues for the planner, through REST.

use henk_domain::allowlist::{Platform, RepoRef};
use serde_json::{Value, json};

use henk_domain::triage::TriageFields;

use crate::error::PlatformError;
use crate::github::writer::GitHubWriter;
use crate::issue::{IssueInfo, IssueRelation, IssueTarget, IssueUpdate, IssueWriter};
use crate::writer::PostedComment;

fn info(value: &Value) -> Result<IssueInfo, PlatformError> {
    let number = value
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::Decode("issue without number".to_owned()))?;
    Ok(IssueInfo {
        id: value.get("id").and_then(Value::as_u64),
        number,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        open: value.get("state").and_then(Value::as_str) == Some("open"),
        is_pull_request: value.get("pull_request").is_some_and(|v| !v.is_null()),
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        url: value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        kind: value
            .get("type")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        fields: TriageFields::default(),
    })
}

async fn issue_id(
    api: &crate::github::GitHubApi,
    repo: &str,
    number: u64,
) -> Result<u64, PlatformError> {
    let value = api.get(&format!("/repos/{repo}/issues/{number}")).await?;
    value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::Decode(format!("issue {number} without id")))
}

#[async_trait::async_trait]
impl IssueWriter for GitHubWriter {
    fn platform(&self) -> Platform {
        Platform::GitHub
    }

    async fn issue(&self, target: &IssueTarget) -> Result<IssueInfo, PlatformError> {
        let value = self
            .api()
            .get(&format!(
                "/repos/{}/issues/{}",
                target.repo.path(),
                target.number
            ))
            .await?;
        info(&value)
    }

    async fn update_issue(
        &self,
        target: &IssueTarget,
        update: IssueUpdate,
    ) -> Result<(), PlatformError> {
        if update.fields.is_some() {
            return Err(PlatformError::Unsupported(
                "GitHub triage fields (priority, effort, target date) are not set by Henk yet"
                    .to_owned(),
            ));
        }
        let mut body = serde_json::Map::new();
        if let Some(title) = update.title {
            body.insert("title".into(), json!(title));
        }
        if let Some(text) = update.body {
            body.insert("body".into(), json!(text));
        }
        if let Some(labels) = update.labels {
            body.insert("labels".into(), json!(labels));
        }
        if let Some(issue_type) = update.issue_type {
            body.insert("type".into(), json!(issue_type));
        }
        if body.is_empty() {
            return Ok(());
        }
        self.api()
            .patch(
                &format!("/repos/{}/issues/{}", target.repo.path(), target.number),
                &Value::Object(body),
            )
            .await?;
        Ok(())
    }

    async fn repo_labels(&self, repo: &RepoRef) -> Result<Vec<String>, PlatformError> {
        let labels = self
            .api()
            .get_all(&format!("/repos/{}/labels", repo.path()))
            .await?;
        Ok(labels
            .iter()
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }

    async fn create_issue(
        &self,
        repo: &RepoRef,
        title: &str,
        body: &str,
    ) -> Result<IssueInfo, PlatformError> {
        let created = self
            .api()
            .post(
                &format!("/repos/{}/issues", repo.path()),
                &json!({"title": title, "body": body}),
            )
            .await?;
        info(&created)
    }

    async fn link_issues(
        &self,
        target: &IssueTarget,
        relation: IssueRelation,
        other: u64,
    ) -> Result<(), PlatformError> {
        let repo = target.repo.path();
        let id_of = |number: u64| issue_id(self.api(), &repo, number);
        match relation {
            IssueRelation::SubIssue => {
                let child = id_of(other).await?;
                self.api()
                    .post(
                        &format!("/repos/{repo}/issues/{}/sub_issues", target.number),
                        &json!({"sub_issue_id": child}),
                    )
                    .await?;
            }
            IssueRelation::Parent => {
                let child = id_of(target.number).await?;
                self.api()
                    .post(
                        &format!("/repos/{repo}/issues/{other}/sub_issues"),
                        &json!({"sub_issue_id": child}),
                    )
                    .await?;
            }
            IssueRelation::BlockedBy => {
                let blocker = id_of(other).await?;
                self.api()
                    .post(
                        &format!(
                            "/repos/{repo}/issues/{}/dependencies/blocked_by",
                            target.number
                        ),
                        &json!({"issue_id": blocker}),
                    )
                    .await?;
            }
            IssueRelation::Blocks => {
                let blocker = id_of(target.number).await?;
                self.api()
                    .post(
                        &format!("/repos/{repo}/issues/{other}/dependencies/blocked_by"),
                        &json!({"issue_id": blocker}),
                    )
                    .await?;
            }
            IssueRelation::RelatesTo => {
                return Err(PlatformError::Unsupported(
                    "GitHub has no 'relates to' relationship; mention the issue in the plan instead".to_owned(),
                ));
            }
        }
        Ok(())
    }

    async fn comment(
        &self,
        target: &IssueTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let created = self
            .api()
            .post(
                &format!(
                    "/repos/{}/issues/{}/comments",
                    target.repo.path(),
                    target.number
                ),
                &json!({"body": body}),
            )
            .await?;
        let id = created
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode("comment without id".to_owned()))?;
        Ok(PostedComment {
            id: id.to_string(),
            node_id: created
                .get("node_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            url: created
                .get("html_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        })
    }
}
