//! The tools a planner gets besides the read-only MCP tools and `web_fetch`.
//! Every tracker change spends budget, stays within the issue and its
//! relations (§4, §8.5), and is logged for the session entry.

use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::plan::{
    ChangeBudget, PlanSection, body_without_plan, extract_plan, with_plan_section,
};
use henk_domain::run::RunId;
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use henk_platform::{IssueRelation, IssueTarget, IssueUpdate, IssueWriter};
use serde_json::{Value, json};
use tracing::{info, warn};

/// What all planner tools share.
pub struct PlanContext {
    /// The run.
    pub run: RunId,
    /// The model, for markers.
    pub model: ModelId,
    /// Who asked, for markers.
    pub requester: Option<u64>,
    /// The issue.
    pub target: IssueTarget,
    /// The tracker.
    pub writer: Arc<dyn IssueWriter>,
    /// Mutable state.
    pub state: Mutex<PlanState>,
    /// Sub-issues allowed.
    pub sub_issue_cap: u32,
}

/// Mutable planner state.
#[derive(Debug)]
pub struct PlanState {
    /// Changes left.
    pub budget: ChangeBudget,
    /// What changed, in words, for the session log.
    pub changes: Vec<String>,
    /// Sub-issues created so far.
    pub sub_issues: Vec<u64>,
    /// The plan text once written.
    pub plan: Option<String>,
    /// Whether questions were asked.
    pub asked: bool,
}

impl PlanContext {
    fn marker(&self, kind: MarkerKind) -> Marker {
        Marker {
            run: self.run.clone(),
            model: self.model.clone(),
            requested_by: self
                .requester
                .map(henk_domain::identity::DiscordUserId::new),
            kind: Some(kind),
        }
    }

    /// Spends budget or returns the refusal for the model.
    fn spend(&self, cost: u32, what: &str) -> Result<(), ToolOutput> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ToolOutput::error("planner state unavailable"))?;
        state
            .budget
            .spend(cost)
            .map_err(|e| ToolOutput::error(e.to_string()))?;
        state.changes.push(what.to_owned());
        Ok(())
    }

    fn style_error(body: &str) -> Option<ToolOutput> {
        let violations = style_violations(body);
        (!violations.is_empty()).then(|| {
            ToolOutput::error(format!(
                "The text breaks the style rules ({} problem(s): no emoji, no em-dash). Rewrite it.",
                violations.len()
            ))
        })
    }
}

fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn object_schema(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

macro_rules! planner_tool {
    ($name:ident) => {
        /// A planner tool.
        pub struct $name(pub Arc<PlanContext>);
    };
}

planner_tool!(SetTitle);
planner_tool!(SetDescription);
planner_tool!(AddLabels);
planner_tool!(SetIssueType);
planner_tool!(LinkIssue);
planner_tool!(CreateSubIssue);
planner_tool!(AskQuestions);
planner_tool!(WritePlan);

#[async_trait::async_trait]
impl Tool for SetTitle {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_title"),
            description: "Sharpens the issue title. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"title": {"type": "string"}, "reason": {"type": "string"}}),
                &["title", "reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(title), Some(reason)) = (arg_str(&args, "title"), arg_str(&args, "reason"))
        else {
            return ToolOutput::error("title and reason are required");
        };
        if let Some(error) = PlanContext::style_error(title) {
            return error;
        }
        if let Err(refusal) = ctx.spend(1, &format!("retitled to {title:?} ({reason})")) {
            return refusal;
        }
        match ctx
            .writer
            .update_issue(
                &ctx.target,
                IssueUpdate {
                    title: Some(title.to_owned()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => ToolOutput::ok("Title updated."),
            Err(error) => ToolOutput::error(format!("Could not update the title: {error}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SetDescription {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_description"),
            description: "Replaces the issue description, except the plan section, which is kept. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"description": {"type": "string"}, "reason": {"type": "string"}}), &["description", "reason"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(description), Some(reason)) =
            (arg_str(&args, "description"), arg_str(&args, "reason"))
        else {
            return ToolOutput::error("description and reason are required");
        };
        if let Some(error) = PlanContext::style_error(description) {
            return error;
        }
        let current = match ctx.writer.issue(&ctx.target).await {
            Ok(issue) => issue.body,
            Err(error) => return ToolOutput::error(format!("Could not read the issue: {error}")),
        };
        let body = match extract_plan(&current) {
            Some(section) => with_plan_section(description, &section),
            None => description.to_owned(),
        };
        if let Err(refusal) = ctx.spend(1, &format!("rewrote the description ({reason})")) {
            return refusal;
        }
        match ctx
            .writer
            .update_issue(
                &ctx.target,
                IssueUpdate {
                    body: Some(body),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => ToolOutput::ok("Description updated; the plan section was kept."),
            Err(error) => ToolOutput::error(format!("Could not update the description: {error}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for AddLabels {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("add_labels"),
            description: "Adds existing repository labels to the issue. Unknown labels are refused. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"labels": {"type": "array", "items": {"type": "string"}}, "reason": {"type": "string"}}),
                &["labels", "reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let wanted: Vec<String> = args
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let Some(reason) = arg_str(&args, "reason") else {
            return ToolOutput::error("reason is required");
        };
        if wanted.is_empty() {
            return ToolOutput::error("labels must not be empty");
        }
        let existing = match ctx.writer.repo_labels(&ctx.target.repo).await {
            Ok(labels) => labels,
            Err(error) => return ToolOutput::error(format!("Could not list labels: {error}")),
        };
        let unknown: Vec<&str> = wanted
            .iter()
            .filter(|w| !existing.iter().any(|e| e.eq_ignore_ascii_case(w)))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return ToolOutput::error(format!(
                "These labels do not exist in the repository: {}. Existing labels: {}",
                unknown.join(", "),
                existing.join(", ")
            ));
        }
        let current = match ctx.writer.issue(&ctx.target).await {
            Ok(issue) => issue.labels,
            Err(error) => return ToolOutput::error(format!("Could not read the issue: {error}")),
        };
        let mut labels = current;
        for w in &wanted {
            if !labels.iter().any(|l| l.eq_ignore_ascii_case(w)) {
                labels.push(w.clone());
            }
        }
        if let Err(refusal) =
            ctx.spend(1, &format!("added labels {} ({reason})", wanted.join(", ")))
        {
            return refusal;
        }
        match ctx
            .writer
            .update_issue(
                &ctx.target,
                IssueUpdate {
                    labels: Some(labels),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => ToolOutput::ok("Labels added."),
            Err(error) => ToolOutput::error(format!("Could not update labels: {error}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SetIssueType {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_issue_type"),
            description: "Sets the issue type where the tracker supports it (GitHub issue types such as Bug, Feature, Task; GitLab issue, incident, task). Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"type": {"type": "string"}, "reason": {"type": "string"}}), &["type", "reason"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(kind), Some(reason)) = (arg_str(&args, "type"), arg_str(&args, "reason")) else {
            return ToolOutput::error("type and reason are required");
        };
        if let Err(refusal) = ctx.spend(1, &format!("set type {kind} ({reason})")) {
            return refusal;
        }
        match ctx
            .writer
            .update_issue(
                &ctx.target,
                IssueUpdate {
                    issue_type: Some(kind.to_owned()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => ToolOutput::ok("Type set."),
            Err(error) => ToolOutput::error(format!("Could not set the type: {error}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for LinkIssue {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("link_issue"),
            description: "Registers a relationship between this issue and another issue in the same repository: parent, sub_issue, blocks, blocked_by or relates_to. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({
                    "relation": {"type": "string", "enum": ["parent", "sub_issue", "blocks", "blocked_by", "relates_to"]},
                    "number": {"type": "integer"},
                    "reason": {"type": "string"}
                }),
                &["relation", "number", "reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(relation), Some(reason)) = (arg_str(&args, "relation"), arg_str(&args, "reason"))
        else {
            return ToolOutput::error("relation and reason are required");
        };
        let Some(relation) = IssueRelation::parse(relation) else {
            return ToolOutput::error(
                "relation must be parent, sub_issue, blocks, blocked_by or relates_to",
            );
        };
        let Some(number) = args
            .get("number")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        else {
            return ToolOutput::error("number must be a positive integer");
        };
        if number == ctx.target.number {
            return ToolOutput::error("an issue cannot relate to itself");
        }
        if let Err(refusal) = ctx.spend(1, &format!("linked {relation:?} #{number} ({reason})")) {
            return refusal;
        }
        match ctx.writer.link_issues(&ctx.target, relation, number).await {
            Ok(()) => ToolOutput::ok("Linked."),
            Err(error) => ToolOutput::error(format!("Could not link: {error}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for CreateSubIssue {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("create_sub_issue"),
            description: "Creates a sub-issue of this issue for clearly separable work. Costs 2 changes. At most a few per plan.".to_owned(),
            input_schema: object_schema(
                &json!({"title": {"type": "string"}, "body": {"type": "string"}, "reason": {"type": "string"}}),
                &["title", "body", "reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(title), Some(body), Some(reason)) = (
            arg_str(&args, "title"),
            arg_str(&args, "body"),
            arg_str(&args, "reason"),
        ) else {
            return ToolOutput::error("title, body and reason are required");
        };
        if let Some(error) =
            PlanContext::style_error(title).or_else(|| PlanContext::style_error(body))
        {
            return error;
        }
        {
            let Ok(state) = ctx.state.lock() else {
                return ToolOutput::error("planner state unavailable");
            };
            if u32::try_from(state.sub_issues.len()).unwrap_or(u32::MAX) >= ctx.sub_issue_cap {
                return ToolOutput::error(format!(
                    "At most {} sub-issues per plan; that cap is reached.",
                    ctx.sub_issue_cap
                ));
            }
        }
        if let Err(refusal) = ctx.spend(2, &format!("created sub-issue {title:?} ({reason})")) {
            return refusal;
        }
        let full_body = ctx.marker(MarkerKind::Plan).attach(&format!(
            "{body}\n\nSplit off from #{} by Meneer Henk.",
            ctx.target.number
        ));
        let created = match ctx
            .writer
            .create_issue(&ctx.target.repo, title, &full_body)
            .await
        {
            Ok(created) => created,
            Err(error) => {
                return ToolOutput::error(format!("Could not create the sub-issue: {error}"));
            }
        };
        if let Ok(mut state) = ctx.state.lock() {
            state.sub_issues.push(created.number);
        }
        match ctx
            .writer
            .link_issues(&ctx.target, IssueRelation::SubIssue, created.number)
            .await
        {
            Ok(()) => ToolOutput::ok(format!(
                "Created and linked sub-issue #{} ({}).",
                created.number, created.url
            )),
            Err(error) => {
                warn!(%error, "sub-issue created but not linked");
                ToolOutput::ok(format!(
                    "Created sub-issue #{} but could not link it: {error}",
                    created.number
                ))
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for AskQuestions {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("ask_questions"),
            description: "Posts one comment with the questions whose answers would change the plan. Use once. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"body": {"type": "string"}}), &["body"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let Some(body) = arg_str(&args, "body") else {
            return ToolOutput::error("body is required");
        };
        if let Some(error) = PlanContext::style_error(body) {
            return error;
        }
        {
            let Ok(state) = ctx.state.lock() else {
                return ToolOutput::error("planner state unavailable");
            };
            if state.asked {
                return ToolOutput::error(
                    "Questions were already asked in this session; put the rest in the plan's open questions.",
                );
            }
        }
        if let Err(refusal) = ctx.spend(1, "asked questions in a comment") {
            return refusal;
        }
        let full = ctx.marker(MarkerKind::Plan).attach(body);
        match ctx.writer.comment(&ctx.target, &full).await {
            Ok(_) => {
                if let Ok(mut state) = ctx.state.lock() {
                    state.asked = true;
                }
                ToolOutput::ok("Questions posted.")
            }
            Err(error) => ToolOutput::error(format!("Could not post the questions: {error}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WritePlan {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("write_plan"),
            description: "Writes the plan into the issue description under a folded heading, replacing any earlier plan. Free of budget. Call it once, last.".to_owned(),
            input_schema: object_schema(
                &json!({"markdown": {"type": "string"}}), &["markdown"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let Some(markdown) = arg_str(&args, "markdown") else {
            return ToolOutput::error("markdown is required");
        };
        if let Some(error) = PlanContext::style_error(markdown) {
            return error;
        }
        let current = match ctx.writer.issue(&ctx.target).await {
            Ok(issue) => issue.body,
            Err(error) => return ToolOutput::error(format!("Could not read the issue: {error}")),
        };
        let sessions = extract_plan(&current)
            .map(|s| s.sessions)
            .unwrap_or_default();
        let section = PlanSection {
            plan: markdown.to_owned(),
            sessions,
        };
        let body = with_plan_section(&body_without_plan(&current), &section);
        match ctx
            .writer
            .update_issue(
                &ctx.target,
                IssueUpdate {
                    body: Some(body),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => {
                if let Ok(mut state) = ctx.state.lock() {
                    state.plan = Some(markdown.to_owned());
                }
                info!(issue = ctx.target.number, "plan written");
                ToolOutput::ok("Plan written into the issue.")
            }
            Err(error) => ToolOutput::error(format!("Could not write the plan: {error}")),
        }
    }
}
