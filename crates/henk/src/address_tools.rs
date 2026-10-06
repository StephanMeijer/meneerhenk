//! The tools an address run's model works with (§3.5): read, search and
//! edit files in the workspace, run the project's checks, and settle each
//! review thread. The tools talk only to a [`Workspace`]; nothing here
//! reaches the network, git or a credential. Committing, pushing and
//! replying are Henk's code, after the session.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use henk_agent::{Tool, ToolOutput, ToolSet};
use henk_domain::address::{ThreadOutcome, WorkspacePath};
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use henk_platform::address::OpenThread;
use serde_json::{Value, json};

use crate::checks::{describe, run_checks};
use crate::workspace::Workspace;

/// Lines `read_file` returns at most per call.
const READ_LINES: usize = 400;
/// Files larger than this are not read, searched or written.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Entries `list_files` and hits `search` return at most.
const LIST_CAP: usize = 500;

/// What all address tools share.
pub struct AddressContext {
    /// Where the files are and the checks run.
    pub workspace: Arc<dyn Workspace>,
    /// The open threads, in the order they were listed.
    pub threads: Vec<OpenThread>,
    /// The project's checks.
    pub check_commands: Vec<Vec<String>>,
    /// Time limit per check.
    pub check_timeout: Duration,
    /// Files one run may change.
    pub max_changed_files: usize,
    /// What the model decided so far.
    pub state: Mutex<AddressState>,
}

/// One settled thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// How it ends.
    pub outcome: ThreadOutcome,
    /// The reply text.
    pub reply: String,
}

/// What the model decided.
#[derive(Debug, Default)]
pub struct AddressState {
    /// Settled threads by thread id.
    pub settled: BTreeMap<String, Settled>,
    /// Files the tools wrote.
    pub written: BTreeSet<String>,
}

impl AddressContext {
    /// Refuses a write to a new file beyond the limit, or of more bytes
    /// than a file may have. Nothing is recorded yet.
    fn may_write(&self, path: &WorkspacePath, bytes: usize) -> Result<(), String> {
        if u64::try_from(bytes).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            return Err(format!("{path} would be larger than 1 MB"));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| "address state unavailable".to_owned())?;
        if !state.written.contains(path.as_str()) && state.written.len() >= self.max_changed_files {
            return Err(format!(
                "At most {} files per run; that limit is reached.",
                self.max_changed_files
            ));
        }
        Ok(())
    }

    /// Writes `content` to `path` within the limits and records it.
    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), String> {
        self.may_write(path, content.len())?;
        self.workspace
            .write(path, content)
            .await
            .map_err(|e| format!("cannot write {path}: {e}"))?;
        self.state
            .lock()
            .map_err(|_| "address state unavailable".to_owned())?
            .written
            .insert(path.as_str().to_owned());
        Ok(())
    }

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

fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn schema(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

macro_rules! address_tool {
    ($name:ident) => {
        /// An address tool.
        pub struct $name(pub Arc<AddressContext>);
    };
}

address_tool!(ListFiles);
address_tool!(ReadFile);
address_tool!(SearchFiles);
address_tool!(EditFile);
address_tool!(WriteFile);
address_tool!(RunChecks);
address_tool!(ListThreads);
address_tool!(SettleThread);

#[async_trait::async_trait]
impl Tool for ListFiles {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_files"),
            description: "Lists the files under a directory of the repository (default: all)."
                .to_owned(),
            input_schema: schema(&json!({"dir": {"type": "string"}}), &[]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let dir = match WorkspacePath::parse_dir(arg_str(&args, "dir").unwrap_or("")) {
            Ok(dir) => dir,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let files = match ctx.workspace.list(&dir, LIST_CAP).await {
            Ok(files) => files,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let mut text = String::new();
        for file in &files {
            let _ = writeln!(text, "{file}");
        }
        if files.len() >= LIST_CAP {
            text.push_str("[more files; list a subdirectory]\n");
        }
        ToolOutput::ok(if text.is_empty() {
            "No files.".to_owned()
        } else {
            text
        })
    }
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("read_file"),
            description: format!(
                "Reads a file with line numbers, from line `from` (default 1), at most {READ_LINES} lines."
            ),
            input_schema: schema(
                &json!({"path": {"type": "string"}, "from": {"type": "integer", "minimum": 1}}),
                &["path"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let path = match WorkspacePath::parse(arg_str(&args, "path").unwrap_or("")) {
            Ok(path) => path,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let text = match ctx.workspace.read(&path, MAX_FILE_BYTES).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => return ToolOutput::error(format!("cannot read {path}: {error}")),
        };
        let from = args
            .get("from")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(1)
            .max(1);
        let total = text.lines().count();
        let mut out = String::new();
        for (index, line) in text.lines().enumerate().skip(from - 1).take(READ_LINES) {
            let _ = writeln!(out, "{:>5} {line}", index + 1);
        }
        if from - 1 + READ_LINES < total {
            let _ = writeln!(
                out,
                "[{total} lines; continue with from = {}]",
                from + READ_LINES
            );
        }
        ToolOutput::ok(if out.is_empty() {
            format!("{path} is empty or shorter than line {from}.")
        } else {
            out
        })
    }
}

#[async_trait::async_trait]
impl Tool for SearchFiles {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("search"),
            description: "Finds lines containing the exact text in every file of the repository."
                .to_owned(),
            input_schema: schema(&json!({"text": {"type": "string"}}), &["text"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let Some(needle) = arg_str(&args, "text").filter(|t| !t.is_empty()) else {
            return ToolOutput::error("text is required");
        };
        let hits = match ctx
            .workspace
            .search(&WorkspacePath::root(), needle, MAX_FILE_BYTES, LIST_CAP)
            .await
        {
            Ok(hits) => hits,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let mut out = String::new();
        for hit in &hits {
            let _ = writeln!(out, "{}:{}: {}", hit.path, hit.line, hit.text);
        }
        if hits.len() >= LIST_CAP {
            out.push_str("[more matches; search for something narrower]\n");
        }
        ToolOutput::ok(if out.is_empty() {
            "No matches.".to_owned()
        } else {
            out
        })
    }
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("edit_file"),
            description: "Replaces exact text in a file. `old` must occur exactly once; include enough surrounding lines to make it unique.".to_owned(),
            input_schema: schema(
                &json!({"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}}),
                &["path", "old", "new"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(raw), Some(old), Some(new)) = (
            arg_str(&args, "path"),
            arg_str(&args, "old"),
            arg_str(&args, "new"),
        ) else {
            return ToolOutput::error("path, old and new are required");
        };
        if old.is_empty() {
            return ToolOutput::error("old must not be empty; use write_file for a new file");
        }
        let path = match WorkspacePath::parse(raw) {
            Ok(path) => path,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let bytes = match ctx.workspace.read(&path, MAX_FILE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => return ToolOutput::error(format!("cannot read {path}: {error}")),
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return ToolOutput::error(format!("{path} is not a text file"));
        };
        match text.matches(old).count() {
            0 => return ToolOutput::error(format!("old text not found in {path}")),
            1 => {}
            n => {
                return ToolOutput::error(format!(
                    "old text occurs {n} times in {path}; include more context"
                ));
            }
        }
        match ctx
            .write(&path, text.replacen(old, new, 1).as_bytes())
            .await
        {
            Ok(()) => ToolOutput::ok(format!("Edited {path}.")),
            Err(error) => ToolOutput::error(error),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("write_file"),
            description: "Writes a whole file, creating it or replacing its content.".to_owned(),
            input_schema: schema(
                &json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(raw), Some(content)) = (arg_str(&args, "path"), arg_str(&args, "content")) else {
            return ToolOutput::error("path and content are required");
        };
        let path = match WorkspacePath::parse(raw) {
            Ok(path) => path,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        match ctx.write(&path, content.as_bytes()).await {
            Ok(()) => ToolOutput::ok(format!("Wrote {path}.")),
            Err(error) => ToolOutput::error(error),
        }
    }
}

#[async_trait::async_trait]
impl Tool for RunChecks {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("run_checks"),
            description: "Runs the project's configured checks (build, tests, lint) in the workspace and reports each.".to_owned(),
            input_schema: schema(&json!({}), &[]),
        }
    }

    async fn call(&self, _args: Value) -> ToolOutput {
        let ctx = &self.0;
        let results = run_checks(
            ctx.workspace.as_ref(),
            &ctx.check_commands,
            ctx.check_timeout,
        )
        .await;
        ToolOutput::ok(describe(&results))
    }
}

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
        if let Ok(state) = ctx.state.lock() {
            for (id, settled) in &state.settled {
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
        let Ok(mut state) = ctx.state.lock() else {
            return ToolOutput::error("address state unavailable");
        };
        state.settled.insert(
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
    let mut set = ToolSet::new();
    set.add(ListFiles(Arc::clone(ctx)))
        .add(ReadFile(Arc::clone(ctx)))
        .add(SearchFiles(Arc::clone(ctx)))
        .add(EditFile(Arc::clone(ctx)))
        .add(WriteFile(Arc::clone(ctx)))
        .add(RunChecks(Arc::clone(ctx)))
        .add(ListThreads(Arc::clone(ctx)))
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

    use henk_platform::address::ThreadNote;

    use henk_domain::workspace::Profile;

    use super::*;
    use crate::git::ScratchDir;
    use crate::workspace::WorkspaceProvider as _;
    use crate::workspace::fake::{FakeProvider, Scripted};

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

    /// The tools over a fake workspace holding `src/a.rs`, where `true`
    /// passes. Path checks on a real file system are the host backend's
    /// tests.
    async fn workspace(name: &str) -> (FakeProvider, Arc<AddressContext>) {
        let dir = ScratchDir::new(name).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
        std::fs::write(
            dir.path().join("src/a.rs"),
            "fn main() {\n    let x = 1;\n    let x = 1;\n}\n",
        )
        .unwrap();
        let mut provider = FakeProvider::default();
        provider
            .script
            .insert("true".to_owned(), Scripted::default());
        let workspace = provider
            .open(dir.path(), &Profile::default())
            .await
            .unwrap();
        let ctx = Arc::new(AddressContext {
            workspace,
            threads: vec![thread("T1", true)],
            check_commands: vec![vec!["true".to_owned()]],
            check_timeout: Duration::from_secs(5),
            max_changed_files: 2,
            state: Mutex::default(),
        });
        (provider, ctx)
    }

    async fn content(ctx: &AddressContext, path: &str) -> String {
        let bytes = ctx
            .workspace
            .read(&WorkspacePath::parse(path).unwrap(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[tokio::test]
    async fn paths_out_of_the_repository_are_refused_before_the_workspace() {
        let (_provider, ctx) = workspace("henk-tools-escape").await;
        for args in [
            json!({"path": "../outside"}),
            json!({"path": "/etc/passwd"}),
            json!({"path": ".git/config"}),
        ] {
            let out = ReadFile(Arc::clone(&ctx)).call(args.clone()).await;
            assert!(out.is_error, "read {args}");
        }
        for args in [
            json!({"path": "../x", "content": "y"}),
            json!({"path": ".git/hooks/pre-commit", "content": "y"}),
        ] {
            let out = WriteFile(Arc::clone(&ctx)).call(args.clone()).await;
            assert!(out.is_error, "write {args}");
        }
        assert!(ctx.workspace.export().await.unwrap().is_empty());
        let listed = ListFiles(Arc::clone(&ctx)).call(json!({})).await;
        assert_eq!(listed.content, "src/a.rs\n");
        let big = "x".repeat(1024 * 1024 + 1);
        let out = WriteFile(Arc::clone(&ctx))
            .call(json!({"path": "big.txt", "content": big}))
            .await;
        assert!(out.content.contains("larger than 1 MB"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_edit_must_match_exactly_once() {
        let (_provider, ctx) = workspace("henk-tools-edit").await;
        let twice = EditFile(Arc::clone(&ctx))
            .call(json!({"path": "src/a.rs", "old": "let x = 1;", "new": "let x = 2;"}))
            .await;
        assert!(twice.is_error);
        assert!(twice.content.contains("2 times"), "{}", twice.content);
        let once = EditFile(Arc::clone(&ctx))
            .call(json!({"path": "src/a.rs", "old": "    let x = 1;\n}", "new": "}"}))
            .await;
        assert!(!once.is_error, "{once:?}");
        assert_eq!(
            content(&ctx, "src/a.rs").await,
            "fn main() {\n    let x = 1;\n}\n"
        );
        let read = ReadFile(Arc::clone(&ctx))
            .call(json!({"path": "src/a.rs", "from": 2}))
            .await;
        assert_eq!(read.content, "    2     let x = 1;\n    3 }\n");
        let found = SearchFiles(Arc::clone(&ctx))
            .call(json!({"text": "let x"}))
            .await;
        assert_eq!(found.content, "src/a.rs:2: let x = 1;\n");
    }

    #[tokio::test]
    async fn new_files_stop_at_the_limit() {
        let (_provider, ctx) = workspace("henk-tools-limit").await;
        for path in ["one.md", "two.md"] {
            let out = WriteFile(Arc::clone(&ctx))
                .call(json!({"path": path, "content": "x"}))
                .await;
            assert!(!out.is_error, "{out:?}");
        }
        let again = WriteFile(Arc::clone(&ctx))
            .call(json!({"path": "one.md", "content": "y"}))
            .await;
        assert!(!again.is_error, "rewriting a file already changed is fine");
        let third = WriteFile(Arc::clone(&ctx))
            .call(json!({"path": "three.md", "content": "x"}))
            .await;
        assert!(third.is_error);
    }

    #[tokio::test]
    async fn a_thread_is_settled_once_with_a_reply_in_style() {
        let (_provider, ctx) = workspace("henk-tools-settle").await;
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
        let state = ctx.state.lock().unwrap();
        assert_eq!(state.settled.len(), 1);
        assert_eq!(state.settled["T1"].outcome, ThreadOutcome::Fixed);
    }

    #[tokio::test]
    async fn checks_run_in_the_workspace() {
        let (provider, ctx) = workspace("henk-tools-checks").await;
        let out = RunChecks(Arc::clone(&ctx)).call(json!({})).await;
        assert_eq!(out.content, "$ true: passed");
        assert_eq!(*provider.ran.lock().unwrap(), ["true"]);
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
