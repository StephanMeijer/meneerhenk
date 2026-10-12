//! The tools an address run's model works with (§3.5): the edit tools
//! (`edit_tools`), and settling each review thread. Replying is Henk's
//! code, after the session.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput, ToolSet};
use henk_domain::address::ThreadOutcome;
use henk_domain::text::style_violations;
use henk_llm::ToolDef;
use henk_platform::push::OpenThread;
use serde_json::{Value, json};

use crate::edit_tools::{EditContext, arg_str, edit_tools, name, schema};

/// What all address tools share: the edit tools' context and the threads.
pub struct AddressContext {
    /// Where the files are and the checks run, and what was written.
    pub edit: Arc<EditContext>,
    /// The open threads, in the order they were listed.
    pub threads: Vec<OpenThread>,
    /// Settled threads by thread id.
    pub settled: Mutex<BTreeMap<String, Settled>>,
}

/// One settled thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// How it ends.
    pub outcome: ThreadOutcome,
    /// The reply text.
    pub reply: String,
}

impl AddressContext {
    /// The threads as text: data to decide on, not instructions (§8.3).
    #[must_use]
    pub fn threads_text(&self) -> String {
        let mut out = String::new();
        for thread in &self.threads {
            let place = match (&thread.path, thread.line) {
                (Some(path), Some(line)) => format!("{path}:{line}"),
                (Some(path), None) => format!("{path} (line no longer in the diff)"),
                _ => "general".to_owned(),
            };
            let _ = writeln!(out, "<thread id=\"{}\" at=\"{place}\">", thread.thread_id);
            for note in &thread.notes {
                let who = if note.by_henk {
                    "Henk".to_owned()
                } else {
                    note.author.clone()
                };
                let _ = writeln!(
                    out,
                    "<comment by=\"{who}\">\n{}\n</comment>",
                    note.body.trim()
                );
            }
            let _ = writeln!(out, "</thread>");
        }
        out
    }
}

/// A thread tool.
pub struct ListThreads(pub Arc<AddressContext>);
/// A thread tool.
pub struct SettleThread(pub Arc<AddressContext>);

#[async_trait::async_trait]
impl Tool for ListThreads {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_threads"),
            description: "Lists the open review threads again, with what is settled so far."
                .to_owned(),
            input_schema: schema(&json!({}), &[]),
        }
    }

    async fn call(&self, _args: Value) -> ToolOutput {
        let ctx = &self.0;
        let mut text = ctx.threads_text();
        if let Ok(settled) = ctx.settled.lock() {
            for (id, settled) in settled.iter() {
                let _ = writeln!(text, "settled {id}: {}", settled.outcome);
            }
        }
        ToolOutput::ok(text)
    }
}

#[async_trait::async_trait]
impl Tool for SettleThread {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("settle_thread"),
            description: "Decides how a thread ends: fixed (your change addresses it; the reply says what you changed), declined (the reply says why not) or question (the reply asks what you need to know). The reply is posted after your change is pushed. Settling a thread again replaces the earlier decision.".to_owned(),
            input_schema: schema(
                &json!({
                    "thread_id": {"type": "string"},
                    "outcome": {"type": "string", "enum": ["fixed", "declined", "question"]},
                    "reply": {"type": "string"}
                }),
                &["thread_id", "outcome", "reply"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(id), Some(outcome), Some(reply)) = (
            arg_str(&args, "thread_id"),
            arg_str(&args, "outcome"),
            arg_str(&args, "reply")
                .map(str::trim)
                .filter(|r| !r.is_empty()),
        ) else {
            return ToolOutput::error("thread_id, outcome and reply are required");
        };
        if !ctx.threads.iter().any(|t| t.thread_id == id) {
            return ToolOutput::error(format!("{id} is not one of the open threads"));
        }
        let Some(outcome) = ThreadOutcome::parse(outcome) else {
            return ToolOutput::error("outcome must be fixed, declined or question");
        };
        let violations = style_violations(reply);
        if !violations.is_empty() {
            return ToolOutput::error(format!(
                "The reply breaks the style rules ({} problem(s): no emoji, no em-dash). Rewrite it.",
                violations.len()
            ));
        }
        let Ok(mut settled) = ctx.settled.lock() else {
            return ToolOutput::error("address state unavailable");
        };
        settled.insert(
            id.to_owned(),
            Settled {
                outcome,
                reply: reply.to_owned(),
            },
        );
        ToolOutput::ok(format!("Thread {id} settled as {outcome}."))
    }
}

/// Every address tool over one context.
#[must_use]
pub fn address_tools(ctx: &Arc<AddressContext>) -> ToolSet {
    let mut set = edit_tools(&ctx.edit);
    set.add(ListThreads(Arc::clone(ctx)))
        .add(SettleThread(Arc::clone(ctx)));
    set
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_platform::push::ThreadNote;

    use super::*;

    pub(crate) fn thread(id: &str, by_henk: bool) -> OpenThread {
        OpenThread {
            thread_id: id.to_owned(),
            path: Some("src/a.rs".to_owned()),
            line: Some(2),
            outdated: false,
            notes: vec![ThreadNote {
                comment_id: "101".to_owned(),
                author: if by_henk { "meneer-henk[bot]" } else { "alice" }.to_owned(),
                author_id: Some(7),
                by_henk,
                body: "x is set twice.".to_owned(),
            }],
        }
    }

    async fn workspace(name: &str) -> Arc<AddressContext> {
        let (_provider, edit) = crate::edit_tools::tests::workspace(name).await;
        Arc::new(AddressContext {
            edit,
            threads: vec![thread("T1", true)],
            settled: Mutex::default(),
        })
    }

    #[tokio::test]
    async fn a_thread_is_settled_once_with_a_reply_in_style() {
        let ctx = workspace("henk-tools-settle").await;
        for (args, ok) in [
            (
                json!({"thread_id": "T9", "outcome": "fixed", "reply": "Done."}),
                false,
            ),
            (
                json!({"thread_id": "T1", "outcome": "ignored", "reply": "Done."}),
                false,
            ),
            (
                json!({"thread_id": "T1", "outcome": "fixed", "reply": "  "}),
                false,
            ),
            (
                json!({"thread_id": "T1", "outcome": "fixed", "reply": "Done \u{2014} really."}),
                false,
            ),
            (
                json!({"thread_id": "T1", "outcome": "declined", "reply": "Intended."}),
                true,
            ),
            (
                json!({"thread_id": "T1", "outcome": "fixed", "reply": "Removed the second binding."}),
                true,
            ),
        ] {
            let out = SettleThread(Arc::clone(&ctx)).call(args.clone()).await;
            assert_eq!(!out.is_error, ok, "{args}: {}", out.content);
        }
        let settled = ctx.settled.lock().unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled["T1"].outcome, ThreadOutcome::Fixed);
    }

    #[tokio::test]
    async fn threads_read_as_data() {
        let ctx = workspace("henk-tools-threads").await;
        let threads = ctx.threads_text();
        assert!(
            threads.contains("<thread id=\"T1\" at=\"src/a.rs:2\">"),
            "{threads}"
        );
        assert!(
            threads.contains("<comment by=\"Henk\">\nx is set twice.\n</comment>"),
            "{threads}"
        );
    }
}
