//! The diff of a review, per file, with line numbers (§3.2).
//!
//! Henk fetches the diff once and hands it to lanes file by file, numbered,
//! so a model never counts lines in a unified diff and a finding can be
//! checked against the diff before anything is posted.

use std::fmt;
use std::fmt::Write as _;

use crate::ignore::PathFilter;

/// Which side of the diff a line comment sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    /// The old version.
    Left,
    /// The new version.
    Right,
}

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// New in this change.
    Added,
    /// Changed in place.
    Modified,
    /// Gone after this change.
    Removed,
    /// Moved, possibly with changes.
    Renamed,
}

impl FileStatus {
    /// One letter, as `git status` would print it.
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Removed => 'D',
            Self::Renamed => 'R',
        }
    }
}

/// One file's patch as a platform hands it over: paths, status and the
/// hunk text (`@@` lines onwards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// Path before the change, `None` for an added file.
    pub old_path: Option<String>,
    /// Path after the change, `None` for a removed file.
    pub new_path: Option<String>,
    /// What happened to it.
    pub status: FileStatus,
    /// The hunks, unified format. Empty for a binary or empty change.
    pub patch: String,
}

/// What a line in a hunk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged, present on both sides.
    Context,
    /// Present on the new side only.
    Added,
    /// Present on the old side only.
    Removed,
}

/// One line of a hunk with its numbers on each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// What it is.
    pub kind: LineKind,
    /// Line number in the old file, when it exists there.
    pub old_no: Option<u32>,
    /// Line number in the new file, when it exists there.
    pub new_no: Option<u32>,
    /// The text without the leading marker.
    pub text: String,
}

/// One hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First old line.
    pub old_start: u32,
    /// First new line.
    pub new_start: u32,
    /// The lines.
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The new-side line range this hunk covers, when it has any.
    #[must_use]
    pub fn new_range(&self) -> Option<(u32, u32)> {
        let numbers = self.lines.iter().filter_map(|l| l.new_no);
        let first = numbers.clone().min()?;
        let last = numbers.max()?;
        Some((first, last))
    }
}

/// One file of the diff, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The path a finding refers to: the new path, or the old one for a
    /// removed file.
    pub path: String,
    /// The old path when it differs from `path`.
    pub old_path: Option<String>,
    /// What happened to it.
    pub status: FileStatus,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// The hunks, in order.
    pub hunks: Vec<Hunk>,
}

/// The diff of one review: the files it reviews, and the paths of changed
/// files it leaves out (`review.ignore`, §3.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewDiff {
    files: Vec<FileDiff>,
    ignored: Vec<String>,
}

/// Why a line cannot carry a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotCommentable {
    /// The path is not in the diff.
    UnknownPath {
        /// The paths that are.
        known: Vec<String>,
    },
    /// The line is not in any hunk of that file on that side.
    LineOutsideHunks {
        /// The side asked for.
        side: DiffSide,
        /// The new-side ranges of the file's hunks.
        ranges: Vec<(u32, u32)>,
    },
}

impl fmt::Display for NotCommentable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPath { known } => {
                write!(
                    f,
                    "that path is not part of the diff; the changed files are: {}",
                    known.join(", ")
                )
            }
            Self::LineOutsideHunks { side, ranges } => {
                let ranges: Vec<String> = ranges.iter().map(|(a, b)| format!("{a}-{b}")).collect();
                let side = match side {
                    DiffSide::Left => "old",
                    DiffSide::Right => "new",
                };
                write!(
                    f,
                    "that {side} line is not in the diff of that file; the diff covers new lines {}",
                    ranges.join(", ")
                )
            }
        }
    }
}

/// Splits a unified diff (as `git diff` or GitHub's compare endpoint
/// produce it) into per-file patches.
#[must_use]
pub fn split_unified(text: &str) -> Vec<FilePatch> {
    let mut patches = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            if let Some(patch) = parse_section(&current) {
                patches.push(patch);
            }
            current.clear();
        }
        current.push(line);
    }
    if !current.is_empty()
        && let Some(patch) = parse_section(&current)
    {
        patches.push(patch);
    }
    patches
}

/// One `diff --git` section: header lines, then hunks.
fn parse_section(lines: &[&str]) -> Option<FilePatch> {
    let first = lines.first()?;
    let (mut old_path, mut new_path) = git_header_paths(first);
    let mut status = FileStatus::Modified;
    let mut hunk_start = lines.len();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("@@ ") {
            hunk_start = index;
            break;
        }
        if line.starts_with("new file mode") {
            status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            status = FileStatus::Removed;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            status = FileStatus::Renamed;
            old_path = Some(from.to_owned());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            status = FileStatus::Renamed;
            new_path = Some(to.to_owned());
        } else if let Some(path) = line.strip_prefix("--- ") {
            if path == "/dev/null" {
                status = FileStatus::Added;
                old_path = None;
            } else {
                old_path = Some(strip_prefix_letter(path));
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if path == "/dev/null" {
                status = FileStatus::Removed;
                new_path = None;
            } else {
                new_path = Some(strip_prefix_letter(path));
            }
        }
    }
    match status {
        FileStatus::Added => old_path = None,
        FileStatus::Removed => new_path = None,
        FileStatus::Modified | FileStatus::Renamed => {}
    }
    if old_path.is_none() && new_path.is_none() {
        return None;
    }
    let patch = lines.get(hunk_start..).unwrap_or_default().join("\n");
    Some(FilePatch {
        old_path,
        new_path,
        status,
        patch,
    })
}

/// `diff --git a/x b/y` gives both paths; the fallback for binary files
/// and other sections without `---`/`+++` lines.
fn git_header_paths(line: &str) -> (Option<String>, Option<String>) {
    let rest = line.strip_prefix("diff --git ").unwrap_or_default();
    let Some(a) = rest.strip_prefix("a/") else {
        return (None, None);
    };
    match a.find(" b/") {
        Some(at) => {
            let old = a.get(..at).unwrap_or_default().to_owned();
            let new = a.get(at + 3..).unwrap_or_default().to_owned();
            (Some(old), Some(new))
        }
        None => (None, None),
    }
}

/// `a/path` or `b/path` to `path`; a trailing tab and timestamp is dropped.
fn strip_prefix_letter(path: &str) -> String {
    let path = path.split('\t').next().unwrap_or_default();
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_owned()
}

impl FileDiff {
    /// Parses the hunks of one patch.
    #[must_use]
    pub fn from_patch(patch: &FilePatch) -> Self {
        let file_path = patch
            .new_path
            .clone()
            .or_else(|| patch.old_path.clone())
            .unwrap_or_default();
        let old_path = patch
            .old_path
            .clone()
            .filter(|old| *old != file_path && patch.new_path.is_some());
        let mut hunks = Vec::new();
        let mut additions = 0;
        let mut deletions = 0;
        let mut current: Option<(Hunk, u32, u32)> = None;
        for line in patch.patch.lines() {
            if let Some(header) = line.strip_prefix("@@ ") {
                if let Some((hunk, _, _)) = current.take() {
                    hunks.push(hunk);
                }
                let (old_start, new_start) = parse_hunk_header(header);
                current = Some((
                    Hunk {
                        old_start,
                        new_start,
                        lines: Vec::new(),
                    },
                    old_start,
                    new_start,
                ));
                continue;
            }
            let Some((hunk, old_next, new_next)) = current.as_mut() else {
                continue;
            };
            let mut chars = line.chars();
            let marker = chars.next();
            let text: String = chars.collect();
            match marker {
                Some(' ') => {
                    hunk.lines.push(DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(*old_next),
                        new_no: Some(*new_next),
                        text,
                    });
                    *old_next += 1;
                    *new_next += 1;
                }
                Some('+') => {
                    hunk.lines.push(DiffLine {
                        kind: LineKind::Added,
                        old_no: None,
                        new_no: Some(*new_next),
                        text,
                    });
                    *new_next += 1;
                    additions += 1;
                }
                Some('-') => {
                    hunk.lines.push(DiffLine {
                        kind: LineKind::Removed,
                        old_no: Some(*old_next),
                        new_no: None,
                        text,
                    });
                    *old_next += 1;
                    deletions += 1;
                }
                // "\ No newline at end of file" and anything unexpected.
                _ => {}
            }
        }
        if let Some((hunk, _, _)) = current {
            hunks.push(hunk);
        }
        Self {
            path: file_path,
            old_path,
            status: patch.status,
            additions,
            deletions,
            hunks,
        }
    }

    /// Whether `line` on `side` is a line of this file's diff.
    #[must_use]
    pub fn has_line(&self, line: u32, side: DiffSide) -> bool {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| match side {
                DiffSide::Right => l.new_no == Some(line),
                DiffSide::Left => l.old_no == Some(line),
            })
    }

    /// The hunks as text, one numbered line each: old number, new number,
    /// marker, text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} {} (+{} -{})",
            self.status.letter(),
            self.describe_path(),
            self.additions,
            self.deletions
        );
        if self.hunks.is_empty() {
            let _ = writeln!(out, "(no text hunks: binary or empty change)");
            return out;
        }
        let _ = writeln!(out, "  old |  new |");
        for hunk in &self.hunks {
            let _ = writeln!(out, "@@ -{} +{} @@", hunk.old_start, hunk.new_start);
            for line in &hunk.lines {
                let marker = match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                };
                let _ = writeln!(
                    out,
                    "{:>5} |{:>5} |{marker}{}",
                    line.old_no.map_or(String::new(), |n| n.to_string()),
                    line.new_no.map_or(String::new(), |n| n.to_string()),
                    line.text
                );
            }
        }
        out
    }

    fn describe_path(&self) -> String {
        match &self.old_path {
            Some(old) => format!("{old} -> {}", self.path),
            None => self.path.clone(),
        }
    }
}

/// `-12,5 +14,6 @@ context` to `(12, 14)`.
fn parse_hunk_header(header: &str) -> (u32, u32) {
    let mut old_start = 1;
    let mut new_start = 1;
    for part in header.split_whitespace().take(2) {
        let (sign, numbers) = part.split_at(1);
        let start = numbers
            .split(',')
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(1);
        match sign {
            "-" => old_start = start,
            "+" => new_start = start,
            _ => {}
        }
    }
    (old_start, new_start)
}

impl ReviewDiff {
    /// Parses every patch.
    #[must_use]
    pub fn from_patches(patches: &[FilePatch]) -> Self {
        Self::from_patches_filtered(patches, &PathFilter::default())
    }

    /// Parses every patch, leaving out the files `ignore` matches. They are
    /// listed as not reviewed and can carry no finding.
    #[must_use]
    pub fn from_patches_filtered(patches: &[FilePatch], ignore: &PathFilter) -> Self {
        let (ignored, kept): (Vec<FileDiff>, Vec<FileDiff>) = patches
            .iter()
            .map(FileDiff::from_patch)
            .partition(|file| ignore.matches(&file.path));
        Self {
            files: kept,
            ignored: ignored.into_iter().map(|file| file.path).collect(),
        }
    }

    /// Parses a whole unified diff.
    #[must_use]
    pub fn from_unified(text: &str) -> Self {
        Self::from_patches(&split_unified(text))
    }

    /// The files, in diff order.
    #[must_use]
    pub fn files(&self) -> &[FileDiff] {
        &self.files
    }

    /// Whether the diff has no file to review.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Whether any file changed at all, reviewed or not.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        !self.files.is_empty() || !self.ignored.is_empty()
    }

    /// The changed files left out of the review, in diff order.
    #[must_use]
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    /// One file by its path (the new path, or the old one for a removed file).
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&FileDiff> {
        let path = path.trim().trim_start_matches("./");
        self.files.iter().find(|f| f.path == path)
    }

    /// The paths findings may refer to.
    pub fn paths(&self) -> impl Iterator<Item = &str> + '_ {
        self.files.iter().map(|f| f.path.as_str())
    }

    /// Whether `line` of `path` on `side` is part of the diff, with the
    /// reason when it is not.
    ///
    /// # Errors
    ///
    /// Returns [`NotCommentable`] for an unknown path or a line outside the
    /// file's hunks.
    pub fn commentable(&self, path: &str, line: u32, side: DiffSide) -> Result<(), NotCommentable> {
        let Some(file) = self.file(path) else {
            return Err(NotCommentable::UnknownPath {
                known: self.paths().map(str::to_owned).collect(),
            });
        };
        if file.has_line(line, side) {
            return Ok(());
        }
        Err(NotCommentable::LineOutsideHunks {
            side,
            ranges: file.hunks.iter().filter_map(Hunk::new_range).collect(),
        })
    }

    /// One line per file: status letter, path, counts; then the files left
    /// out, if any.
    #[must_use]
    pub fn render_list(&self) -> String {
        if !self.has_changes() {
            return "The diff is empty.".to_owned();
        }
        let mut out = String::new();
        if self.files.is_empty() {
            out.push_str("No file to review: every changed file is left out.\n");
        }
        for file in &self.files {
            let _ = writeln!(
                out,
                "{} {} (+{} -{})",
                file.status.letter(),
                file.describe_path(),
                file.additions,
                file.deletions
            );
        }
        if !self.ignored.is_empty() {
            let _ = writeln!(
                out,
                "Not reviewed (review.ignore): {}",
                self.ignored.join(", ")
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@
 fn main() {
-    let x = 1;
+    let x = 2;
+    let y = 3;
     println!(\"{x}\");
 }
@@ -20,3 +21,3 @@ fn other() {
     a();
-    b();
+    c();
     d();
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 000..333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,2 @@
+# New
+text
\\ No newline at end of file
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 444..000
--- a/old.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-gone
diff --git a/from.rs b/to.rs
similarity index 90%
rename from from.rs
rename to to.rs
index 555..666 100644
--- a/from.rs
+++ b/to.rs
@@ -3,2 +3,2 @@
-old
+new
 keep
diff --git a/img.png b/img.png
new file mode 100644
index 000..777
Binary files /dev/null and b/img.png differ
";

    #[test]
    fn splits_sections_with_status_and_paths() {
        let patches = split_unified(SAMPLE);
        assert_eq!(patches.len(), 5);
        assert_eq!(patches[0].status, FileStatus::Modified);
        assert_eq!(patches[0].new_path.as_deref(), Some("src/a.rs"));
        assert_eq!(patches[1].status, FileStatus::Added);
        assert_eq!(patches[1].old_path, None);
        assert_eq!(patches[2].status, FileStatus::Removed);
        assert_eq!(patches[2].new_path, None);
        assert_eq!(patches[2].old_path.as_deref(), Some("old.txt"));
        assert_eq!(patches[3].status, FileStatus::Renamed);
        assert_eq!(patches[3].old_path.as_deref(), Some("from.rs"));
        assert_eq!(patches[3].new_path.as_deref(), Some("to.rs"));
        assert_eq!(patches[4].status, FileStatus::Added);
        assert_eq!(patches[4].new_path.as_deref(), Some("img.png"));
        assert!(patches[4].patch.is_empty(), "binary has no hunks");
        assert!(patches[0].patch.starts_with("@@ -1,4 +1,5 @@"));
    }

    #[test]
    fn numbers_lines_on_both_sides() {
        let diff = ReviewDiff::from_unified(SAMPLE);
        let a = diff.file("src/a.rs").unwrap();
        assert_eq!(a.additions, 3);
        assert_eq!(a.deletions, 2);
        assert_eq!(a.hunks.len(), 2);
        let lines = &a.hunks[0].lines;
        assert_eq!(
            lines[0],
            DiffLine {
                kind: LineKind::Context,
                old_no: Some(1),
                new_no: Some(1),
                text: "fn main() {".into()
            }
        );
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[1].old_no, Some(2));
        assert_eq!(lines[2].new_no, Some(2));
        assert_eq!(lines[3].new_no, Some(3));
        assert_eq!(
            lines[4],
            DiffLine {
                kind: LineKind::Context,
                old_no: Some(3),
                new_no: Some(4),
                text: "    println!(\"{x}\");".into()
            }
        );
        assert_eq!(a.hunks[1].new_range(), Some((21, 23)));
        let new = diff.file("docs/new.md").unwrap();
        assert_eq!(new.additions, 2);
        assert_eq!(
            new.hunks[0].lines.len(),
            2,
            "the no-newline marker is not a line"
        );
        let renamed = diff.file("to.rs").unwrap();
        assert_eq!(renamed.old_path.as_deref(), Some("from.rs"));
        assert!(diff.file("from.rs").is_none());
        let removed = diff.file("old.txt").unwrap();
        assert_eq!(removed.status, FileStatus::Removed);
        assert_eq!(removed.deletions, 1);
    }

    #[test]
    fn commentable_checks_path_side_and_line() {
        let diff = ReviewDiff::from_unified(SAMPLE);
        assert_eq!(diff.commentable("src/a.rs", 2, DiffSide::Right), Ok(()));
        assert_eq!(
            diff.commentable("src/a.rs", 4, DiffSide::Right),
            Ok(()),
            "context lines count"
        );
        assert_eq!(
            diff.commentable("src/a.rs", 2, DiffSide::Left),
            Ok(()),
            "removed line on the old side"
        );
        assert_eq!(diff.commentable("./src/a.rs", 22, DiffSide::Right), Ok(()));
        let outside = diff
            .commentable("src/a.rs", 10, DiffSide::Right)
            .unwrap_err();
        assert_eq!(
            outside,
            NotCommentable::LineOutsideHunks {
                side: DiffSide::Right,
                ranges: vec![(1, 5), (21, 23)]
            }
        );
        assert!(outside.to_string().contains("1-5, 21-23"));
        let unknown = diff
            .commentable("src/zzz.rs", 1, DiffSide::Right)
            .unwrap_err();
        assert!(matches!(unknown, NotCommentable::UnknownPath { ref known } if known.len() == 5));
        assert!(unknown.to_string().contains("src/a.rs"));
        assert!(diff.commentable("old.txt", 1, DiffSide::Left).is_ok());
        assert!(diff.commentable("old.txt", 1, DiffSide::Right).is_err());
        assert!(diff.commentable("img.png", 1, DiffSide::Right).is_err());
    }

    #[test]
    fn renders_list_and_numbered_file() {
        let diff = ReviewDiff::from_unified(SAMPLE);
        let list = diff.render_list();
        assert!(list.contains("M src/a.rs (+3 -2)\n"), "{list}");
        assert!(list.contains("A docs/new.md (+2 -0)\n"));
        assert!(list.contains("D old.txt (+0 -1)\n"));
        assert!(list.contains("R from.rs -> to.rs (+1 -1)\n"));
        let text = diff.file("src/a.rs").unwrap().render();
        assert!(text.starts_with("M src/a.rs (+3 -2)\n"), "{text}");
        assert!(text.contains("    2 |      |-    let x = 1;\n"), "{text}");
        assert!(text.contains("      |    2 |+    let x = 2;\n"), "{text}");
        assert!(text.contains("    3 |    4 |     println!"), "{text}");
        assert!(text.contains("@@ -20 +21 @@\n"));
        let binary = diff.file("img.png").unwrap().render();
        assert!(binary.contains("binary or empty"));
        assert_eq!(ReviewDiff::default().render_list(), "The diff is empty.");
    }

    #[test]
    fn hunk_headers_without_counts_and_odd_paths() {
        assert_eq!(parse_hunk_header("-1 +1 @@"), (1, 1));
        assert_eq!(parse_hunk_header("-12,5 +14,6 @@ fn x()"), (12, 14));
        assert_eq!(
            strip_prefix_letter("b/path/with space.rs\t2026-01-01"),
            "path/with space.rs"
        );
        assert_eq!(
            git_header_paths("diff --git a/x/y.rs b/x/y.rs"),
            (Some("x/y.rs".into()), Some("x/y.rs".into()))
        );
        assert_eq!(git_header_paths("diff --cc merged"), (None, None));
        assert!(split_unified("").is_empty());
        assert!(split_unified("not a diff at all\n").is_empty());
    }
}
