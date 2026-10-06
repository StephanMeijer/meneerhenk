//! The code tools every lane with a workspace gets (#90): list, read and
//! search the files of its own copy of the repository. An address run's
//! copy is its checkout (§3.5); a review lane's and the fact-checker's is
//! the reviewed commit (#170). The tools only read, talk only to a
//! [`Workspace`], and what they return is text from the repository: data
//! for the model, never instructions (§8.3).

use std::fmt::Write as _;
use std::sync::Arc;

use henk_agent::{Tool, ToolOutput, ToolSet};
use henk_domain::address::WorkspacePath;
use henk_domain::ignore::PathFilter;
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

use crate::workspace::{Pattern, Workspace};

/// Lines `read_file` returns at most per call.
pub const READ_LINES: usize = 400;
/// Files larger than this are not read or searched.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Entries `list_files` returns at most.
const LIST_CAP: usize = 500;
/// Hits `search` returns at most.
const SEARCH_CAP: usize = 200;
/// Characters of a hit's line that `search` shows.
const HIT_CHARS: usize = 240;
/// Characters a glob may have, and how many wildcards: matching is
/// backtracking, so a model's glob is kept small.
const GLOB_LENGTH: usize = 200;
const GLOB_WILDCARDS: usize = 16;

/// Adds `list_files`, `read_file` and `search` on `workspace`.
pub fn add(set: &mut ToolSet, workspace: &Arc<dyn Workspace>) {
    set.add(ListFiles(Arc::clone(workspace)))
        .add(ReadFile(Arc::clone(workspace)))
        .add(Search(Arc::clone(workspace)));
}

fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_line(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// The `dir` argument, the root when absent.
fn dir(args: &Value) -> Result<WorkspacePath, String> {
    WorkspacePath::parse_dir(arg_str(args, "dir").unwrap_or("")).map_err(|e| e.to_string())
}

/// The `glob` argument, checked: none when absent or blank.
fn glob(args: &Value) -> Result<Option<PathFilter>, String> {
    let Some(glob) = arg_str(args, "glob")
        .map(str::trim)
        .filter(|g| !g.is_empty())
    else {
        return Ok(None);
    };
    if glob.chars().count() > GLOB_LENGTH {
        return Err(format!("the glob is longer than {GLOB_LENGTH} characters"));
    }
    if glob.chars().filter(|c| matches!(c, '*' | '?')).count() > GLOB_WILDCARDS {
        return Err(format!("the glob has more than {GLOB_WILDCARDS} wildcards"));
    }
    Ok(Some(PathFilter::new([glob])))
}

/// `line`, cut to [`HIT_CHARS`] characters.
fn cut(line: &str) -> String {
    match line.char_indices().nth(HIT_CHARS) {
        Some((at, _)) => format!("{} [cut]", line.get(..at).unwrap_or(line)),
        None => line.to_owned(),
    }
}

/// `list_files`.
pub struct ListFiles(pub Arc<dyn Workspace>);
/// `read_file`.
pub struct ReadFile(pub Arc<dyn Workspace>);
/// `search`.
pub struct Search(pub Arc<dyn Workspace>);

#[async_trait::async_trait]
impl Tool for ListFiles {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_files"),
            description: format!(
                "Lists the files of the repository under `dir` (default: all), sorted, at most {LIST_CAP}. `glob` keeps only matching paths: `*.rs` matches the name at any depth, `src/**/*.rs` the whole path."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dir": {"type": "string", "description": "Directory in the repository"},
                    "glob": {"type": "string", "description": "Only paths matching this glob"}
                },
                "required": []
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let (dir, only) = match (dir(&args), glob(&args)) {
            (Ok(dir), Ok(only)) => (dir, only),
            (Err(error), _) | (_, Err(error)) => return ToolOutput::error(error),
        };
        let mut files = match self.0.list(&dir, only.as_ref(), LIST_CAP + 1).await {
            Ok(files) => files,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let more = files.len() > LIST_CAP;
        files.truncate(LIST_CAP);
        let mut text = String::new();
        for file in &files {
            let _ = writeln!(text, "{file}");
        }
        if more {
            let _ = writeln!(
                text,
                "[more than {LIST_CAP} files; list a subdirectory or narrow the glob]"
            );
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
                "Reads a line range of a text file in the repository, numbered. At most {READ_LINES} lines per call; ask for the range you need."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path in the repository"},
                    "start_line": {"type": "integer", "description": "First line, 1-based (default 1)"},
                    "end_line": {"type": "integer", "description": format!("Last line, inclusive (default start_line + {})", READ_LINES - 1)}
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let path = match WorkspacePath::parse(arg_str(&args, "path").unwrap_or("")) {
            Ok(path) => path,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let start = arg_line(&args, "start_line").unwrap_or(1);
        if start == 0 {
            return ToolOutput::error("start_line is 1-based; the first line is 1.");
        }
        let last = start.saturating_add(READ_LINES - 1);
        let end = match arg_line(&args, "end_line") {
            Some(end) if end < start => {
                return ToolOutput::error(format!(
                    "end_line {end} is before start_line {start}. Ask for start_line <= end_line, at most {READ_LINES} lines."
                ));
            }
            Some(end) => end.min(last),
            None => last,
        };
        let bytes = match self.0.read(&path, MAX_FILE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => return ToolOutput::error(format!("cannot read {path}: {error}")),
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => text,
            _ => return ToolOutput::error(format!("{path} is not a text file")),
        };
        let total = text.lines().count();
        if total == 0 {
            return ToolOutput::ok(format!("{path} is empty."));
        }
        if start > total {
            return ToolOutput::ok(format!(
                "{path} has {total} lines; nothing at line {start}."
            ));
        }
        let mut out = String::new();
        for (index, line) in text
            .lines()
            .enumerate()
            .skip(start - 1)
            .take(end - start + 1)
        {
            let _ = writeln!(out, "{:>5}| {line}", index + 1);
        }
        if end < total {
            let _ = writeln!(
                out,
                "[{total} lines; continue with start_line = {}]",
                end + 1
            );
        }
        ToolOutput::ok(out)
    }
}

#[async_trait::async_trait]
impl Tool for Search {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("search"),
            description: format!(
                "Finds lines matching a regular expression in the text files of the repository, as path:line: text, at most {SEARCH_CAP}. The syntax is the common one: classes, \\b, \\d, \\w, (?i) for any case, alternation with |. `glob` keeps only matching files, `dir` a directory."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression, matched per line"},
                    "glob": {"type": "string", "description": "Only files matching this glob, such as *.rs"},
                    "dir": {"type": "string", "description": "Directory in the repository"}
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let pattern = match Pattern::parse(arg_str(&args, "pattern").unwrap_or("")) {
            Ok(pattern) => pattern,
            Err(error) => return ToolOutput::error(error),
        };
        let (dir, only) = match (dir(&args), glob(&args)) {
            (Ok(dir), Ok(only)) => (dir, only),
            (Err(error), _) | (_, Err(error)) => return ToolOutput::error(error),
        };
        let mut hits = match self
            .0
            .search(
                &dir,
                &pattern,
                only.as_ref(),
                MAX_FILE_BYTES,
                SEARCH_CAP + 1,
            )
            .await
        {
            Ok(hits) => hits,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let more = hits.len() > SEARCH_CAP;
        hits.truncate(SEARCH_CAP);
        let mut out = String::new();
        for hit in &hits {
            let _ = writeln!(out, "{}:{}: {}", hit.path, hit.line, cut(&hit.text));
        }
        if more {
            let _ = writeln!(
                out,
                "[more than {SEARCH_CAP} matches; narrow the pattern or the glob]"
            );
        }
        ToolOutput::ok(if out.is_empty() {
            "No matches.".to_owned()
        } else {
            out
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use henk_domain::workspace::Profile;

    use super::*;
    use crate::git::ScratchDir;
    use crate::workspace::WorkspaceProvider as _;
    use crate::workspace::fake::FakeProvider;

    /// A fake workspace holding a few files, and its tools.
    async fn tools(name: &str) -> (Arc<dyn Workspace>, ScratchDir) {
        let dir = ScratchDir::new(name).unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "fn main() {\n    let x = 1;\n    Parse(x);\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/deep/b.rs"), "fn parse_all() {}\n").unwrap();
        std::fs::write(root.join("docs/notes.md"), "parse the input\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"a\0b").unwrap();
        let long = (1..=450).fold(String::new(), |mut text, n| {
            let _ = writeln!(text, "line {n}");
            text
        });
        std::fs::write(root.join("long.txt"), long).unwrap();
        std::fs::write(root.join("wide.txt"), format!("{}\n", "w".repeat(300))).unwrap();
        let ws = FakeProvider::default()
            .open(root, &Profile::default())
            .await
            .unwrap();
        (ws, dir)
    }

    #[tokio::test]
    async fn read_file_reads_a_numbered_range_and_says_how_to_go_on() {
        let (ws, _dir) = tools("henk-code-read").await;
        let read = ReadFile(Arc::clone(&ws));
        let out = read
            .call(json!({"path": "src/a.rs", "start_line": 2, "end_line": 3}))
            .await;
        assert_eq!(
            out.content,
            "    2|     let x = 1;\n    3|     Parse(x);\n[4 lines; continue with start_line = 4]\n"
        );
        let out = read.call(json!({"path": "long.txt"})).await;
        assert!(
            out.content.starts_with("    1| line 1\n"),
            "{}",
            out.content
        );
        assert!(
            out.content
                .ends_with("  400| line 400\n[450 lines; continue with start_line = 401]\n"),
            "{}",
            out.content
        );
        let out = read
            .call(json!({"path": "long.txt", "start_line": 401}))
            .await;
        assert!(
            out.content.ends_with("  450| line 450\n"),
            "{}",
            out.content
        );
        assert!(read.call(json!({"path": "blob.bin"})).await.is_error);
        assert!(
            read.call(json!({"path": "src/a.rs", "start_line": 0}))
                .await
                .is_error
        );
        assert!(
            read.call(json!({"path": "src/a.rs", "start_line": 3, "end_line": 2}))
                .await
                .is_error
        );
        for escape in ["../outside", "/etc/passwd", ".git/config", "nope.rs"] {
            assert!(
                read.call(json!({"path": escape})).await.is_error,
                "{escape}"
            );
        }
        let past = read
            .call(json!({"path": "src/a.rs", "start_line": 9}))
            .await;
        assert_eq!(past.content, "src/a.rs has 4 lines; nothing at line 9.");
    }

    #[tokio::test]
    async fn list_files_takes_a_directory_and_a_glob() {
        let (ws, _dir) = tools("henk-code-list").await;
        let list = ListFiles(Arc::clone(&ws));
        assert_eq!(
            list.call(json!({"glob": "*.rs"})).await.content,
            "src/a.rs\nsrc/deep/b.rs\n"
        );
        assert_eq!(
            list.call(json!({"dir": "src", "glob": "src/*.rs"}))
                .await
                .content,
            "src/a.rs\n"
        );
        assert_eq!(
            list.call(json!({"glob": "*.py"})).await.content,
            "No files."
        );
        assert!(list.call(json!({"dir": "../up"})).await.is_error);
        assert!(
            list.call(json!({"glob": "*".repeat(GLOB_WILDCARDS + 1)}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn search_matches_a_regular_expression_in_matching_files() {
        let (ws, _dir) = tools("henk-code-search").await;
        let search = Search(Arc::clone(&ws));
        assert_eq!(
            search
                .call(json!({"pattern": "(?i)\\bparse\\w*\\(", "glob": "*.rs"}))
                .await
                .content,
            "src/a.rs:3: Parse(x);\nsrc/deep/b.rs:1: fn parse_all() {}\n"
        );
        assert_eq!(
            search
                .call(json!({"pattern": "let x = \\d+"}))
                .await
                .content,
            "src/a.rs:2: let x = 1;\n"
        );
        assert_eq!(
            search
                .call(json!({"pattern": "parse", "dir": "docs"}))
                .await
                .content,
            "docs/notes.md:1: parse the input\n"
        );
        let wide = search.call(json!({"pattern": "^w+$"})).await.content;
        assert_eq!(
            wide,
            format!("wide.txt:1: {} [cut]\n", "w".repeat(HIT_CHARS))
        );
        let many = search.call(json!({"pattern": "^line"})).await.content;
        assert!(
            many.ends_with(&format!(
                "[more than {SEARCH_CAP} matches; narrow the pattern or the glob]\n"
            )),
            "{many}"
        );
        assert_eq!(many.lines().count(), SEARCH_CAP + 1);
        for bad in ["", "(", "[a-z&&[^aeiou]]"] {
            let out = search.call(json!({"pattern": bad})).await;
            assert!(out.is_error, "{bad:?}: {}", out.content);
        }
    }

    #[tokio::test]
    async fn every_description_is_in_style() {
        let (ws, _dir) = tools("henk-code-style").await;
        let mut set = ToolSet::new();
        add(&mut set, &ws);
        let definitions = set.definitions();
        assert_eq!(definitions.len(), 3);
        for definition in definitions {
            assert!(
                henk_domain::text::is_in_style(&definition.description),
                "{}: {}",
                definition.name,
                definition.description
            );
        }
    }
}
