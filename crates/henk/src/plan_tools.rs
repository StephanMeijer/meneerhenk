//! The tools a planner gets besides the read-only MCP tools and `web_fetch`.
//! Every tracker change is drafted, not made (#328): it waits in the run
//! until the reviewer is done with the plan, and [`publish`] then makes
//! them all. The draft stays within the budget, every change stays within
//! the issue and its relations (§4, §8.5), and what was made is logged for
//! the session entry.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::plan::{
    ChangeBudget, PlanSection, body_without_plan, extract_plan, with_plan_section,
};
use henk_domain::plan_review::PlanStop;
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
    /// When the plan's time is up: from when it began, so a wait for a
    /// sandbox host counts against it (#403).
    pub deadline: tokio::time::Instant,
}

/// Mutable planner state.
#[derive(Debug)]
pub struct PlanState {
    /// What the draft may cost.
    pub budget: ChangeBudget,
    /// The changes and the plan, held until the review is done.
    pub draft: PlanDraft,
    /// What changed on the tracker, in words, for the session log: filled
    /// by [`publish`].
    pub changes: Vec<String>,
    /// Sub-issues created when the draft was published.
    pub sub_issues: Vec<u64>,
    /// The plan text once published.
    pub plan: Option<String>,
    /// Why the review of the draft stopped, set once when it has; `None`
    /// until then, and when the planner failed before the review began.
    pub stop: Option<PlanStop>,
}

impl PlanState {
    /// A state with nothing drafted.
    #[must_use]
    pub fn new(budget: ChangeBudget) -> Self {
        Self {
            budget,
            draft: PlanDraft::default(),
            changes: Vec::new(),
            sub_issues: Vec::new(),
            plan: None,
            stop: None,
        }
    }
}

/// One tracker change the planner drafted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A sharper title.
    Title(String),
    /// A new description; the plan section is kept.
    Description(String),
    /// Existing labels to add.
    Labels(Vec<String>),
    /// The issue type.
    IssueType(String),
    /// Empty triage fields to fill.
    Fields(TriageFields),
    /// A relationship to another issue.
    Link(IssueRelation, u64),
    /// A sub-issue to create.
    SubIssue {
        /// Its title.
        title: String,
        /// Its body.
        body: String,
    },
    /// The one comment with questions.
    Questions(String),
}

impl Change {
    /// What it costs of the budget.
    fn cost(&self) -> u32 {
        match self {
            Self::SubIssue { .. } => 2,
            _ => 1,
        }
    }

    /// The kind a later change of the same kind replaces, for a change
    /// there is only one of.
    fn single(&self) -> Option<std::mem::Discriminant<Self>> {
        matches!(
            self,
            Self::Title(_)
                | Self::Description(_)
                | Self::IssueType(_)
                | Self::Fields(_)
                | Self::Questions(_)
        )
        .then(|| std::mem::discriminant(self))
    }

    /// The order changes go to the tracker in: the issue itself first,
    /// then its relations, the sub-issues and the questions.
    fn order(&self) -> u8 {
        match self {
            Self::Title(_) => 0,
            Self::Description(_) => 1,
            Self::Labels(_) => 2,
            Self::IssueType(_) => 3,
            Self::Fields(_) => 4,
            Self::Link(..) => 5,
            Self::SubIssue { .. } => 6,
            Self::Questions(_) => 7,
        }
    }
}

/// A drafted change: its id, the change and, in words, what it does and
/// why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drafted {
    /// `c1`, `c2`, ...: what `withdraw_change` takes.
    pub id: String,
    /// The change.
    pub change: Change,
    /// What it does and why, for the reviewer and the session log.
    pub what: String,
}

/// The planner's draft: its changes and its plan, until the review is done
/// (#328).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanDraft {
    changes: Vec<Drafted>,
    next: u32,
    /// The plan's Markdown, once drafted.
    pub plan: Option<String>,
}

impl PlanDraft {
    /// What the draft costs of the budget.
    #[must_use]
    pub fn cost(&self) -> u32 {
        self.changes.iter().map(|c| c.change.cost()).sum()
    }

    /// The drafted changes, in the order drafted.
    #[must_use]
    pub fn changes(&self) -> &[Drafted] {
        &self.changes
    }

    /// Drafts `change`, replacing an earlier one of a kind there is only
    /// one of, when the draft then fits `budget`. Returns its id.
    ///
    /// # Errors
    ///
    /// The budget's refusal, in words; the draft is unchanged.
    pub fn put(
        &mut self,
        budget: ChangeBudget,
        change: Change,
        what: String,
    ) -> Result<String, String> {
        let replaced = change.single();
        let kept: u32 = self
            .changes
            .iter()
            .filter(|c| replaced.is_none() || c.change.single() != replaced)
            .map(|c| c.change.cost())
            .sum();
        budget
            .fits(kept + change.cost())
            .map_err(|e| format!("{e}; withdraw a change to make room"))?;
        if replaced.is_some() {
            self.changes.retain(|c| c.change.single() != replaced);
        }
        self.next += 1;
        let id = format!("c{}", self.next);
        self.changes.push(Drafted {
            id: id.clone(),
            change,
            what,
        });
        Ok(id)
    }

    /// Takes change `id` out of the draft, returning it.
    pub fn withdraw(&mut self, id: &str) -> Option<Drafted> {
        let at = self.changes.iter().position(|c| c.id == id.trim())?;
        Some(self.changes.remove(at))
    }

    /// The sub-issues drafted.
    #[must_use]
    pub fn sub_issues(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| matches!(c.change, Change::SubIssue { .. }))
            .count()
    }

    /// The draft as the reviewer reads it: every change, one a line, then
    /// the plan.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = String::from("Tracker changes, made once the review is done:");
        if self.changes.is_empty() {
            text.push_str(" none.");
        }
        for drafted in &self.changes {
            let _ = write!(text, "\n- {}: {}", drafted.id, drafted.what);
            if let Change::SubIssue { body, .. } = &drafted.change {
                let _ = write!(text, "\n  Its body:\n  {}", body.replace('\n', "\n  "));
            }
            if let Change::Questions(body) | Change::Description(body) = &drafted.change {
                let _ = write!(text, "\n  The text:\n  {}", body.replace('\n', "\n  "));
            }
        }
        match &self.plan {
            Some(plan) => {
                let _ = write!(text, "\n\nThe plan:\n\n{plan}");
            }
            None => text.push_str("\n\nNo plan is drafted."),
        }
        text
    }
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

    /// Drafts `change` or returns the refusal for the model.
    fn draft(&self, change: Change, what: String) -> ToolOutput {
        let Ok(mut state) = self.state.lock() else {
            return ToolOutput::error("planner state unavailable");
        };
        let budget = state.budget;
        match state.draft.put(budget, change, what) {
            Ok(id) => ToolOutput::ok(format!(
                "Drafted as {id}. It goes to the tracker once the review is done. The draft costs {} of {} changes.",
                state.draft.cost(),
                budget.limit()
            )),
            Err(refusal) => ToolOutput::error(refusal),
        }
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
planner_tool!(WithdrawChange);

#[async_trait::async_trait]
impl Tool for SetTitle {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_title"),
            description:
                "Drafts a sharper issue title, replacing one drafted before. Costs 1 change."
                    .to_owned(),
            input_schema: object_schema(
                &json!({"title": {"type": "string"}, "reason": {"type": "string"}}),
                &["title", "reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let (Some(title), Some(reason)) = (arg_str(&args, "title"), arg_str(&args, "reason"))
        else {
            return ToolOutput::error("title and reason are required");
        };
        if let Some(error) = PlanContext::style_error(title) {
            return error;
        }
        self.0.draft(
            Change::Title(title.to_owned()),
            format!("retitled to {title:?} ({reason})"),
        )
    }
}

#[async_trait::async_trait]
impl Tool for SetDescription {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_description"),
            description: "Drafts a new issue description, replacing one drafted before; the plan section is kept. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"description": {"type": "string"}, "reason": {"type": "string"}}), &["description", "reason"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let (Some(description), Some(reason)) =
            (arg_str(&args, "description"), arg_str(&args, "reason"))
        else {
            return ToolOutput::error("description and reason are required");
        };
        if let Some(error) = PlanContext::style_error(description) {
            return error;
        }
        self.0.draft(
            Change::Description(description.to_owned()),
            format!("rewrote the description ({reason})"),
        )
    }
}

#[async_trait::async_trait]
impl Tool for AddLabels {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("add_labels"),
            description: "Drafts adding existing repository labels to the issue. Unknown labels are refused. Costs 1 change.".to_owned(),
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
        let what = format!("added labels {} ({reason})", wanted.join(", "));
        ctx.draft(Change::Labels(wanted), what)
    }
}

#[async_trait::async_trait]
impl Tool for SetIssueType {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_issue_type"),
            description: "Drafts the issue type where the tracker supports it (GitHub issue types such as Bug, Feature, Task; on GitLab issue or task), replacing one drafted before. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"type": {"type": "string"}, "reason": {"type": "string"}}), &["type", "reason"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let (Some(kind), Some(reason)) = (arg_str(&args, "type"), arg_str(&args, "reason")) else {
            return ToolOutput::error("type and reason are required");
        };
        self.0.draft(
            Change::IssueType(kind.to_owned()),
            format!("set type {kind} ({reason})"),
        )
    }
}

#[async_trait::async_trait]
impl Tool for SetFields {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("set_fields"),
            description: "Drafts filling empty triage fields on a GitLab issue or task: weight (the effort, a small integer), start_date and due_date (YYYY-MM-DD; the due date is the target date) and health (on_track, needs_attention, at_risk). A field that already has a value is left alone. Replaces fields drafted before. Not available on GitHub. Costs 1 change.".to_owned(),
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
        ctx.draft(Change::Fields(wanted), format!("set {wanted} ({reason})"))
    }
}

#[async_trait::async_trait]
impl Tool for LinkIssue {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("link_issue"),
            description: "Drafts a relationship between this issue and another issue in the same repository: parent, sub_issue, blocks, blocked_by or relates_to. Costs 1 change.".to_owned(),
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
        ctx.draft(
            Change::Link(relation, number),
            format!("linked {relation:?} #{number} ({reason})"),
        )
    }
}

#[async_trait::async_trait]
impl Tool for CreateSubIssue {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("create_sub_issue"),
            description: "Drafts a sub-issue of this issue for clearly separable work. Costs 2 changes. At most a few per plan.".to_owned(),
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
            if u32::try_from(state.draft.sub_issues()).unwrap_or(u32::MAX) >= ctx.sub_issue_cap {
                return ToolOutput::error(format!(
                    "At most {} sub-issues per plan; that cap is reached. Withdraw one to draft another.",
                    ctx.sub_issue_cap
                ));
            }
        }
        // A parent that cannot have children is refused now, as nothing
        // would be created.
        let parent = match ctx.writer.issue(&ctx.target).await {
            Ok(parent) => parent,
            Err(error) => return ToolOutput::error(format!("Could not read the issue: {error}")),
        };
        if let Err(error) = ctx.writer.may_have_children(&parent) {
            return ToolOutput::error(format!("Could not create the sub-issue: {error}"));
        }
        ctx.draft(
            Change::SubIssue {
                title: title.to_owned(),
                body: body.to_owned(),
            },
            format!("created sub-issue {title:?} ({reason})"),
        )
    }
}

#[async_trait::async_trait]
impl Tool for AskQuestions {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("ask_questions"),
            description: "Drafts the one comment with the questions whose answers would change the plan, replacing one drafted before. Costs 1 change.".to_owned(),
            input_schema: object_schema(
                &json!({"body": {"type": "string"}}), &["body"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let Some(body) = arg_str(&args, "body") else {
            return ToolOutput::error("body is required");
        };
        if let Some(error) = PlanContext::style_error(body) {
            return error;
        }
        self.0.draft(
            Change::Questions(body.to_owned()),
            "asked questions in a comment".to_owned(),
        )
    }
}

#[async_trait::async_trait]
impl Tool for WritePlan {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("write_plan"),
            description: "Drafts the plan, replacing the one drafted before. Once the review is done it goes into the issue description under a folded heading, replacing any earlier plan. Free of budget.".to_owned(),
            input_schema: object_schema(
                &json!({"markdown": {"type": "string"}}), &["markdown"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let Some(markdown) = arg_str(&args, "markdown") else {
            return ToolOutput::error("markdown is required");
        };
        if let Some(error) = PlanContext::style_error(markdown) {
            return error;
        }
        let Ok(mut state) = self.0.state.lock() else {
            return ToolOutput::error("planner state unavailable");
        };
        state.draft.plan = Some(markdown.to_owned());
        ToolOutput::ok("Plan drafted. It goes into the issue once the review is done.")
    }
}

#[async_trait::async_trait]
impl Tool for WithdrawChange {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("withdraw_change"),
            description: "Takes a drafted tracker change out of the draft, such as c3; its cost comes back to the budget.".to_owned(),
            input_schema: object_schema(
                &json!({"change": {"type": "string"}, "reason": {"type": "string"}}),
                &["change", "reason"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let (Some(id), Some(_)) = (arg_str(&args, "change"), arg_str(&args, "reason")) else {
            return ToolOutput::error("change and reason are required");
        };
        let Ok(mut state) = self.0.state.lock() else {
            return ToolOutput::error("planner state unavailable");
        };
        match state.draft.withdraw(id) {
            Some(drafted) => ToolOutput::ok(format!("Withdrawn: {}.", drafted.what)),
            None => ToolOutput::error(format!("There is no drafted change {id}.")),
        }
    }
}

/// Makes the draft's changes on the tracker, then writes its plan with
/// `open_note` under it, when there is one: what the review left open
/// (#328). What was made goes into the state's `changes`, and a change the
/// tracker refused says so there; only the plan failing fails.
///
/// # Errors
///
/// When there is no plan to write, or the tracker refused it.
pub async fn publish(ctx: &PlanContext, open_note: &str) -> anyhow::Result<()> {
    let draft = ctx
        .state
        .lock()
        .map(|s| s.draft.clone())
        .map_err(|_| anyhow::anyhow!("planner state unavailable"))?;
    let Some(plan) = draft.plan.clone() else {
        return Err(anyhow::anyhow!("the model ended without writing a plan"));
    };
    let mut changes = draft.changes().to_vec();
    changes.sort_by_key(|c| c.change.order());
    for drafted in changes {
        let made = make(ctx, &drafted.change).await;
        let Ok(mut state) = ctx.state.lock() else {
            continue;
        };
        match made {
            Ok(created) => {
                state.sub_issues.extend(created);
                state.changes.push(drafted.what);
            }
            Err(error) => {
                warn!(%error, change = %drafted.id, "the tracker refused a drafted change");
                state
                    .changes
                    .push(format!("could not make {}: {error}", drafted.what));
            }
        }
    }
    let markdown = if open_note.is_empty() {
        plan
    } else {
        format!("{plan}\n\n{open_note}")
    };
    let current = ctx
        .writer
        .issue(&ctx.target)
        .await
        .map_err(|e| anyhow::anyhow!("could not read the issue: {e}"))?
        .body;
    let sessions = extract_plan(&current)
        .map(|s| s.sessions)
        .unwrap_or_default();
    let section = PlanSection {
        plan: markdown.clone(),
        sessions,
    };
    let body = with_plan_section(&body_without_plan(&current), &section);
    ctx.writer
        .update_issue(
            &ctx.target,
            IssueUpdate {
                body: Some(body),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("could not write the plan: {e}"))?;
    if let Ok(mut state) = ctx.state.lock() {
        state.plan = Some(markdown);
    }
    info!(issue = ctx.target.number, "plan written");
    Ok(())
}

/// Makes one drafted change on the tracker; a sub-issue returns its
/// number.
async fn make(ctx: &PlanContext, change: &Change) -> anyhow::Result<Option<u64>> {
    let update = |update: IssueUpdate| async move {
        ctx.writer
            .update_issue(&ctx.target, update)
            .await
            .map_err(anyhow::Error::from)
    };
    match change {
        Change::Title(title) => {
            update(IssueUpdate {
                title: Some(title.clone()),
                ..Default::default()
            })
            .await?;
        }
        Change::Description(description) => {
            let current = ctx.writer.issue(&ctx.target).await?.body;
            let body = match extract_plan(&current) {
                Some(section) => with_plan_section(description, &section),
                None => description.clone(),
            };
            update(IssueUpdate {
                body: Some(body),
                ..Default::default()
            })
            .await?;
        }
        Change::Labels(wanted) => {
            let mut labels = ctx.writer.issue(&ctx.target).await?.labels;
            for w in wanted {
                if !labels.iter().any(|l| l.eq_ignore_ascii_case(w)) {
                    labels.push(w.clone());
                }
            }
            update(IssueUpdate {
                labels: Some(labels),
                ..Default::default()
            })
            .await?;
        }
        Change::IssueType(kind) => {
            update(IssueUpdate {
                issue_type: Some(kind.clone()),
                ..Default::default()
            })
            .await?;
        }
        Change::Fields(fields) => {
            // A person may have filled a field since the draft; theirs stays.
            let current = ctx.writer.issue(&ctx.target).await?.fields;
            fields.may_fill(&current)?;
            update(IssueUpdate {
                fields: Some(*fields),
                ..Default::default()
            })
            .await?;
        }
        Change::Link(relation, number) => {
            ctx.writer
                .link_issues(&ctx.target, *relation, *number)
                .await?;
        }
        Change::SubIssue { title, body } => {
            let full_body = ctx.marker(MarkerKind::Plan).attach(&format!(
                "{body}\n\nSplit off from #{} by Meneer Henk.",
                ctx.target.number
            ));
            let created = ctx
                .writer
                .create_sub_issue(&ctx.target, title, &full_body)
                .await?;
            if let Some(error) = created.unlinked {
                warn!(%error, "sub-issue created but not linked");
            }
            return Ok(Some(created.issue.number));
        }
        Change::Questions(body) => {
            let full = ctx.marker(MarkerKind::Plan).attach(body);
            ctx.writer.comment(&ctx.target, &full).await?;
        }
    }
    Ok(None)
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
                    retry_after: None,
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
            state: Mutex::new(PlanState::new(ChangeBudget::new(budget))),
            sub_issue_cap: 2,
            deadline: tokio::time::Instant::now() + std::time::Duration::from_mins(10),
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

    fn cost(ctx: &PlanContext) -> u32 {
        ctx.state.lock().unwrap().draft.cost()
    }

    /// Nothing reached the tracker.
    fn untouched(writer: &FakeIssueWriter) -> bool {
        writer.updates.lock().unwrap().is_empty()
            && writer.created.lock().unwrap().is_empty()
            && writer.links.lock().unwrap().is_empty()
            && writer.comments.lock().unwrap().is_empty()
    }

    async fn plan_and_publish(ctx: &Arc<PlanContext>, note: &str) {
        let out = WritePlan(Arc::clone(ctx))
            .call(json!({"markdown": "## Goal\n\nExport runs."}))
            .await;
        assert!(!out.is_error, "{out:?}");
        publish(ctx, note).await.unwrap();
    }

    #[tokio::test]
    async fn set_fields_fills_empty_gitlab_fields_once_published_and_logs_the_change() {
        let (writer, ctx) = gitlab("Issue");
        let out = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 3, "due_date": "2026-11-30", "health": "on_track", "reason": "small change, due with the release"}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("Drafted as c1"), "{}", out.content);
        assert!(untouched(&writer), "a draft stays in the run");
        plan_and_publish(&ctx, "").await;
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
    async fn set_fields_leaves_a_persons_value_alone_and_drafts_nothing() {
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
        assert_eq!(cost(&ctx), 0, "a refusal costs nothing");
        assert!(untouched(&writer));
        assert_eq!(writer.issue.lock().unwrap().fields.weight, Some(8));
    }

    #[tokio::test]
    async fn set_fields_leaves_a_value_a_person_filled_after_the_draft_alone() {
        let (writer, ctx) = gitlab("Issue");
        let out = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 3, "health": "on_track", "reason": "small change"}))
            .await;
        assert!(!out.is_error, "{out:?}");
        writer.issue.lock().unwrap().fields.weight = Some(8);
        plan_and_publish(&ctx, "").await;
        let fields = writer.issue.lock().unwrap().fields;
        assert_eq!(fields.weight, Some(8), "the person's weight stays");
        assert_eq!(
            fields.health, None,
            "the change is made whole or not at all"
        );
        assert!(
            changes(&ctx)
                .iter()
                .any(|c| c.starts_with("could not make set weight 3")
                    && c.contains("already set, left alone: weight")),
            "{:?}",
            changes(&ctx)
        );
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
        assert_eq!(cost(&ctx), 0);
        assert!(untouched(&writer));

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
        assert_eq!(cost(&ctx), 0, "a refusal is not drafted");

        let (writer_ok, ctx) = gitlab("Issue");
        let out = CreateSubIssue(Arc::clone(&ctx)).call(args).await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(cost(&ctx), 2);
        assert!(untouched(&writer_ok), "created once published");
        plan_and_publish(&ctx, "").await;
        assert_eq!(
            *writer_ok.links.lock().unwrap(),
            [(IssueRelation::SubIssue, 101)]
        );
        assert_eq!(ctx.state.lock().unwrap().sub_issues, [101]);
        assert!(writer.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_budget_bounds_the_draft_and_a_withdrawal_makes_room() {
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
        let again = SetTitle(Arc::clone(&ctx))
            .call(json!({"title": "Export runs as CSV files", "reason": "sharper still"}))
            .await;
        assert!(
            !again.is_error,
            "a title replaces the one drafted: {again:?}"
        );
        let second = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 2, "reason": "r"}))
            .await;
        assert!(second.is_error);
        assert!(
            second.content.contains("withdraw a change"),
            "{}",
            second.content
        );
        let withdrawn = WithdrawChange(Arc::clone(&ctx))
            .call(json!({"change": "c2", "reason": "the weight matters more"}))
            .await;
        assert!(!withdrawn.is_error, "{withdrawn:?}");
        let third = SetFields(Arc::clone(&ctx))
            .call(json!({"weight": 2, "reason": "r"}))
            .await;
        assert!(!third.is_error, "{third:?}");
        plan_and_publish(&ctx, "").await;
        assert_eq!(
            writer.issue.lock().unwrap().title,
            "Export runs",
            "withdrawn"
        );
        assert_eq!(writer.issue.lock().unwrap().fields.weight, Some(2));
    }

    #[tokio::test]
    async fn text_that_breaks_the_style_rules_is_never_drafted() {
        let (writer, ctx) = gitlab("Issue");
        let out = CreateSubIssue(Arc::clone(&ctx))
            .call(json!({"title": "Export \u{2014} CSV", "body": "Body.", "reason": "r"}))
            .await;
        assert!(out.is_error);
        let out = WritePlan(Arc::clone(&ctx))
            .call(json!({"markdown": "## Goal\n\nShip it \u{1f680}"}))
            .await;
        assert!(out.is_error);
        assert_eq!(cost(&ctx), 0);
        assert!(ctx.state.lock().unwrap().draft.plan.is_none());
        assert!(untouched(&writer));
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
        plan_and_publish(&ctx, "").await;
        assert_eq!(*writer.links.lock().unwrap(), [(IssueRelation::Parent, 4)]);
    }

    #[tokio::test]
    async fn the_published_plan_carries_the_open_points_and_the_last_questions() {
        let (writer, ctx) = gitlab("Issue");
        for body in ["Which format?", "Which format, CSV or JSON?"] {
            let out = AskQuestions(Arc::clone(&ctx))
                .call(json!({ "body": body }))
                .await;
            assert!(!out.is_error, "{out:?}");
        }
        assert_eq!(cost(&ctx), 1, "the questions replace the earlier ones");
        let draft = ctx.state.lock().unwrap().draft.describe();
        assert!(
            draft.contains("- c2: asked questions in a comment"),
            "{draft}"
        );
        assert!(draft.contains("Which format, CSV or JSON?"), "{draft}");
        plan_and_publish(
            &ctx,
            "### Points still open after review\n\n- step 1 (x): y",
        )
        .await;
        let comments = writer.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert!(comments[0].contains("CSV or JSON"), "{comments:?}");
        let body = writer.issue.lock().unwrap().body.clone();
        assert!(body.contains("Export runs."), "{body}");
        assert!(body.contains("Points still open after review"), "{body}");
    }

    #[tokio::test]
    async fn a_refused_change_is_logged_and_only_a_refused_plan_fails() {
        let writer = Arc::new(FakeIssueWriter {
            fail_writes: true,
            ..FakeIssueWriter::new(Platform::GitLab, Some("Issue"), "Body.")
        });
        let ctx = context(Arc::clone(&writer), 5);
        let out = LinkIssue(Arc::clone(&ctx))
            .call(json!({"relation": "parent", "number": 4, "reason": "epic"}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert!(publish(&ctx, "").await.is_err(), "no plan drafted");
        WritePlan(Arc::clone(&ctx))
            .call(json!({"markdown": "## Goal"}))
            .await;
        let error = publish(&ctx, "").await.unwrap_err();
        assert!(
            error.to_string().contains("could not write the plan"),
            "{error}"
        );
        assert!(
            changes(&ctx)
                .iter()
                .any(|c| c.starts_with("could not make linked Parent #4")),
            "{:?}",
            changes(&ctx)
        );
    }
}
