//! A second model checks every finding before it is posted (§3.2).
//!
//! Lanes are wrong in a recognisable way: they claim a file lacks something
//! it has, or call intended behaviour a bug. The check is a short session of
//! its own with a different model, the read-only diff tools and one tool to
//! give its verdict. A rejected finding is not posted; the lane is told why
//! and may correct it. When no check can be run, the finding goes out
//! unchecked and the run records that: Henk is advisory, and an outage of
//! the checking model must not silence reviews.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{AgentConfig, Continuation, EndReason, Tool, ToolOutput, ToolSet};
use henk_domain::diff::ReviewDiff;
use henk_domain::marker::ModelId;
use henk_domain::review::LaneName;
use henk_domain::run::RunId;
use henk_llm::{ChatMessage, ModelClient, ToolDef, ToolName};
use henk_platform::DiffSide;
use henk_session::{SessionSpec, model_id, run_session};
use henk_store::RunStore;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::review_tools::{DiffFiles, GetFileDiff, ListChangedFiles, ReadFile};

/// What is being checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckKind {
    /// A new finding; the text is the finding.
    NewFinding,
    /// A rewrite of an existing finding; the text is the new version.
    Rewrite {
        /// The finding as it stands.
        current: String,
    },
    /// Withdrawing a finding as wrong; the text is the reason.
    Withdrawal {
        /// The finding to withdraw.
        finding: String,
    },
}

/// One claim to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRequest {
    /// The lane making the claim.
    pub lane: LaneName,
    /// The model behind that lane; it does not check itself when another
    /// model is configured.
    pub lane_model: ModelId,
    /// What is being checked.
    pub kind: CheckKind,
    /// File of the finding.
    pub path: String,
    /// Line of the finding.
    pub line: u32,
    /// Side of the line.
    pub side: DiffSide,
    /// The finding, the rewrite or the reason, per `kind`.
    pub text: String,
}

/// The outcome of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckVerdict {
    /// The claim holds.
    Confirmed {
        /// The checking model.
        by: ModelId,
        /// Why.
        reason: String,
    },
    /// The claim does not hold.
    Rejected {
        /// The checking model.
        by: ModelId,
        /// What is wrong with it, for the lane.
        reason: String,
    },
    /// No model gave a verdict.
    Unavailable {
        /// Why not, for the lane and the run.
        why: String,
    },
}

/// Something that checks claims. The review uses [`SessionFactCheck`];
/// tests use fixed answers.
#[async_trait::async_trait]
pub trait FactCheck: Send + Sync {
    /// Checks one claim.
    async fn check(&self, request: &CheckRequest) -> CheckVerdict;
}

/// Checks each claim in a session of its own on the run.
pub struct SessionFactCheck {
    /// Run records; each check is a session row named `check-<lane>-<n>`.
    pub store: Arc<dyn RunStore>,
    /// The run.
    pub run: RunId,
    /// The checking model first, then the backup, if any.
    pub models: Vec<Arc<dyn ModelClient>>,
    /// The diff of the review.
    pub diff: Arc<ReviewDiff>,
    /// The guarded file read at the reviewed commit, when the platform has one.
    pub file_reader: Option<Arc<dyn Tool>>,
    /// The checker's own workspace at the reviewed commit, when the review
    /// has them (#170). With it, the code tools read there instead of the
    /// platform (#90). Checks only read, so they share it.
    pub workspace: Option<Arc<dyn crate::workspace::Workspace>>,
    /// The rendered system prompt, without skills.
    pub system: String,
    /// The skills the checker may load.
    pub skills: crate::skill_tools::AgentSkills,
    /// Turn and time limits per check.
    pub limits: AgentConfig,
    /// The review's cancellation.
    pub cancel: CancellationToken,
    /// Numbers the check sessions of the run.
    pub sequence: AtomicU32,
}

impl SessionFactCheck {
    /// The models to try, in order: never the lane's own model first when
    /// there is another.
    fn order(&self, lane_model: &ModelId) -> Vec<Arc<dyn ModelClient>> {
        let mut models = self.models.clone();
        if models.len() > 1
            && models
                .first()
                .is_some_and(|m| model_id(m.as_ref()) == *lane_model)
        {
            models.rotate_left(1);
        }
        models
    }

    async fn check_with(
        &self,
        model: Arc<dyn ModelClient>,
        request: &CheckRequest,
    ) -> Option<(bool, String)> {
        let number = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let slot = Arc::new(Mutex::new(None));
        let files = Arc::new(DiffFiles::new(Arc::clone(&self.diff)));
        let mut tools = ToolSet::new();
        tools.add(ListChangedFiles(Arc::clone(&files)));
        tools.add(GetFileDiff(files));
        match (&self.workspace, &self.file_reader) {
            (Some(workspace), _) => crate::code_tools::add(&mut tools, workspace),
            (None, Some(inner)) => {
                tools.add(ReadFile {
                    inner: Arc::clone(inner),
                });
            }
            (None, None) => {}
        }
        tools.add(GiveVerdict(Arc::clone(&slot)));
        let mut system = self.system.clone();
        crate::skill_tools::equip(&mut system, &mut tools, self.skills.clone());
        let spec = SessionSpec {
            name: format!("check-{}-{number}", request.lane),
            model,
            system,
            opening: vec![ChatMessage::user(opening(request, &self.diff))],
            tools,
            limits: self.limits,
            continuation: Some(ask_for_verdict(Arc::clone(&slot))),
            turn_warning: None,
        };
        let outcome = run_session(self.store.as_ref(), &self.run, spec, self.cancel.clone()).await;
        let verdict = slot.lock().ok().and_then(|v| v.clone());
        if verdict.is_none() {
            warn!(stop = ?outcome.stop, lane = %request.lane, "fact-check ended without a verdict");
        }
        verdict
    }
}

#[async_trait::async_trait]
impl FactCheck for SessionFactCheck {
    async fn check(&self, request: &CheckRequest) -> CheckVerdict {
        let mut tried = Vec::new();
        for model in self.order(&request.lane_model) {
            if self.cancel.is_cancelled() {
                break;
            }
            let by = model_id(model.as_ref());
            if let Some((confirmed, reason)) = self.check_with(model, request).await {
                let word = if confirmed { "confirmed" } else { "rejected" };
                info!(lane = %request.lane, path = %request.path, line = request.line, %by, verdict = word, "fact-check");
                let _ = self
                    .store
                    .event(
                        &self.run,
                        "info",
                        &format!(
                            "{}: {} {} on {}:{} ({by}): {reason}",
                            request.lane,
                            word,
                            kind_name(&request.kind),
                            request.path,
                            request.line
                        ),
                    )
                    .await;
                return if confirmed {
                    CheckVerdict::Confirmed { by, reason }
                } else {
                    CheckVerdict::Rejected { by, reason }
                };
            }
            tried.push(by.to_string());
        }
        let why = if tried.is_empty() {
            "the review was cancelled".to_owned()
        } else {
            format!("no verdict from {}", tried.join(" or "))
        };
        let _ = self
            .store
            .event(
                &self.run,
                "warn",
                &format!(
                    "{}: {} on {}:{} went out unchecked: {why}",
                    request.lane,
                    kind_name(&request.kind),
                    request.path,
                    request.line
                ),
            )
            .await;
        CheckVerdict::Unavailable { why }
    }
}

fn kind_name(kind: &CheckKind) -> &'static str {
    match kind {
        CheckKind::NewFinding => "finding",
        CheckKind::Rewrite { .. } => "rewrite",
        CheckKind::Withdrawal { .. } => "withdrawal",
    }
}

/// The opening message: the claim, fenced as material, and the numbered
/// diff of its file.
fn opening(request: &CheckRequest, diff: &ReviewDiff) -> String {
    let side = match request.side {
        DiffSide::Left => " (old side, a removed line)",
        DiffSide::Right => "",
    };
    let mut out = String::new();
    let place = format!("{}:{}{side}", request.path, request.line);
    let _ = match &request.kind {
        CheckKind::NewFinding => write!(
            out,
            "Check this new finding on {place}.\n\nFinding:\n```text\n{}\n```\n",
            request.text
        ),
        CheckKind::Rewrite { current } => write!(
            out,
            "Check this rewrite of the finding on {place}.\n\nThe finding now:\n```text\n{current}\n```\n\nProposed text:\n```text\n{}\n```\n",
            request.text
        ),
        CheckKind::Withdrawal { finding } => write!(
            out,
            "Check whether the finding on {place} should be withdrawn as wrong.\n\nThe finding:\n```text\n{finding}\n```\n\nReason given for withdrawing it:\n```text\n{}\n```\n",
            request.text
        ),
    };
    match diff.file(&request.path) {
        Some(file) => {
            let _ = write!(out, "\nThe diff of {}:\n{}", request.path, file.render());
        }
        None => {
            let _ = write!(
                out,
                "\n{} is not part of this change; the changed files are:\n{}",
                request.path,
                diff.render_list()
            );
        }
    }
    out
}

/// Asks once for the verdict when the model ends its turn without one.
fn ask_for_verdict(slot: Arc<Mutex<Option<(bool, String)>>>) -> Continuation {
    let asked = std::sync::atomic::AtomicBool::new(false);
    Box::new(move |ending| {
        let missing = slot.lock().is_ok_and(|v| v.is_none());
        (ending.reason == EndReason::EndTurn && missing && !asked.swap(true, Ordering::SeqCst))
            .then(|| "You ended without a verdict. Call give_verdict with confirmed or rejected and a reason.".to_owned())
    })
}

/// `give_verdict`: the one thing a check produces.
struct GiveVerdict(Arc<Mutex<Option<(bool, String)>>>);

#[async_trait::async_trait]
impl Tool for GiveVerdict {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("give_verdict")
                .unwrap_or_else(|_| unreachable!("a constant tool name")),
            description: "Records your verdict on the claim, once: confirmed if it holds, rejected if it does not, with the evidence as path:line.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "verdict": {"type": "string", "enum": ["confirmed", "rejected"]},
                    "reason": {"type": "string", "description": "A few plain sentences citing the code as path:line"}
                },
                "required": ["verdict", "reason"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let confirmed = match args.get("verdict").and_then(Value::as_str) {
            Some("confirmed") => true,
            Some("rejected") => false,
            _ => return ToolOutput::error("verdict must be \"confirmed\" or \"rejected\""),
        };
        let Some(reason) = args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
        else {
            return ToolOutput::error("reason is required");
        };
        let Ok(mut slot) = self.0.lock() else {
            return ToolOutput::error("verdict unavailable");
        };
        if slot.is_some() {
            return ToolOutput::error("A verdict is already recorded. End your turn.");
        }
        *slot = Some((confirmed, reason.to_owned()));
        ToolOutput::ok("Recorded. End your turn now.")
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unnecessary_wraps
    )]

    use std::time::Duration;

    use henk_domain::allowlist::Platform;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{Completion, StopReason, ToolArguments, ToolCall, Usage};

    use super::*;

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }
";

    async fn store() -> (Arc<dyn RunStore>, RunId) {
        let store = Arc::new(henk_store::SqliteStore::in_memory().unwrap());
        let run = RunId::parse("r-1").unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run.clone(),
                kind: henk_domain::run::RunKind::Review,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "test".into(),
                link: "l".into(),
            })
            .await
            .unwrap();
        (store, run)
    }

    fn verdict_call(verdict: &str, reason: &str) -> Result<Completion, henk_llm::LlmError> {
        Ok(Completion {
            message: ChatMessage {
                role: henk_llm::Role::Assistant,
                blocks: vec![henk_llm::Block::ToolCall(ToolCall {
                    id: "v1".into(),
                    name: "give_verdict".into(),
                    arguments: ToolArguments::Parsed(json!({"verdict": verdict, "reason": reason})),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        })
    }

    fn done() -> Result<Completion, henk_llm::LlmError> {
        Ok(Completion {
            message: ChatMessage::assistant("Done."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    async fn checker(models: Vec<Arc<dyn ModelClient>>) -> SessionFactCheck {
        let (store, run) = store().await;
        SessionFactCheck {
            store,
            run,
            models,
            diff: Arc::new(ReviewDiff::from_unified(DIFF)),
            file_reader: None,
            workspace: None,
            system: "check".into(),
            skills: crate::skill_tools::AgentSkills::new(),
            limits: AgentConfig {
                max_turns: 6,
                timeout: Duration::from_secs(10),
                ..AgentConfig::default()
            },
            cancel: CancellationToken::new(),
            sequence: AtomicU32::new(0),
        }
    }

    fn request(lane_model: &str) -> CheckRequest {
        CheckRequest {
            lane: LaneName::new("lane-a"),
            lane_model: ModelId::parse(lane_model).unwrap(),
            kind: CheckKind::NewFinding,
            path: "src/a.rs".into(),
            line: 2,
            side: DiffSide::Right,
            text: "x is never set.".into(),
        }
    }

    #[tokio::test]
    async fn with_a_workspace_the_checker_lists_searches_and_reads_it() {
        use crate::workspace::WorkspaceProvider as _;
        let dir = crate::git::ScratchDir::new("henk-check-ws").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "let x = 1;\n").unwrap();
        let workspace = crate::workspace::fake::FakeProvider::default()
            .open(dir.path(), &henk_domain::workspace::Profile::default())
            .await
            .unwrap();
        let search = Ok(Completion {
            message: ChatMessage {
                role: henk_llm::Role::Assistant,
                blocks: vec![henk_llm::Block::ToolCall(ToolCall {
                    id: "s1".into(),
                    name: "search".into(),
                    arguments: ToolArguments::Parsed(json!({"pattern": "let x = \\d"})),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        });
        let model = Arc::new(ScriptedClient::new(
            "opus",
            [
                search,
                verdict_call("rejected", "src/a.rs:1 sets x."),
                done(),
            ],
        ));
        let mut check = checker(vec![model.clone()]).await;
        check.workspace = Some(workspace);
        let verdict = check.check(&request("lane-model")).await;
        assert!(
            matches!(verdict, CheckVerdict::Rejected { .. }),
            "{verdict:?}"
        );
        let requests = model.requests();
        let names: Vec<String> = requests[0]
            .tools
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        for tool in ["list_files", "read_file", "search", "give_verdict"] {
            assert!(names.iter().any(|n| n == tool), "{tool}: {names:?}");
        }
        let answered = requests[1]
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .any(|b| matches!(b, henk_llm::Block::ToolResult(r) if r.content == "src/a.rs:1: let x = 1;\n"));
        assert!(
            answered,
            "the search read the workspace: {:?}",
            requests[1].messages
        );
    }

    #[tokio::test]
    async fn without_a_workspace_the_checker_has_no_code_tools() {
        let model = Arc::new(ScriptedClient::new(
            "opus",
            [verdict_call("rejected", "no."), done()],
        ));
        let check = checker(vec![model.clone()]).await;
        let _ = check.check(&request("lane-model")).await;
        let names: Vec<String> = model.requests()[0]
            .tools
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            !names.iter().any(|n| n == "search" || n == "list_files"),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn a_rejection_comes_back_with_its_reason_and_the_diff_was_shown() {
        let model = Arc::new(ScriptedClient::new(
            "opus",
            [verdict_call("rejected", "src/a.rs:2 sets x."), done()],
        ));
        let check = checker(vec![model.clone()]).await;
        let verdict = check.check(&request("lane-model")).await;
        assert_eq!(
            verdict,
            CheckVerdict::Rejected {
                by: ModelId::parse("opus").unwrap(),
                reason: "src/a.rs:2 sets x.".into()
            }
        );
        let opening = model.requests()[0].messages[0].text();
        assert!(opening.contains("x is never set."), "{opening}");
        assert!(opening.contains("+    let x = 2;"), "{opening}");
        let events = check.store.events(&check.run).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("rejected finding on src/a.rs:2"))
        );
    }

    #[tokio::test]
    async fn the_lanes_own_model_goes_last_and_a_silent_model_falls_through() {
        // The lane runs on "opus", so the backup "sonnet" checks first; it
        // ends without a verdict twice, and then "opus" confirms.
        let sonnet = Arc::new(ScriptedClient::new("sonnet", [done(), done()]));
        let opus = Arc::new(ScriptedClient::new(
            "opus",
            [verdict_call("confirmed", "It holds."), done()],
        ));
        let check = checker(vec![opus.clone(), sonnet.clone()]).await;
        let verdict = check.check(&request("opus")).await;
        assert!(matches!(verdict, CheckVerdict::Confirmed { ref by, .. } if by.as_str() == "opus"));
        assert_eq!(
            sonnet.requests().len(),
            2,
            "asked once for the missing verdict"
        );
        let last = sonnet.requests()[1].messages.last().unwrap().text();
        assert!(last.contains("give_verdict"), "{last}");
    }

    #[tokio::test]
    async fn no_verdict_from_anyone_is_unavailable() {
        let model = Arc::new(ScriptedClient::new("opus", [done(), done()]));
        let check = checker(vec![model]).await;
        let verdict = check.check(&request("lane-model")).await;
        assert!(matches!(verdict, CheckVerdict::Unavailable { ref why } if why.contains("opus")));
    }
}
