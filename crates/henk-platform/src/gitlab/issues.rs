//! GitLab issues for the planner, through the write-mode MCP session.

use henk_domain::allowlist::{Platform, RepoRef};
use serde_json::{Value, json};

use crate::error::PlatformError;
use crate::gitlab::writer::GitLabWriter;
use crate::issue::{IssueInfo, IssueRelation, IssueTarget, IssueUpdate, IssueWriter};
use crate::writer::PostedComment;

fn info(value: &Value) -> Result<IssueInfo, PlatformError> {
    let number = value
        .get("iid")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::Decode("issue without iid".to_owned()))?;
    Ok(IssueInfo {
        id: value.get("id").and_then(Value::as_u64),
        number,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        body: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        open: value.get("state").and_then(Value::as_str) == Some("opened"),
        is_pull_request: false,
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| {
                        l.as_str()
                            .map(str::to_owned)
                            .or_else(|| l.get("name").and_then(Value::as_str).map(str::to_owned))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        url: value
            .get("web_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    })
}

#[async_trait::async_trait]
impl IssueWriter for GitLabWriter {
    fn platform(&self) -> Platform {
        Platform::GitLab
    }

    async fn issue(&self, target: &IssueTarget) -> Result<IssueInfo, PlatformError> {
        let value = self
            .call_tool("get_issue", json!({"project_id": target.repo.path(), "issue_iid": target.number.to_string(), "full_response": true}))
            .await?;
        info(&value)
    }

    async fn update_issue(
        &self,
        target: &IssueTarget,
        update: IssueUpdate,
    ) -> Result<(), PlatformError> {
        let mut args = serde_json::Map::new();
        args.insert("project_id".into(), json!(target.repo.path()));
        args.insert("issue_iid".into(), json!(target.number.to_string()));
        let mut changed = false;
        if let Some(title) = update.title {
            args.insert("title".into(), json!(title));
            changed = true;
        }
        if let Some(body) = update.body {
            args.insert("description".into(), json!(body));
            changed = true;
        }
        if let Some(labels) = update.labels {
            args.insert("labels".into(), json!(labels));
            changed = true;
        }
        if let Some(issue_type) = update.issue_type {
            args.insert("issue_type".into(), json!(issue_type));
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        self.call_tool("update_issue", Value::Object(args)).await?;
        Ok(())
    }

    async fn repo_labels(&self, repo: &RepoRef) -> Result<Vec<String>, PlatformError> {
        let value = self
            .call_tool(
                "list_labels",
                json!({"project_id": repo.path(), "per_page": 100}),
            )
            .await?;
        let items = match value {
            Value::Array(items) => items,
            Value::Object(map) => map
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(items
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
            .call_tool(
                "create_issue",
                json!({"project_id": repo.path(), "title": title, "description": body}),
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
        let link_type = match relation {
            IssueRelation::RelatesTo => "relates_to",
            IssueRelation::Blocks => "blocks",
            IssueRelation::BlockedBy => "is_blocked_by",
            IssueRelation::Parent | IssueRelation::SubIssue => {
                return Err(PlatformError::Decode(
                    "GitLab parent and child links are not supported yet; use blocks, blocked_by or relates_to".to_owned(),
                ));
            }
        };
        self.call_tool(
            "create_issue_link",
            json!({
                "project_id": target.repo.path(),
                "issue_iid": target.number.to_string(),
                "target_project_id": target.repo.path(),
                "target_issue_iid": other.to_string(),
                "link_type": link_type,
            }),
        )
        .await?;
        Ok(())
    }

    async fn comment(
        &self,
        target: &IssueTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let created = self
            .call_tool("create_issue_note", json!({"project_id": target.repo.path(), "issue_iid": target.number.to_string(), "body": body}))
            .await?;
        let id = match created.get("id") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => return Err(PlatformError::Decode("note without id".to_owned())),
        };
        Ok(PostedComment {
            id,
            node_id: None,
            url: String::new(),
        })
    }
}
