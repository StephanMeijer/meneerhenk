//! A second model checks every draft before anything is written (§3.2,
//! #189).
//!
//! Lanes are wrong in a recognisable way: they claim a file lacks something
//! it has, or call intended behaviour a bug. Lanes therefore write drafts,
//! and once every lane has ended the drafts are checked together: one
//! session per checking model and chunk of [`CHUNK`] drafts, each with the
//! diff of the files it is about once, the read-only code tools and one
//! tool to give a verdict per draft. A draft is never checked first by its
//! own lane's model when there is another. The sessions run one after
//! another, so the checker's workspace is each session's alone.
//!
//! What no model gives a verdict on goes out unchecked and the run records
//! that: Henk is advisory, and an outage of the checking model must not
//! silence reviews.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{AgentConfig, Continuation, EndReason, Tool, ToolOutput, ToolSet};
use henk_domain::diff::ReviewDiff;
use henk_domain::draft::{
    CHUNK, Draft, DraftBook, DraftId, DraftKind, Original, Verdict, same_as_allowed,
};
use henk_domain::finding::Finding;
use henk_domain::marker::ModelId;
use henk_domain::run::RunId;
use henk_llm::{ChatMessage, ModelClient, ToolDef, ToolName};
use henk_platform::DiffSide;
use henk_session::{SessionSpec, model_id, run_session};
use henk_store::RunStore;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::review_tools::{DiffFiles, GetFileDiff, ListChangedFiles, ReadFile};

/// How much of another draft or finding the opening shows: enough to see
/// whether it is the same problem.
const CONTEXT_CHARS: usize = 400;

/// Checks a review's drafts after its lanes, in sessions on the run.
pub struct FactChecker {
    /// Run records; each check is a session row named `check-<n>`.
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
    /// platform (#90). One session uses it at a time.
    pub workspace: Option<Arc<dyn crate::workspace::Workspace>>,
    /// The time each session's commands get in that workspace (#85).
    pub check_limits: henk_domain::workspace::Limits,
    /// The rendered system prompt, without skills.
    pub system: String,
    /// The skills the checker may load.
    pub skills: crate::skill_tools::AgentSkills,
    /// Turn and time limits per draft; a session gets them times the
    /// drafts it judges.
    pub limits: AgentConfig,
    /// The review's cancellation.
    pub cancel: CancellationToken,
}

impl FactChecker {
    /// The models to try for a draft of `lane_model`, in order: never the
    /// lane's own model first when there is another.
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

    /// A verdict on every draft in `book`. `existing` are the findings on
    /// the pull request, which a draft may repeat. A draft no model gave a
    /// verdict on is [`Verdict::Unchecked`].
    pub async fn check_all(
        &self,
        book: &DraftBook,
        existing: &[Finding],
    ) -> BTreeMap<DraftId, Verdict> {
        let mut verdicts = BTreeMap::new();
        let mut tried: BTreeMap<DraftId, Vec<String>> = BTreeMap::new();
        let mut pending: Vec<DraftId> = book.iter().map(|d| d.id).collect();
        let mut sessions = 0;
        while !pending.is_empty() && !self.cancel.is_cancelled() {
            let next = |draft: &Draft| {
                let done = tried.get(&draft.id).map_or(0, Vec::len);
                self.order(&draft.model)
                    .get(done)
                    .map(|model| model_id(model.as_ref()))
            };
            let chunks = book.chunks(&pending, next, CHUNK);
            if chunks.is_empty() {
                break;
            }
            for (checker, ids) in chunks {
                if self.cancel.is_cancelled() {
                    break;
                }
                let Some(model) = self.models.iter().find(|m| model_id(m.as_ref()) == checker)
                else {
                    continue;
                };
                sessions += 1;
                let got = self
                    .session(sessions, Arc::clone(model), book, &ids, existing)
                    .await;
                for id in ids {
                    match got.get(&id) {
                        Some(verdict) => {
                            verdicts.insert(id, verdict.clone());
                        }
                        None => tried.entry(id).or_default().push(checker.to_string()),
                    }
                }
            }
            pending.retain(|id| !verdicts.contains_key(id));
        }
        for draft in book.iter() {
            verdicts.entry(draft.id).or_insert_with(|| {
                let why = match tried.get(&draft.id) {
                    Some(models) if !models.is_empty() => {
                        format!("no verdict from {}", models.join(" or "))
                    }
                    _ => "the review was cancelled".to_owned(),
                };
                Verdict::Unchecked { why }
            });
        }
        verdicts
    }

    /// One session on `model`, judging `ids`.
    async fn session(
        &self,
        number: u32,
        model: Arc<dyn ModelClient>,
        book: &DraftBook,
        ids: &[DraftId],
        existing: &[Finding],
    ) -> BTreeMap<DraftId, Verdict> {
        let by = model_id(model.as_ref());
        let given = Arc::new(Mutex::new(BTreeMap::new()));
        let comments: Vec<String> = existing
            .iter()
            .filter(|f| !f.resolved)
            .map(|f| f.comment_id.clone())
            .collect();
        let files = Arc::new(DiffFiles::new(Arc::clone(&self.diff)));
        let mut tools = ToolSet::new();
        tools.add(ListChangedFiles(Arc::clone(&files)));
        tools.add(GetFileDiff(files));
        match (&self.workspace, &self.file_reader) {
            (Some(workspace), _) => {
                let workspace =
                    crate::workspace::metered(Arc::clone(workspace), self.check_limits.clone());
                crate::code_tools::add(&mut tools, &workspace);
                crate::code_tools::add_bash(
                    &mut tools,
                    &workspace,
                    std::time::Duration::from_secs(self.check_limits.command_secs),
                    crate::code_tools::BashUse::Review,
                );
            }
            (None, Some(inner)) => {
                tools.add(ReadFile {
                    inner: Arc::clone(inner),
                });
            }
            (None, None) => {}
        }
        tools.add(GiveVerdict {
            by: by.clone(),
            book: book.clone(),
            judging: ids.iter().copied().collect(),
            comments,
            given: Arc::clone(&given),
        });
        let mut system = self.system.clone();
        crate::skill_tools::equip(&mut system, &mut tools, self.skills.clone());
        let drafts = u32::try_from(ids.len()).unwrap_or(u32::MAX).max(1);
        let limits = AgentConfig {
            max_turns: self.limits.max_turns.saturating_mul(drafts),
            timeout: self.limits.timeout.saturating_mul(drafts),
            ..self.limits
        };
        let spec = SessionSpec {
            name: format!("check-{number}"),
            model,
            system,
            opening: vec![ChatMessage::user(opening(book, ids, existing, &self.diff))],
            tools,
            limits,
            continuation: Some(ask_for_verdicts(ids.to_vec(), Arc::clone(&given))),
            turn_warning: None,
        };
        let outcome = run_session(self.store.as_ref(), &self.run, spec, self.cancel.clone()).await;
        let given = given.lock().map(|g| g.clone()).unwrap_or_default();
        info!(session = number, %by, drafts = ids.len(), verdicts = given.len(), stop = ?outcome.stop, "fact-check session");
        if given.len() < ids.len() {
            warn!(session = number, %by, missing = ids.len() - given.len(), "fact-check ended without every verdict");
        }
        given
    }
}

/// The opening message: the drafts to judge, fenced as material; the
/// earlier drafts and the existing findings on the same files, for
/// `same_as` only; and the numbered diff of each file the drafts are about,
/// once.
fn opening(book: &DraftBook, ids: &[DraftId], existing: &[Finding], diff: &ReviewDiff) -> String {
    let judging: Vec<&Draft> = ids.iter().filter_map(|id| book.get(*id)).collect();
    let paths: BTreeSet<&str> = judging.iter().map(|d| d.key.path.as_str()).collect();
    let mut out = format!(
        "Check these {} draft(s). Give one verdict per draft with give_verdict.\n",
        judging.len()
    );
    for draft in &judging {
        out.push('\n');
        out.push_str(&describe(draft));
    }

    let last = ids.iter().max().copied();
    let earlier: Vec<&Draft> = book
        .iter()
        .filter(|d| !ids.contains(&d.id) && last.is_some_and(|last| d.id < last))
        .collect();
    let others: Vec<&Finding> = existing
        .iter()
        .filter(|f| !f.resolved && paths.contains(f.key.path.as_str()))
        .collect();
    if !earlier.is_empty() || !others.is_empty() {
        out.push_str(
            "\nFor same_as only, not to judge here: what other reviewers drafted earlier, and the findings already on these files.\n",
        );
        for draft in earlier {
            let _ = writeln!(
                out,
                "- {} at {}:{}: {}",
                draft.id,
                draft.key.path,
                draft.key.line,
                short(&draft.text)
            );
        }
        for finding in others {
            let _ = writeln!(
                out,
                "- comment {} at {}:{}: {}",
                finding.comment_id,
                finding.key.path,
                finding.key.line,
                short(crate::review_tools::visible_text(&finding.body))
            );
        }
    }

    for path in paths {
        match diff.file(path) {
            Some(file) => {
                let _ = write!(out, "\nThe diff of {path}:\n{}", file.render());
            }
            None => {
                let _ = write!(
                    out,
                    "\n{path} is not part of this change; the changed files are:\n{}",
                    diff.render_list()
                );
            }
        }
    }
    out
}

/// One draft to judge, as the opening shows it.
fn describe(draft: &Draft) -> String {
    let side = match draft.kind {
        DraftKind::Finding {
            side: DiffSide::Left,
        } => " (old side, a removed line)",
        _ => "",
    };
    let place = format!("{}:{}{side}", draft.key.path, draft.key.line);
    match &draft.kind {
        DraftKind::Finding { .. } => format!(
            "## {}: a new finding on {place}\n```text\n{}\n```\n",
            draft.id, draft.text
        ),
        DraftKind::Rewrite {
            comment_id,
            current,
        } => format!(
            "## {}: a rewrite of finding {comment_id} on {place}\nThe finding now:\n```text\n{current}\n```\nProposed text:\n```text\n{}\n```\n",
            draft.id, draft.text
        ),
        DraftKind::Withdrawal {
            comment_id,
            finding,
        } => format!(
            "## {}: withdrawing finding {comment_id} on {place} as wrong\nThe finding:\n```text\n{finding}\n```\nThe reason given for withdrawing it:\n```text\n{}\n```\n",
            draft.id, draft.text
        ),
    }
}

/// The first line of `text`, cut to [`CONTEXT_CHARS`].
fn short(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(CONTEXT_CHARS) {
        Some((cut, _)) => format!("{}...", line.get(..cut).unwrap_or(line)),
        None => line.to_owned(),
    }
}

/// Asks once for the missing verdicts when the model ends its turn
/// without all of them.
fn ask_for_verdicts(
    ids: Vec<DraftId>,
    given: Arc<Mutex<BTreeMap<DraftId, Verdict>>>,
) -> Continuation {
    let asked = AtomicBool::new(false);
    Box::new(move |ending| {
        if ending.reason != EndReason::EndTurn {
            return None;
        }
        let missing: Vec<String> = given.lock().map_or_else(
            |_| Vec::new(),
            |given| {
                ids.iter()
                    .filter(|id| !given.contains_key(id))
                    .map(ToString::to_string)
                    .collect()
            },
        );
        (!missing.is_empty() && !asked.swap(true, Ordering::SeqCst)).then(|| {
            format!(
                "You have no verdict yet on {}. Call give_verdict for each, then end your turn.",
                missing.join(", ")
            )
        })
    })
}

/// `give_verdict`: one verdict per draft of the session.
struct GiveVerdict {
    by: ModelId,
    book: DraftBook,
    judging: BTreeSet<DraftId>,
    comments: Vec<String>,
    given: Arc<Mutex<BTreeMap<DraftId, Verdict>>>,
}

#[async_trait::async_trait]
impl Tool for GiveVerdict {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("give_verdict")
                .unwrap_or_else(|_| unreachable!("a constant tool name")),
            description: "Records your verdict on one draft, once: confirmed if it holds, rejected if it does not, or same_as when it says what an earlier draft or an existing finding already says. Cite the evidence as path:line.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The draft, such as d3"},
                    "verdict": {"type": "string", "enum": ["confirmed", "rejected", "same_as"]},
                    "same_as": {"type": "string", "description": "With same_as: the earlier draft (d2) or the comment id of the finding it repeats"},
                    "reason": {"type": "string", "description": "A few plain sentences citing the code as path:line"}
                },
                "required": ["id", "verdict", "reason"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let Some(id) = text("id").and_then(DraftId::parse) else {
            return ToolOutput::error("id must name a draft, such as d3");
        };
        if !self.judging.contains(&id) {
            let yours: Vec<String> = self.judging.iter().map(ToString::to_string).collect();
            return ToolOutput::error(format!(
                "{id} is not yours to judge here; yours are {}.",
                yours.join(", ")
            ));
        }
        let Some(reason) = text("reason").map(str::to_owned) else {
            return ToolOutput::error("reason is required");
        };
        let by = self.by.clone();
        let verdict = match text("verdict") {
            Some("confirmed") => Verdict::Confirmed { by, reason },
            Some("rejected") => Verdict::Rejected { by, reason },
            Some("same_as") => {
                let Some(of) = text("same_as").map(Original::parse) else {
                    return ToolOutput::error(
                        "same_as needs the earlier draft (d2) or the comment id it repeats",
                    );
                };
                let comments: Vec<&str> = self.comments.iter().map(String::as_str).collect();
                if !same_as_allowed(&self.book, id, &of, &comments) {
                    return ToolOutput::error(format!(
                        "{id} can only repeat an earlier draft or a finding shown to you, not {of}."
                    ));
                }
                Verdict::SameAs { by, of, reason }
            }
            _ => {
                return ToolOutput::error(
                    "verdict must be \"confirmed\", \"rejected\" or \"same_as\"",
                );
            }
        };
        let Ok(mut given) = self.given.lock() else {
            return ToolOutput::error("verdicts unavailable");
        };
        if given.contains_key(&id) {
            return ToolOutput::error(format!("{id} already has a verdict."));
        }
        given.insert(id, verdict);
        let left = self.judging.len() - given.len();
        ToolOutput::ok(if left == 0 {
            "Recorded. Every draft has a verdict; end your turn now.".to_owned()
        } else {
            format!("Recorded. {left} draft(s) still need a verdict.")
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unnecessary_wraps,
        clippy::needless_pass_by_value
    )]

    use std::time::Duration;

    use henk_domain::allowlist::Platform;
    use henk_domain::finding::FindingKey;
    use henk_domain::review::LaneName;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{Block, Completion, LlmError, Role, StopReason, ToolArguments, ToolCall, Usage};
    use henk_store::SqliteStore;

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

    async fn store() -> Arc<SqliteStore> {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        store
            .create_run(&henk_store::NewRun {
                id: RunId::parse("r-1").unwrap(),
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
        store
    }

    fn verdict(id: &str, verdict: &str, extra: Value) -> Result<Completion, LlmError> {
        let mut args = json!({"id": id, "verdict": verdict, "reason": "src/a.rs:2 says so."});
        if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
            args.extend(extra.clone());
        }
        Ok(Completion {
            message: ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall(ToolCall {
                    id: format!("v-{id}"),
                    name: "give_verdict".into(),
                    arguments: ToolArguments::Parsed(args),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        })
    }

    fn done() -> Result<Completion, LlmError> {
        Ok(Completion {
            message: ChatMessage::assistant("Done."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    fn model(name: &str) -> ModelId {
        ModelId::parse(name).unwrap()
    }

    fn checker(store: Arc<SqliteStore>, models: Vec<Arc<dyn ModelClient>>) -> FactChecker {
        FactChecker {
            store,
            run: RunId::parse("r-1").unwrap(),
            models,
            diff: Arc::new(ReviewDiff::from_unified(DIFF)),
            file_reader: None,
            workspace: None,
            check_limits: henk_domain::workspace::Limits::default(),
            system: "Check.".into(),
            skills: crate::skill_tools::AgentSkills::default(),
            limits: AgentConfig {
                max_turns: 10,
                timeout: Duration::from_secs(10),
                ..AgentConfig::default()
            },
            cancel: CancellationToken::new(),
        }
    }

    /// `n` new findings by lane-a on model `lane_model`, on line 2 then
    /// lines past it (the book does not check the diff).
    fn book(n: u32, lane_model: &str) -> DraftBook {
        let mut book = DraftBook::new();
        for line in 0..n {
            book.add(
                &LaneName::new("lane-a"),
                &model(lane_model),
                DraftKind::Finding {
                    side: DiffSide::Right,
                },
                FindingKey {
                    path: "src/a.rs".into(),
                    line: 2 + line,
                },
                &format!("Problem {line}."),
            )
            .unwrap();
        }
        book
    }

    fn id(text: &str) -> DraftId {
        DraftId::parse(text).unwrap()
    }

    #[tokio::test]
    async fn one_session_judges_its_drafts_with_the_diff_shown_once() {
        let store = store().await;
        let other = Arc::new(ScriptedClient::new(
            "n",
            [
                verdict("d1", "confirmed", json!({})),
                verdict("d2", "rejected", json!({})),
                verdict("d3", "same_as", json!({"same_as": "d1"})),
                done(),
            ],
        ));
        let own = Arc::new(ScriptedClient::new("m", []));
        let checker = checker(
            Arc::clone(&store),
            vec![
                Arc::clone(&own) as Arc<dyn ModelClient>,
                Arc::clone(&other) as Arc<dyn ModelClient>,
            ],
        );
        let verdicts = checker.check_all(&book(3, "m"), &[]).await;
        assert!(
            matches!(verdicts[&id("d1")], Verdict::Confirmed { ref by, .. } if *by == model("n"))
        );
        assert!(matches!(verdicts[&id("d2")], Verdict::Rejected { .. }));
        assert!(matches!(
            verdicts[&id("d3")],
            Verdict::SameAs { of: Original::Draft(of), .. } if of == id("d1")
        ));
        assert!(
            own.requests().is_empty(),
            "the lane's own model never checks first"
        );

        let requests = other.requests();
        let opening = requests[0].messages[0].text();
        assert_eq!(
            opening.matches("The diff of src/a.rs:").count(),
            1,
            "{opening}"
        );
        for expected in ["## d1: a new finding on src/a.rs:2", "## d3:", "Problem 2."] {
            assert!(opening.contains(expected), "{expected} in {opening}");
        }
        let sessions: Vec<String> = store
            .lanes(&RunId::parse("r-1").unwrap())
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert_eq!(sessions, ["check-1"]);
    }

    #[tokio::test]
    async fn eleven_drafts_take_two_sessions() {
        let store = store().await;
        let mut script: Vec<_> = (1..=10)
            .map(|n| verdict(&format!("d{n}"), "confirmed", json!({})))
            .collect();
        script.push(done());
        script.push(verdict("d11", "confirmed", json!({})));
        script.push(done());
        let checker = checker(
            Arc::clone(&store),
            vec![Arc::new(ScriptedClient::new("n", script)) as Arc<dyn ModelClient>],
        );
        let verdicts = checker.check_all(&book(11, "m"), &[]).await;
        assert_eq!(verdicts.len(), 11);
        assert!(
            verdicts
                .values()
                .all(|v| matches!(v, Verdict::Confirmed { .. }))
        );
        let lanes = store.lanes(&RunId::parse("r-1").unwrap()).await.unwrap();
        assert_eq!(lanes.len(), 2);
    }

    #[tokio::test]
    async fn bad_verdicts_are_refused_and_a_missing_one_falls_through_then_goes_unchecked() {
        let store = store().await;
        let first = Arc::new(ScriptedClient::new(
            "n",
            [
                verdict("d9", "confirmed", json!({})),
                verdict("d2", "same_as", json!({"same_as": "d3"})),
                verdict("d1", "same_as", json!({"same_as": "c-404"})),
                verdict("d1", "maybe", json!({})),
                verdict("d1", "confirmed", json!({})),
                verdict("d1", "rejected", json!({})),
                done(),
                done(),
            ],
        ));
        let backup = Arc::new(ScriptedClient::new("o", [done(), done()]));
        let checker = checker(
            Arc::clone(&store),
            vec![
                Arc::clone(&first) as Arc<dyn ModelClient>,
                Arc::clone(&backup) as Arc<dyn ModelClient>,
            ],
        );
        let verdicts = checker.check_all(&book(2, "m"), &[]).await;
        assert!(
            matches!(verdicts[&id("d1")], Verdict::Confirmed { .. }),
            "the first verdict stands"
        );
        assert_eq!(
            verdicts[&id("d2")],
            Verdict::Unchecked {
                why: "no verdict from n or o".into()
            }
        );
        let results: Vec<String> = first
            .requests()
            .last()
            .unwrap()
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                Block::ToolResult(r) => Some(r.content.clone()),
                _ => None,
            })
            .collect();
        assert!(results[0].contains("d9 is not yours"), "{results:?}");
        assert!(
            results[1].contains("only repeat an earlier draft"),
            "{results:?}"
        );
        assert!(results[2].contains("not comment c-404"), "{results:?}");
        assert!(results[3].contains("verdict must be"), "{results:?}");
        assert!(results[5].contains("already has a verdict"), "{results:?}");
        let nudge = first
            .requests()
            .last()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .text();
        assert!(nudge.contains("no verdict yet on d2"), "{nudge}");
        assert_eq!(
            backup.requests()[0].messages[0]
                .text()
                .matches("## ")
                .count(),
            1,
            "the backup gets only what is still open"
        );
    }

    #[tokio::test]
    async fn a_same_as_may_name_an_existing_finding_shown_in_the_opening() {
        let store = store().await;
        let client = Arc::new(ScriptedClient::new(
            "n",
            [verdict("d1", "same_as", json!({"same_as": "c-7"})), done()],
        ));
        let checker = checker(
            Arc::clone(&store),
            vec![Arc::clone(&client) as Arc<dyn ModelClient>],
        );
        let existing = Finding {
            key: FindingKey {
                path: "src/a.rs".into(),
                line: 2,
            },
            comment_id: "c-7".into(),
            body: "x changes meaning.<!-- henk -->".into(),
            lane: None,
            answered_by_person: false,
            resolved: false,
            in_diff: true,
        };
        let verdicts = checker.check_all(&book(1, "m"), &[existing]).await;
        assert!(matches!(
            &verdicts[&id("d1")],
            Verdict::SameAs { of: Original::Comment(c), .. } if c == "c-7"
        ));
        let opening = client.requests()[0].messages[0].text();
        assert!(
            opening.contains("- comment c-7 at src/a.rs:2: x changes meaning."),
            "{opening}"
        );
    }

    #[tokio::test]
    async fn without_a_workspace_the_checker_has_no_code_tools() {
        let store = store().await;
        let client = Arc::new(ScriptedClient::new(
            "n",
            [verdict("d1", "confirmed", json!({})), done()],
        ));
        let checker = checker(store, vec![Arc::clone(&client) as Arc<dyn ModelClient>]);
        checker.check_all(&book(1, "m"), &[]).await;
        let tools: Vec<String> = client.requests()[0]
            .tools
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            tools,
            ["get_file_diff", "give_verdict", "list_changed_files"]
        );
    }

    #[tokio::test]
    async fn a_cancelled_review_checks_nothing() {
        let store = store().await;
        let client = Arc::new(ScriptedClient::new("n", []));
        let checker = checker(store, vec![Arc::clone(&client) as Arc<dyn ModelClient>]);
        checker.cancel.cancel();
        let verdicts = checker.check_all(&book(1, "m"), &[]).await;
        assert_eq!(
            verdicts[&id("d1")],
            Verdict::Unchecked {
                why: "the review was cancelled".into()
            }
        );
        assert!(client.requests().is_empty());
    }
}
