//! GitLab issues for the planner, as work items through the write-mode MCP
//! session: issues and tasks, their hierarchy, links and triage fields (§4).

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::triage::{Health, TriageFields, date_text, parse_date};
use serde_json::{Value, json};

use crate::error::PlatformError;
use crate::gitlab::writer::GitLabWriter;
use crate::issue::{
    CreatedSubIssue, IssueInfo, IssueRelation, IssueTarget, IssueUpdate, IssueWriter,
};
use crate::writer::PostedComment;

/// A number GitLab may send as a JSON number or, from GraphQL, as a string.
fn number(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// The fields a work item has now. A value Henk cannot read counts as empty.
fn fields(value: &Value) -> TriageFields {
    let date = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(|d| parse_date("date", d).ok())
    };
    TriageFields {
        weight: number(value.get("weight")).and_then(|w| u32::try_from(w).ok()),
        start: date("startDate"),
        due: date("dueDate"),
        health: value
            .get("healthStatus")
            .and_then(Value::as_str)
            .and_then(Health::parse),
    }
}

/// A work item as `get_work_item` or `create_work_item` returns it.
fn info(value: &Value) -> Result<IssueInfo, PlatformError> {
    let number = number(value.get("iid"))
        .ok_or_else(|| PlatformError::Decode("work item without iid".to_owned()))?;
    let state = text(value, "state");
    Ok(IssueInfo {
        // A global id, "gid://gitlab/WorkItem/123".
        id: value
            .get("id")
            .and_then(Value::as_str)
            .and_then(|gid| gid.rsplit('/').next())
            .and_then(|n| n.parse().ok()),
        number,
        title: text(value, "title"),
        body: text(value, "description"),
        open: state.eq_ignore_ascii_case("open") || state.eq_ignore_ascii_case("opened"),
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
                            .or_else(|| l.get("title").and_then(Value::as_str).map(str::to_owned))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        url: text(value, "webUrl"),
        kind: value.get("type").and_then(Value::as_str).map(str::to_owned),
        fields: fields(value),
    })
}

/// The work item types a planner may set.
fn work_item_type(wanted: &str) -> Result<&'static str, PlatformError> {
    match wanted.trim().to_ascii_lowercase().as_str() {
        "issue" => Ok("issue"),
        "task" => Ok("task"),
        other => Err(PlatformError::Unsupported(format!(
            "GitLab work item type {other:?}: a planner sets issue or task"
        ))),
    }
}

/// What a child of a `parent` work item is, if it may have children.
fn child_type(parent: Option<&str>) -> Result<&'static str, PlatformError> {
    match parent.map(str::to_ascii_lowercase).as_deref() {
        Some("issue") | None => Ok("task"),
        Some("epic") => Ok("issue"),
        Some(other) => Err(PlatformError::Unsupported(format!(
            "a GitLab {other} cannot have children; put the work in the plan instead"
        ))),
    }
}

impl GitLabWriter {
    async fn work_item(&self, target: &IssueTarget) -> Result<IssueInfo, PlatformError> {
        let value = self
            .call_tool(
                "get_work_item",
                json!({"project_id": target.repo.path(), "iid": target.number}),
            )
            .await?;
        info(&value)
    }
}

#[async_trait::async_trait]
impl IssueWriter for GitLabWriter {
    fn platform(&self) -> Platform {
        Platform::GitLab
    }

    async fn issue(&self, target: &IssueTarget) -> Result<IssueInfo, PlatformError> {
        self.work_item(target).await
    }

    async fn update_issue(
        &self,
        target: &IssueTarget,
        update: IssueUpdate,
    ) -> Result<(), PlatformError> {
        if let Some(kind) = &update.issue_type {
            let new_type = work_item_type(kind)?;
            self.call_tool(
                "convert_work_item_type",
                json!({"project_id": target.repo.path(), "iid": target.number, "new_type": new_type}),
            )
            .await?;
        }

        let mut args = serde_json::Map::new();
        if let Some(title) = update.title {
            args.insert("title".into(), json!(title));
        }
        if let Some(body) = update.body {
            args.insert("description".into(), json!(body));
        }
        if let Some(wanted) = update.labels {
            // Work items take labels to add and to remove, not a full set.
            let current = self.work_item(target).await?.labels;
            let add: Vec<&String> = wanted.iter().filter(|l| !current.contains(l)).collect();
            let remove: Vec<&String> = current.iter().filter(|l| !wanted.contains(l)).collect();
            if !add.is_empty() {
                args.insert("add_labels".into(), json!(add));
            }
            if !remove.is_empty() {
                args.insert("remove_labels".into(), json!(remove));
            }
        }
        if let Some(fields) = update.fields {
            if let Some(weight) = fields.weight {
                args.insert("weight".into(), json!(weight));
            }
            if let Some(start) = fields.start {
                args.insert("start_date".into(), json!(date_text(start)));
            }
            if let Some(due) = fields.due {
                args.insert("due_date".into(), json!(date_text(due)));
            }
            if let Some(health) = fields.health {
                args.insert("health_status".into(), json!(health.gitlab_name()));
            }
        }
        if args.is_empty() {
            return Ok(());
        }
        args.insert("project_id".into(), json!(target.repo.path()));
        args.insert("iid".into(), json!(target.number));
        self.call_tool("update_work_item", Value::Object(args))
            .await?;
        Ok(())
    }

    async fn repo_labels(&self, repo: &RepoRef) -> Result<Vec<String>, PlatformError> {
        let mut args = serde_json::Map::new();
        args.insert("project_id".into(), json!(repo.path()));
        let items = self.call_tool_all("list_labels", args, &["items"]).await?;
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
                "create_work_item",
                json!({"project_id": repo.path(), "title": title, "description": body, "type": "issue"}),
            )
            .await?;
        // The create answer is short: no description or state. It is new and open.
        Ok(IssueInfo {
            body: body.to_owned(),
            open: true,
            ..info(&created)?
        })
    }

    fn may_have_children(&self, parent: &IssueInfo) -> Result<(), PlatformError> {
        child_type(parent.kind.as_deref()).map(|_| ())
    }

    async fn create_sub_issue(
        &self,
        target: &IssueTarget,
        title: &str,
        body: &str,
    ) -> Result<CreatedSubIssue, PlatformError> {
        let parent = self.work_item(target).await?;
        // Checked again here: the caller's read may be stale.
        let kind = child_type(parent.kind.as_deref())?;
        let created = self
            .call_tool(
                "create_work_item",
                json!({
                    "project_id": target.repo.path(),
                    "title": title,
                    "description": body,
                    "type": kind,
                    "parent_iid": target.number,
                }),
            )
            .await?;
        Ok(CreatedSubIssue {
            issue: IssueInfo {
                body: body.to_owned(),
                open: true,
                ..info(&created)?
            },
            unlinked: None,
        })
    }

    async fn link_issues(
        &self,
        target: &IssueTarget,
        relation: IssueRelation,
        other: u64,
    ) -> Result<(), PlatformError> {
        let mut args = serde_json::Map::new();
        args.insert("project_id".into(), json!(target.repo.path()));
        args.insert("iid".into(), json!(target.number));
        match relation {
            IssueRelation::Parent => {
                args.insert("parent_iid".into(), json!(other));
            }
            IssueRelation::SubIssue => {
                args.insert("children_to_add".into(), json!([{"iid": other}]));
            }
            IssueRelation::Blocks | IssueRelation::BlockedBy | IssueRelation::RelatesTo => {
                let link_type = match relation {
                    IssueRelation::Blocks => "BLOCKS",
                    IssueRelation::BlockedBy => "BLOCKED_BY",
                    _ => "RELATED",
                };
                args.insert(
                    "linked_items_to_add".into(),
                    json!([{"iid": other, "link_type": link_type}]),
                );
            }
        }
        self.call_tool("update_work_item", Value::Object(args))
            .await?;
        Ok(())
    }

    async fn comment(
        &self,
        target: &IssueTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let created = self
            .call_tool(
                "create_work_item_note",
                json!({"project_id": target.repo.path(), "iid": target.number, "body": body}),
            )
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
