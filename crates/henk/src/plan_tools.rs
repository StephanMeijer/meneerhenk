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
use henk_domain::triage::TriageFields;
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
            checked_by: None,
            withdrawn: None,
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
planner_tool!(SetFields);
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
            description: "Sets the issue type where the tracker supports it (GitHub issue types such as Bug, Feature, Task; on GitLab issue or task). Costs 1 change.".to_owned(),
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
impl Tool for SetFields {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_fields"),
            description: "Fills empty triage fields on a GitLab issue or task: weight (the effort, a small integer), start_date and due_date (YYYY-MM-DD; the due date is the target date) and health (on_track, needs_attention, at_risk). A field that already has a value is left alone. Not available on GitHub. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({
                    "weight": {"type": "integer", "minimum": 0},
                    "start_date": {"type": "string"},
                    "due_date": {"type": "string"},
                    "health": {"type": "string", "enum": ["on_track", "needs_attention", "at_risk"]},
                    "reason": {"type": "string"}
                }),
                &["reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let Some(reason) = arg_str(&args, "reason") else {
            return ToolOutput::error("reason is required");
        };
        if ctx.writer.platform() == henk_domain::allowlist::Platform::GitHub {
            return ToolOutput::error(
                "Triage fields are not set on GitHub yet; skip them and mention estimates in the plan.",
            );
        }
        if args.get("weight").is_some_and(|w| w.as_u64().is_none()) {
            return ToolOutput::error("weight must be a whole number of zero or more");
        }
        let wanted = match TriageFields::parse(
            args.get("weight").and_then(Value::as_u64),
            arg_str(&args, "start_date"),
            arg_str(&args, "due_date"),
            arg_str(&args, "health"),
        ) {
            Ok(wanted) => wanted,
            Err(error) => return ToolOutput::error(format!("Not set: {error}.")),
        };
        let current = match ctx.writer.issue(&ctx.target).await {
            Ok(issue) => issue.fields,
            Err(error) => return ToolOutput::error(format!("Could not read the issue: {error}")),
        };
        if let Err(error) = wanted.may_fill(&current) {
            return ToolOutput::error(format!("Not set: {error}. Only empty fields are filled."));
        }
        if let Err(refusal) = ctx.spend(1, &format!("set {wanted} ({reason})")) {
            return refusal;
        }
        match ctx
            .writer
            .update_issue(
                &ctx.target,
                IssueUpdate {
                    fields: Some(wanted),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(()) => ToolOutput::ok(format!("Set {wanted}.")),
            Err(error) => ToolOutput::error(format!("Could not set the fields: {error}")),
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
        // A parent that cannot have children is refused before the budget
        // is spent, as nothing would be created.
        let parent = match ctx.writer.issue(&ctx.target).await {
            Ok(parent) => parent,
            Err(error) => return ToolOutput::error(format!("Could not read the issue: {error}")),
        };
        if let Err(error) = ctx.writer.may_have_children(&parent) {
            return ToolOutput::error(format!("Could not create the sub-issue: {error}"));
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
            .create_sub_issue(&ctx.target, title, &full_body)
            .await
        {
            Ok(created) => created,
            Err(error) => {
                return ToolOutput::error(format!("Could not create the sub-issue: {error}"));
            }
        };
        let issue = created.issue;
        if let Ok(mut state) = ctx.state.lock() {
            state.sub_issues.push(issue.number);
        }
        match created.unlinked {
            None => ToolOutput::ok(format!(
                "Created and linked sub-issue #{} ({}).",
                issue.number, issue.url
            )),
            Some(error) => {
                warn!(%error, "sub-issue created but not linked");
                ToolOutput::ok(format!(
                    "Created sub-issue #{} but could not link it: {error}",
                    issue.number
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

#[cfg(test)]
pub(crate) mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::triage::Health;
    use henk_platform::{CreatedSubIssue, IssueInfo, PlatformError, PostedComment};

    use super::*;

    /// An issue tracker in memory: one issue, the repository's labels, and
    /// every write it was asked for. Behaves like the platform it names.
    pub(crate) struct FakeIssueWriter {
        pub(crate) platform: Platform,
        pub(crate) issue: Mutex<IssueInfo>,
        pub(crate) repo_labels: Vec<String>,
        pub(crate) comments: Mutex<Vec<String>>,
        pub(crate) links: Mutex<Vec<(IssueRelation, u64)>>,
        pub(crate) created: Mutex<Vec<String>>,
        pub(crate) updates: Mutex<Vec<IssueUpdate>>,
        pub(crate) fail_writes: bool,
    }

    impl FakeIssueWriter {
        pub(crate) fn new(platform: Platform, kind: Option<&str>, body: &str) -> Self {
            Self {
                platform,
                issue: Mutex::new(IssueInfo {
                    id: Some(1),
                    number: 9,
                    title: "Export runs".to_owned(),
                    body: body.to_owned(),
                    open: true,
                    is_pull_request: false,
                    labels: Vec::new(),
                    url: "https://tracker.example/o/r/issues/9".to_owned(),
                    kind: kind.map(str::to_owned),
                    fields: TriageFields::default(),
                }),
                repo_labels: vec!["backend".to_owned(), "priority::high".to_owned()],
                comments: Mutex::default(),
                links: Mutex::default(),
                created: Mutex::default(),
                updates: Mutex::default(),
                fail_writes: false,
            }
        }

        fn write(&self) -> Result<(), PlatformError> {
            if self.fail_writes {
                return Err(PlatformError::Status {
                    status: 500,
                    body: "tracker down".to_owned(),
                });
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl IssueWriter for FakeIssueWriter {
        fn platform(&self) -> Platform {
            self.platform
        }

        async fn issue(&self, _: &IssueTarget) -> Result<IssueInfo, PlatformError> {
            Ok(self.issue.lock().unwrap().clone())
        }

        async fn update_issue(
            &self,
            _: &IssueTarget,
            update: IssueUpdate,
        ) -> Result<(), PlatformError> {
            self.write()?;
            if update.fields.is_some() && self.platform == Platform::GitHub {
                return Err(PlatformError::Unsupported("no fields on GitHub".to_owned()));
            }
            let mut issue = self.issue.lock().unwrap();
            if let Some(title) = &update.title {
                issue.title.clone_from(title);
            }
            if let Some(body) = &update.body {
                issue.body.clone_from(body);
            }
            if let Some(labels) = &update.labels {
                issue.labels.clone_from(labels);
            }
            if let Some(kind) = &update.issue_type {
                issue.kind = Some(kind.clone());
            }
            if let Some(fields) = update.fields {
                let current = &mut issue.fields;
                current.weight = current.weight.or(fields.weight);
                current.start = current.start.or(fields.start);
                current.due = current.due.or(fields.due);
                current.health = current.health.or(fields.health);
            }
            self.updates.lock().unwrap().push(update);
            Ok(())
        }

        async fn repo_labels(&self, _: &RepoRef) -> Result<Vec<String>, PlatformError> {
            Ok(self.repo_labels.clone())
        }

        async fn create_issue(
            &self,
            _: &RepoRef,
            title: &str,
            body: &str,
        ) -> Result<IssueInfo, PlatformError> {
            self.write()?;
            let mut created = self.created.lock().unwrap();
            created.push(title.to_owned());
            let number = 100 + u64::try_from(created.len()).unwrap();
            Ok(IssueInfo {
                id: Some(number),
                number,
                title: title.to_owned(),
                body: body.to_owned(),
                open: true,
                is_pull_request: false,
                labels: Vec::new(),
                url: format!("https://tracker.example/o/r/issues/{number}"),
                kind: None,
                fields: TriageFields::default(),
            })
        }

        fn may_have_children(&self, parent: &IssueInfo) -> Result<(), PlatformError> {
            if self.platform == Platform::GitLab && parent.kind.as_deref() == Some("Task") {
                return Err(PlatformError::Unsupported(
                    "a GitLab task cannot have children".to_owned(),
                ));
            }
            Ok(())
        }

        async fn create_sub_issue(
            &self,
            target: &IssueTarget,
            title: &str,
            body: &str,
        ) -> Result<CreatedSubIssue, PlatformError> {
            let parent = self.issue.lock().unwrap().clone();
            self.may_have_children(&parent)?;
            let issue = self.create_issue(&target.repo, title, body).await?;
            self.links
                .lock()
                .unwrap()
                .push((IssueRelation::SubIssue, issue.number));
            Ok(CreatedSubIssue {
                issue,
                unlinked: None,
            })
        }

        async fn link_issues(
            &self,
            _: &IssueTarget,
            relation: IssueRelation,
            other: u64,
        ) -> Result<(), PlatformError> {
            self.write()?;
            self.links.lock().unwrap().push((relation, other));
            Ok(())
        }

        async fn comment(
            &self,
            _: &IssueTarget,
            body: &str,
        ) -> Result<PostedComment, PlatformError> {
            self.write()?;
            let mut comments = self.comments.lock().unwrap();
            comments.push(body.to_owned());
            Ok(PostedComment {
                id: format!("n{}", comments.len()),
                node_id: None,
                url: String::new(),
            })
        }
    }

    pub(crate) fn context(writer: Arc<FakeIssueWriter>, budget: u32) -> Arc<PlanContext> {
        let platform = writer.platform;
        let repo = match platform {
            Platform::GitHub => "o/r",
            Platform::GitLab => "group/project",
        };
        Arc::new(PlanContext {
            run: RunId::parse("r-plan").unwrap(),
            model: ModelId::parse("planner-model").unwrap(),
            requester: Some(3),
            target: IssueTarget {
                repo: RepoRef::parse(platform, repo).unwrap(),
                number: 9,
            },
            writer,
            state: Mutex::new(PlanState {
                budget: ChangeBudget::new(budget),
                changes: Vec::new(),
                sub_issues: Vec::new(),
                plan: None,
                asked: false,
            }),
            sub_issue_cap: 2,
        })
    }

    fn gitlab(kind: &str) -> (Arc<FakeIssueWriter>, Arc<PlanContext>) {
        let writer = Arc::new(FakeIssueWriter::new(Platform::GitLab, Some(kind), "Body."));
        let ctx = context(Arc::clone(&writer), 20);
        (writer, ctx)
    }

    fn changes(ctx: &PlanContext) -> Vec<String> {
        ctx.state.lock().unwrap().changes.clone()
    }

    #[tokio::test]
    async fn set_fields_fills_empty_gitlab_fields_and_logs_the_change() {
        let (writer, ctx) = gitlab("Issue");
        let out = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 3, "due_date": "2026-11-30", "health": "on_track", "reason": "small change, due with the release"}))
            .await;
        assert!(!out.is_error, "{out:?}");
        let fields = writer.issue.lock().unwrap().fields;
        assert_eq!(fields.weight, Some(3));
        assert_eq!(
            fields.due.map(henk_domain::triage::date_text).as_deref(),
            Some("2026-11-30")
        );
        assert_eq!(fields.health, Some(Health::OnTrack));
        assert_eq!(
            changes(&ctx),
            ["set weight 3, due 2026-11-30, health on track (small change, due with the release)"]
        );
    }

    #[tokio::test]
    async fn set_fields_leaves_a_persons_value_alone_and_spends_nothing() {
        let (writer, ctx) = gitlab("Issue");
        writer.issue.lock().unwrap().fields.weight = Some(8);
        let out = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 3, "health": "at_risk", "reason": "r"}))
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("already set, left alone: weight"),
            "{}",
            out.content
        );
        assert!(writer.updates.lock().unwrap().is_empty());
        assert!(changes(&ctx).is_empty(), "a refusal costs nothing");
        assert_eq!(writer.issue.lock().unwrap().fields.weight, Some(8));
    }

    #[tokio::test]
    async fn set_fields_refuses_bad_values_and_github() {
        let (writer, ctx) = gitlab("Issue");
        for args in [
            json!({"due_date": "30-11-2026", "reason": "r"}),
            json!({"start_date": "2026-12-02", "due_date": "2026-12-01", "reason": "r"}),
            json!({"weight": -1, "reason": "r"}),
            json!({"health": "great", "reason": "r"}),
            json!({"reason": "r"}),
        ] {
            let out = SetFields(Arc::clone(&ctx)).call(args.clone()).await;
            assert!(out.is_error, "{args}");
        }
        assert!(writer.updates.lock().unwrap().is_empty());

        let github = Arc::new(FakeIssueWriter::new(Platform::GitHub, None, "Body."));
        let out = SetFields(context(Arc::clone(&github), 20))
            .call(json!({"weight": 3, "reason": "r"}))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("not set on GitHub"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_gitlab_task_gets_no_sub_issues_but_an_issue_does() {
        let (writer, ctx) = gitlab("Task");
        let args = json!({"title": "Export CSV", "body": "The CSV half.", "reason": "separable"});
        let out = CreateSubIssue(Arc::clone(&ctx)).call(args.clone()).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("cannot have children"),
            "{}",
            out.content
        );
        assert!(writer.created.lock().unwrap().is_empty());
        assert!(ctx.state.lock().unwrap().sub_issues.is_empty());
        assert!(changes(&ctx).is_empty(), "a refusal is not a change");
        assert_eq!(
            ctx.state.lock().unwrap().budget.remaining(),
            20,
            "and costs nothing"
        );

        let (writer, ctx) = gitlab("Issue");
        let out = CreateSubIssue(Arc::clone(&ctx)).call(args).await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            *writer.links.lock().unwrap(),
            [(IssueRelation::SubIssue, 101)]
        );
        assert_eq!(ctx.state.lock().unwrap().sub_issues, [101]);
    }

    #[tokio::test]
    async fn the_budget_runs_out_and_the_next_change_is_refused() {
        let writer = Arc::new(FakeIssueWriter::new(
            Platform::GitLab,
            Some("Issue"),
            "Body.",
        ));
        let ctx = context(Arc::clone(&writer), 1);
        let first = SetTitle(Arc::clone(&ctx))
            .call(json!({"title": "Export runs as CSV", "reason": "sharper"}))
            .await;
        assert!(!first.is_error, "{first:?}");
        let second = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 2, "reason": "r"}))
            .await;
        assert!(second.is_error);
        assert_eq!(
            writer.updates.lock().unwrap().len(),
            1,
            "only the title went out"
        );
    }

    #[tokio::test]
    async fn text_that_breaks_the_style_rules_never_reaches_the_tracker() {
        let (writer, ctx) = gitlab("Issue");
        let out = CreateSubIssue(Arc::clone(&ctx))
            .call(json!({"title": "Export \u{2014} CSV", "body": "Body.", "reason": "r"}))
            .await;
        assert!(out.is_error);
        let out = WritePlan(Arc::clone(&ctx))
            .call(json!({"markdown": "## Goal\n\nShip it \u{1f680}"}))
            .await;
        assert!(out.is_error);
        assert!(writer.created.lock().unwrap().is_empty());
        assert!(writer.updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_relation_goes_to_the_tracker_as_asked() {
        let (writer, ctx) = gitlab("Issue");
        let out = LinkIssue(Arc::clone(&ctx))
            .call(json!({"relation": "parent", "number": 4, "reason": "part of the epic"}))
            .await;
        assert!(!out.is_error, "{out:?}");
        let refused = LinkIssue(Arc::clone(&ctx))
            .call(json!({"relation": "blocks", "number": 9, "reason": "r"}))
            .await;
        assert!(refused.is_error, "an issue cannot relate to itself");
        assert_eq!(*writer.links.lock().unwrap(), [(IssueRelation::Parent, 4)]);
    }
}
