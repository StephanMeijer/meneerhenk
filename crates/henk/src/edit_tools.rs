//! The tools a model changes code with: the shared code tools to read and
//! search (`code_tools`), edit files in the workspace, and `bash` on a
//! backend apart from Henk, where the executor runs the repository's own
//! checks (#396). The review loop's executor works with these (#284). The tools
//! talk only to a [`Workspace`]; nothing here reaches the network, git or a
//! credential. Committing and pushing are Henk's code, after the session.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use henk_agent::{Tool, ToolOutput, ToolSet};
use henk_domain::patch::{self, Operation, Patch};
use henk_domain::path::WorkspacePath;
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

use crate::code_tools::{self, MAX_FILE_BYTES};
use crate::config::EditFormat;
use crate::workspace::{Workspace, WorkspaceError};

/// What the edit tools share.
pub struct EditContext {
    /// Where the files are and the model's commands run.
    pub workspace: Arc<dyn Workspace>,
    /// Time limit per `bash` command.
    pub command_timeout: Duration,
    /// Files one push may change.
    pub max_changed_files: usize,
    /// Whether the model may run commands of its own (`bash`, #172): only
    /// on a backend apart from Henk, and when its role's tools grant it.
    pub bash: bool,
    /// How the model edits: `edit_file` or `apply_patch` (#561).
    pub edit_format: EditFormat,
    /// What the tools changed so far.
    pub state: Mutex<EditState>,
}

/// What the tools changed.
#[derive(Debug, Default)]
pub struct EditState {
    /// Files the tools wrote.
    pub written: BTreeSet<String>,
}

impl EditContext {
    /// Refuses changes that would take the files changed past the limit.
    /// A file already changed is not counted again. Nothing is recorded.
    fn may_change<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a WorkspacePath>,
    ) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "edit state unavailable".to_owned())?;
        let new: BTreeSet<&str> = paths
            .into_iter()
            .map(WorkspacePath::as_str)
            .filter(|path| !state.written.contains(*path))
            .collect();
        if state.written.len() + new.len() > self.max_changed_files {
            return Err(format!(
                "At most {} files per run; that limit is reached.",
                self.max_changed_files
            ));
        }
        Ok(())
    }

    /// Refuses a write of more bytes than a file may have.
    fn may_hold(path: &WorkspacePath, bytes: usize) -> Result<(), String> {
        if u64::try_from(bytes).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            return Err(format!("{path} would be larger than 1 MB"));
        }
        Ok(())
    }

    fn record(&self, path: &WorkspacePath) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|_| "edit state unavailable".to_owned())?
            .written
            .insert(path.as_str().to_owned());
        Ok(())
    }

    /// Writes `content` to `path` within the limits and records it.
    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), String> {
        Self::may_hold(path, content.len())?;
        self.may_change([path])?;
        self.workspace
            .write(path, content)
            .await
            .map_err(|e| format!("cannot write {path}: {e}"))?;
        self.record(path)
    }

    /// Removes `path` within the limits and records it.
    async fn remove(&self, path: &WorkspacePath) -> Result<(), String> {
        self.may_change([path])?;
        self.workspace
            .remove(path)
            .await
            .map_err(|e| format!("cannot delete {path}: {e}"))?;
        self.record(path)
    }

    /// Whether `path` is a file; an error for anything that cannot be one
    /// to add or move to, such as a directory.
    async fn exists(&self, path: &WorkspacePath) -> Result<bool, String> {
        match self.workspace.read(path, MAX_FILE_BYTES).await {
            Ok(_) => Ok(true),
            Err(WorkspaceError::NotFound(_)) => Ok(false),
            Err(error) => Err(format!("cannot use {path}: {error}")),
        }
    }

    /// The text of an existing file.
    async fn text(&self, path: &WorkspacePath) -> Result<String, String> {
        let bytes = self
            .workspace
            .read(path, MAX_FILE_BYTES)
            .await
            .map_err(|error| format!("cannot read {path}: {error}"))?;
        String::from_utf8(bytes).map_err(|_| format!("{path} is not a text file"))
    }
}

pub(crate) fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

pub(crate) fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

pub(crate) fn schema(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

macro_rules! edit_tool {
    ($name:ident) => {
        /// An edit tool.
        pub struct $name(pub Arc<EditContext>);
    };
}

edit_tool!(EditFile);
edit_tool!(WriteFile);
edit_tool!(ApplyPatch);

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

/// One step of an applied patch, worked out before anything is written.
enum Step {
    Write(WorkspacePath, String),
    Remove(WorkspacePath),
}

impl ApplyPatch {
    /// Works out every step of `parsed` and what to say of it, reading the
    /// files but changing none.
    async fn plan(&self, parsed: &Patch) -> Result<(Vec<Step>, Vec<String>), String> {
        let ctx = &self.0;
        let mut steps = Vec::new();
        let mut done = Vec::new();
        for operation in &parsed.operations {
            match operation {
                Operation::Add { path, content } => {
                    if ctx.exists(path).await? {
                        return Err(format!("{path} already exists; update it instead"));
                    }
                    steps.push(Step::Write(path.clone(), content.clone()));
                    done.push(format!("added {path}"));
                }
                Operation::Delete { path } => {
                    if !ctx.exists(path).await? {
                        return Err(format!("{path} does not exist"));
                    }
                    steps.push(Step::Remove(path.clone()));
                    done.push(format!("deleted {path}"));
                }
                Operation::Update {
                    path,
                    move_to,
                    hunks,
                } => {
                    let text = ctx.text(path).await?;
                    let new = patch::apply(path, &text, hunks).map_err(|e| e.to_string())?;
                    if let Some(to) = move_to {
                        if ctx.exists(to).await? {
                            return Err(format!("{to} already exists; cannot move {path} there"));
                        }
                        steps.push(Step::Write(to.clone(), new));
                        steps.push(Step::Remove(path.clone()));
                        done.push(format!("moved {path} to {to}"));
                    } else {
                        steps.push(Step::Write(path.clone(), new));
                        done.push(format!("updated {path}"));
                    }
                }
            }
        }
        for step in &steps {
            if let Step::Write(path, content) = step {
                EditContext::may_hold(path, content.len())?;
            }
        }
        ctx.may_change(steps.iter().map(|step| match step {
            Step::Write(path, _) | Step::Remove(path) => path,
        }))?;
        Ok((steps, done))
    }
}

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("apply_patch"),
            description: "Adds, updates, moves and deletes files with one patch, applied all or nothing. The patch is:\n*** Begin Patch\n*** Add File: path\n+every line of the new file\n*** Update File: path\n*** Move to: new/path (optional)\n@@ a line just above the change (optional)\n context line\n-removed line\n+added line\n*** Delete File: path\n*** End Patch\nContext lines are copied exactly from the file, with about three around each change, enough that each change matches one place.".to_owned(),
            input_schema: schema(&json!({"patch": {"type": "string"}}), &["patch"]),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let Some(text) = arg_str(&args, "patch") else {
            return ToolOutput::error("patch is required");
        };
        let parsed = match Patch::parse(text) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutput::error(format!("{error}; nothing was changed")),
        };
        let (steps, done) = match self.plan(&parsed).await {
            Ok(planned) => planned,
            Err(error) => return ToolOutput::error(format!("{error}; nothing was changed")),
        };
        for (index, step) in steps.iter().enumerate() {
            let result = match step {
                Step::Write(path, content) => self.0.write(path, content.as_bytes()).await,
                Step::Remove(path) => self.0.remove(path).await,
            };
            if let Err(error) = result {
                return ToolOutput::error(format!(
                    "{error}; {index} of {} steps were applied before it, so read the files before trying again",
                    steps.len()
                ));
            }
        }
        let mut summary = done.join(", ");
        if let Some(first) = summary.get(..1) {
            summary = format!(
                "{}{}.",
                first.to_uppercase(),
                summary.get(1..).unwrap_or_default()
            );
        }
        ToolOutput::ok(summary)
    }
}

/// The tools to read and change the code, and to run the repository's
/// checks where commands may run: what the executor works with (#284).
#[must_use]
pub fn edit_tools(ctx: &Arc<EditContext>) -> ToolSet {
    let mut set = ToolSet::new();
    code_tools::add(&mut set, &ctx.workspace);
    if ctx.bash {
        code_tools::add_bash(
            &mut set,
            &ctx.workspace,
            ctx.command_timeout,
            code_tools::BashUse::Executor,
        );
    }
    match ctx.edit_format {
        EditFormat::Replace => set.add(EditFile(Arc::clone(ctx))),
        EditFormat::Patch => set.add(ApplyPatch(Arc::clone(ctx))),
    };
    set.add(WriteFile(Arc::clone(ctx)));
    set
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::workspace::Profile;

    use super::*;
    use crate::git::ScratchDir;
    use crate::workspace::WorkspaceProvider as _;
    use crate::workspace::fake::{FakeProvider, Scripted};

    /// The tools over a fake workspace holding `src/a.rs`, where `true`
    /// passes. Path checks on a real file system are the host backend's
    /// tests.
    pub(crate) async fn workspace(name: &str) -> (FakeProvider, Arc<EditContext>) {
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
        let ctx = Arc::new(EditContext {
            workspace,
            command_timeout: Duration::from_secs(5),
            max_changed_files: 2,
            bash: false,
            edit_format: EditFormat::Replace,
            state: Mutex::default(),
        });
        (provider, ctx)
    }

    async fn content(ctx: &EditContext, path: &str) -> String {
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
            let out = code_tools::ReadFile(Arc::clone(&ctx.workspace))
                .call(args.clone())
                .await;
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
        let listed = code_tools::ListFiles(Arc::clone(&ctx.workspace))
            .call(json!({}))
            .await;
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
        let read = code_tools::ReadFile(Arc::clone(&ctx.workspace))
            .call(json!({"path": "src/a.rs", "start_line": 2}))
            .await;
        assert_eq!(read.content, "    2|     let x = 1;\n    3| }\n");
        let found = code_tools::Search(Arc::clone(&ctx.workspace))
            .call(json!({"pattern": "let x"}))
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

    /// The tools of `ctx`'s workspace, editing with patches, with room
    /// for `files` changed files; `docs/old.md` and `notes.txt` are there
    /// too.
    async fn patching(ctx: &Arc<EditContext>, files: usize) -> Arc<EditContext> {
        for (path, text) in [("docs/old.md", "old\n"), ("notes.txt", "keep\n")] {
            ctx.workspace
                .write(&WorkspacePath::parse(path).unwrap(), text.as_bytes())
                .await
                .unwrap();
        }
        ctx.workspace.baseline().await.unwrap();
        Arc::new(EditContext {
            workspace: Arc::clone(&ctx.workspace),
            command_timeout: ctx.command_timeout,
            max_changed_files: files,
            bash: false,
            edit_format: EditFormat::Patch,
            state: Mutex::default(),
        })
    }

    const PATCH: &str = "*** Begin Patch
*** Add File: docs/new.md
+# New
*** Update File: src/a.rs
*** Move to: src/main.rs
@@ fn main() {
     let x = 1;
-    let x = 1;
+    let y = 2;
*** Delete File: docs/old.md
*** End Patch";

    #[tokio::test]
    async fn one_patch_adds_updates_moves_and_deletes() {
        let (_provider, plain) = workspace("henk-tools-patch").await;
        let ctx = patching(&plain, 10).await;
        let out = ApplyPatch(Arc::clone(&ctx))
            .call(json!({"patch": PATCH}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            out.content,
            "Added docs/new.md, moved src/a.rs to src/main.rs, deleted docs/old.md."
        );
        assert_eq!(content(&ctx, "docs/new.md").await, "# New\n");
        assert_eq!(
            content(&ctx, "src/main.rs").await,
            "fn main() {\n    let x = 1;\n    let y = 2;\n}\n"
        );
        let mut changes: Vec<(String, bool)> = ctx
            .workspace
            .export()
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.raw.path, c.raw.deleted))
            .collect();
        changes.sort();
        assert_eq!(
            changes,
            [
                ("docs/new.md".to_owned(), false),
                ("docs/old.md".to_owned(), true),
                ("src/a.rs".to_owned(), true),
                ("src/main.rs".to_owned(), false),
            ]
        );
        assert_eq!(ctx.state.lock().unwrap().written.len(), 4);
    }

    #[tokio::test]
    async fn a_refused_patch_changes_nothing() {
        let (_provider, plain) = workspace("henk-tools-patch-refused").await;
        let ctx = patching(&plain, 10).await;
        let bad_hunk = PATCH.replace("-    let x = 1;\n", "-    let z = 1;\n");
        for (patch, why) in [
            (bad_hunk.as_str(), "context is not in the file"),
            (
                "*** Begin Patch\n*** Add File: notes.txt\n+x\n*** End Patch",
                "already exists",
            ),
            (
                "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch",
                "does not exist",
            ),
            (
                "*** Begin Patch\n*** Add File: ../out.txt\n+x\n*** End Patch",
                "leaves the repository",
            ),
            ("not a patch", "*** Begin Patch"),
        ] {
            let out = ApplyPatch(Arc::clone(&ctx))
                .call(json!({ "patch": patch }))
                .await;
            assert!(out.is_error, "{patch}");
            assert!(out.content.contains(why), "{}", out.content);
            assert!(
                out.content.contains("nothing was changed"),
                "{}",
                out.content
            );
        }
        assert!(ctx.workspace.export().await.unwrap().is_empty());
        assert!(ctx.state.lock().unwrap().written.is_empty());
    }

    #[tokio::test]
    async fn moves_and_deletes_count_against_the_file_limit() {
        let (_provider, plain) = workspace("henk-tools-patch-limit").await;
        let ctx = patching(&plain, 3).await;
        let out = ApplyPatch(Arc::clone(&ctx))
            .call(json!({"patch": PATCH}))
            .await;
        assert!(out.content.contains("At most 3 files"), "{}", out.content);
        assert!(ctx.workspace.export().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_edit_format_picks_the_edit_tool() {
        let (_provider, plain) = workspace("henk-tools-format").await;
        let names = |ctx: &Arc<EditContext>| {
            edit_tools(ctx)
                .names()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let replace = names(&plain);
        assert!(replace.iter().any(|n| n == "edit_file"), "{replace:?}");
        assert!(!replace.iter().any(|n| n == "apply_patch"), "{replace:?}");
        let patch = names(&patching(&plain, 2).await);
        assert!(patch.iter().any(|n| n == "apply_patch"), "{patch:?}");
        assert!(!patch.iter().any(|n| n == "edit_file"), "{patch:?}");
        assert!(patch.iter().any(|n| n == "write_file"), "{patch:?}");
    }

    #[tokio::test]
    async fn the_checks_are_the_executor_s_own_with_bash_and_henk_has_none() {
        let (_provider, ctx) = workspace("henk-tools-checks").await;
        let names = |ctx: &Arc<EditContext>| {
            edit_tools(ctx)
                .names()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let without = names(&ctx);
        assert!(!without.iter().any(|n| n == "run_checks"), "{without:?}");
        assert!(!without.iter().any(|n| n == "bash"), "{without:?}");
        let with_bash = Arc::new(EditContext {
            workspace: Arc::clone(&ctx.workspace),
            command_timeout: ctx.command_timeout,
            max_changed_files: ctx.max_changed_files,
            bash: true,
            edit_format: ctx.edit_format,
            state: Mutex::default(),
        });
        let with = names(&with_bash);
        assert!(with.iter().any(|n| n == "bash"), "{with:?}");
        assert!(!with.iter().any(|n| n == "run_checks"), "{with:?}");
    }
}
