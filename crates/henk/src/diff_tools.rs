//! The read tools of the review loop's sessions (#284): the changed files
//! and their diffs, numbered, from the diff the review fetched once. A
//! requirements review (#535) swaps in the diff of the executor's changes
//! as they stand each round.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput};
use henk_domain::diff::ReviewDiff;
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

/// The diff of a review as the read tools serve it.
pub struct DiffFiles {
    diff: Mutex<Arc<ReviewDiff>>,
}

impl DiffFiles {
    /// Serves `diff`.
    #[must_use]
    pub fn new(diff: Arc<ReviewDiff>) -> Self {
        Self {
            diff: Mutex::new(diff),
        }
    }

    /// The diff now served.
    #[must_use]
    pub fn current(&self) -> Arc<ReviewDiff> {
        match self.diff.lock() {
            Ok(diff) => Arc::clone(&diff),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Serves `diff` from now on: a session keeps its tools across rounds,
    /// and the changes it reads move between them.
    pub fn set(&self, diff: Arc<ReviewDiff>) {
        match self.diff.lock() {
            Ok(mut served) => *served = diff,
            Err(poisoned) => *poisoned.into_inner() = diff,
        }
    }
}

/// `list_changed_files`: the files of the diff with their counts.
pub struct ListChangedFiles(pub Arc<DiffFiles>);

#[async_trait::async_trait]
impl Tool for ListChangedFiles {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_changed_files"),
            description: "Lists the files this change touches: status letter (A added, M modified, D removed, R renamed), path, lines added and removed. Start here, then read each file's diff with get_file_diff.".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        ToolOutput::ok(self.0.current().render_list())
    }
}

/// `get_file_diff`: the hunks of one or more changed files, with line
/// numbers on both sides.
pub struct GetFileDiff(pub Arc<DiffFiles>);

/// Characters of diff one `get_file_diff` call returns at most; the files
/// that would pass it are named so the model can ask for them next. The
/// first file asked for is always returned.
const FILE_DIFF_BATCH_CHARS: usize = 40_000;

#[async_trait::async_trait]
impl Tool for GetFileDiff {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("get_file_diff"),
            description: "The diff of changed files, every line numbered: old line number, new line number, then + for added, - for removed, space for context. Pass several paths at once to read them in one call. A finding goes on a line shown here, by its new line number (or old number with side LEFT for a removed line).".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Paths from list_changed_files; several at once"},
                    "path": {"type": "string", "description": "One path from list_changed_files"}
                }
            }),
        }
    }

    /// The diff is what a session reviews: other results are stubbed first.
    fn keep_in_context(&self) -> bool {
        true
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let mut paths: Vec<&str> = args
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if let Some(path) = arg_str(&args, "path") {
            paths.insert(0, path);
        }
        if paths.is_empty() {
            return ToolOutput::error("paths (or path) is required");
        }
        match render_files(&self.0, &paths, FILE_DIFF_BATCH_CHARS) {
            Ok(text) => ToolOutput::ok(text),
            Err(text) => ToolOutput::error(text),
        }
    }
}

/// The diffs of `paths`, in order, under a header each, until `cap`
/// characters; unknown paths get a note. An error when no path is part of the change.
fn render_files(files: &DiffFiles, paths: &[&str], cap: usize) -> Result<String, String> {
    let served = files.current();
    let diff = &served;
    let mut out = String::new();
    let mut unknown = Vec::new();
    let mut left_out = Vec::new();
    let mut returned = 0_usize;
    for path in paths {
        let Some(file) = diff.file(path) else {
            unknown.push(*path);
            continue;
        };
        let rendered = file.render();
        if returned > 0 && out.len() + rendered.len() > cap {
            left_out.push(file.path.as_str());
            continue;
        }
        if returned > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "== {} ==", file.path);
        out.push_str(&rendered);
        returned += 1;
    }
    if returned == 0 {
        return Err(format!(
            "{} is not part of this change. The changed files are:\n{}",
            unknown.join(", "),
            diff.render_list()
        ));
    }
    if !unknown.is_empty() {
        let _ = write!(out, "\nNot part of this change: {}.", unknown.join(", "));
    }
    if !left_out.is_empty() {
        let _ = write!(
            out,
            "\nNot included, too long for one call; ask again: {}.",
            left_out.join(", ")
        );
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::diff::ReviewDiff;

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
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -5 +5 @@
-old
+new
";

    #[tokio::test]
    async fn list_and_file_diff_serve_the_changed_files() {
        let files = Arc::new(DiffFiles::new(Arc::new(ReviewDiff::from_unified(DIFF))));
        let list = ListChangedFiles(Arc::clone(&files)).call(json!({})).await;
        assert!(!list.is_error);
        assert!(list.content.contains("M src/a.rs (+1 -1)"));

        let file = GetFileDiff(Arc::clone(&files))
            .call(json!({"path": "src/a.rs"}))
            .await;
        assert!(!file.is_error, "{file:?}");
        assert!(
            file.content.contains("    2 |      |-    let x = 1;"),
            "{}",
            file.content
        );

        let missing = GetFileDiff(Arc::clone(&files))
            .call(json!({"path": "src/zzz.rs"}))
            .await;
        assert!(missing.is_error);
        assert!(missing.content.contains("README.md"));
    }

    #[tokio::test]
    async fn the_tools_serve_the_diff_they_were_last_given() {
        let files = Arc::new(DiffFiles::new(Arc::new(ReviewDiff::from_unified(DIFF))));
        let list = ListChangedFiles(Arc::clone(&files));
        assert!(!list.call(json!({})).await.is_error);
        assert!(list.call(json!({})).await.content.contains("src/"));

        files.set(Arc::new(ReviewDiff::from_unified("")));
        assert_eq!(list.call(json!({})).await.content, "The diff is empty.");
        let diff = GetFileDiff(Arc::clone(&files))
            .call(json!({"path": "src/lib.rs"}))
            .await;
        assert!(diff.is_error, "{diff:?}");
    }

    #[tokio::test]
    async fn get_file_diff_reads_several_files_in_one_call() {
        let files = Arc::new(DiffFiles::new(Arc::new(ReviewDiff::from_unified(DIFF))));
        let both = GetFileDiff(Arc::clone(&files))
            .call(json!({"paths": ["src/a.rs", "README.md"]}))
            .await;
        assert!(!both.is_error, "{both:?}");
        assert!(
            both.content.starts_with("== src/a.rs =="),
            "{}",
            both.content
        );
        assert!(both.content.contains("== README.md =="));

        let mixed = GetFileDiff(Arc::clone(&files))
            .call(json!({"paths": ["src/zzz.rs", "README.md"]}))
            .await;
        assert!(!mixed.is_error);
        assert!(
            mixed
                .content
                .ends_with("Not part of this change: src/zzz.rs."),
            "{}",
            mixed.content
        );

        let none = GetFileDiff(Arc::clone(&files))
            .call(json!({"paths": ["src/zzz.rs"]}))
            .await;
        assert!(none.is_error);
        assert!(
            none.content.contains("README.md"),
            "the file list comes back"
        );

        assert!(
            GetFileDiff(Arc::clone(&files))
                .call(json!({}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn a_batch_stops_at_the_cap_and_names_what_it_left_out() {
        let files = Arc::new(DiffFiles::new(Arc::new(ReviewDiff::from_unified(DIFF))));
        let text = render_files(&files, &["src/a.rs", "README.md"], 10).unwrap();
        assert!(
            text.starts_with("== src/a.rs =="),
            "the first file always comes"
        );
        assert!(!text.contains("== README.md =="));
        assert!(
            text.ends_with("Not included, too long for one call; ask again: README.md."),
            "{text}"
        );
    }
}
