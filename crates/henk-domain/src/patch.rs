//! The Codex patch envelope (#561): how a model trained on `apply_patch`
//! describes several file changes at once, and applying one change to a
//! file's text.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: docs/new.md
//! +text
//! *** Update File: src/a.rs
//! *** Move to: src/b.rs
//! @@ fn main() {
//!      let x = 1;
//! -    let y = 2;
//! +    let y = 3;
//! *** Delete File: old.txt
//! *** End Patch
//! ```
//!
//! A hunk is found by its context, never by line numbers. Without an `@@`
//! anchor its context must match exactly one place; with one, the anchor
//! must, and the hunk is the first match after it. Missing or ambiguous
//! context is an error, not a guess. Paths are checked as [`WorkspacePath`]s, so nothing outside the
//! checkout can be named.

use std::collections::BTreeSet;

use crate::path::{PathError, WorkspacePath};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

/// Why a patch is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// The text is not one `*** Begin Patch` … `*** End Patch` envelope.
    #[error("a patch starts with `*** Begin Patch` and ends with `*** End Patch`")]
    Envelope,
    /// The envelope holds no file operation.
    #[error("the patch changes no file")]
    Empty,
    /// A line that is not part of the format where it stands.
    #[error("line {line}: {text:?} is not a patch line here")]
    Line {
        /// 1-based line in the patch.
        line: usize,
        /// The line.
        text: String,
    },
    /// A path the checkout does not allow.
    #[error("line {line}: {error}")]
    Path {
        /// 1-based line in the patch.
        line: usize,
        /// Why the path is refused.
        error: PathError,
    },
    /// The same file named by two operations, or as a move's target.
    #[error("{0} appears more than once in the patch")]
    Twice(String),
    /// An update with no hunk.
    #[error("the update of {0} has no change")]
    NoHunks(String),
    /// A hunk whose context is not in the file.
    #[error(
        "hunk {hunk} of {path}: its context is not in the file; read the file and quote it exactly"
    )]
    NotFound {
        /// The file.
        path: String,
        /// 1-based hunk number.
        hunk: usize,
    },
    /// A hunk whose context is in the file more than once.
    #[error(
        "hunk {hunk} of {path}: its context matches {count} places; add lines around it until it matches one"
    )]
    Ambiguous {
        /// The file.
        path: String,
        /// 1-based hunk number.
        hunk: usize,
        /// How many places it matches.
        count: usize,
    },
}

/// One file operation of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// A new file with this content.
    Add {
        /// Where.
        path: WorkspacePath,
        /// The whole content.
        content: String,
    },
    /// Remove a file.
    Delete {
        /// Which.
        path: WorkspacePath,
    },
    /// Change a file in place, and move it when `move_to` is set.
    Update {
        /// Which.
        path: WorkspacePath,
        /// Where it goes, if anywhere.
        move_to: Option<WorkspacePath>,
        /// The changes, in file order.
        hunks: Vec<Hunk>,
    },
}

impl Operation {
    /// The file the operation starts from.
    #[must_use]
    pub fn path(&self) -> &WorkspacePath {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

/// One change inside a file: the lines it replaces (context and removed
/// lines, in order) and the lines that take their place.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hunk {
    /// The text after `@@`: a line that must come before the change,
    /// compared without the whitespace around it.
    pub anchor: Option<String>,
    /// Context and `-` lines, as they are in the file.
    pub old: Vec<String>,
    /// Context and `+` lines, as they will be.
    pub new: Vec<String>,
    /// `*** End of File`: the old lines end the file.
    pub at_end: bool,
}

impl Hunk {
    fn is_empty(&self) -> bool {
        self.anchor.is_none() && self.old.is_empty() && self.new.is_empty()
    }
}

/// A parsed patch: its file operations in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    /// The operations.
    pub operations: Vec<Operation>,
}

impl Patch {
    /// Parses the envelope. Whitespace around it is ignored.
    ///
    /// # Errors
    ///
    /// [`PatchError`] for anything that is not the format, a refused path,
    /// a file named twice, or an update without a change.
    pub fn parse(text: &str) -> Result<Self, PatchError> {
        let lines: Vec<&str> = text.trim().lines().collect();
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
            return Err(PatchError::Envelope);
        };
        if first.trim() != BEGIN || last.trim() != END || lines.len() < 2 {
            return Err(PatchError::Envelope);
        }
        let body = lines.get(1..lines.len() - 1).unwrap_or_default();
        let mut parser = Parser {
            lines: body,
            at: 0,
            operations: Vec::new(),
        };
        while parser.at < body.len() {
            parser.operation()?;
        }
        if parser.operations.is_empty() {
            return Err(PatchError::Empty);
        }
        let mut named = BTreeSet::new();
        for operation in &parser.operations {
            let mut paths = vec![operation.path()];
            if let Operation::Update {
                move_to: Some(to), ..
            } = operation
            {
                paths.push(to);
            }
            for path in paths {
                if !named.insert(path.as_str()) {
                    return Err(PatchError::Twice(path.to_string()));
                }
            }
        }
        Ok(Self {
            operations: parser.operations,
        })
    }
}

struct Parser<'a> {
    lines: &'a [&'a str],
    at: usize,
    operations: Vec<Operation>,
}

impl Parser<'_> {
    /// The 1-based line in the whole patch, after `*** Begin Patch`.
    fn line_number(&self) -> usize {
        self.at + 2
    }

    fn current(&self) -> Option<&str> {
        self.lines.get(self.at).copied()
    }

    fn path(&self, value: &str) -> Result<WorkspacePath, PatchError> {
        WorkspacePath::parse(value).map_err(|error| PatchError::Path {
            line: self.line_number(),
            error,
        })
    }

    fn unexpected(&self) -> PatchError {
        PatchError::Line {
            line: self.line_number(),
            text: self.current().unwrap_or_default().to_owned(),
        }
    }

    fn operation(&mut self) -> Result<(), PatchError> {
        let line = self.current().unwrap_or_default();
        if let Some(raw) = line.strip_prefix(ADD) {
            let path = self.path(raw)?;
            self.at += 1;
            let mut content = String::new();
            while let Some(added) = self.current().and_then(|l| l.strip_prefix('+')) {
                content.push_str(added);
                content.push('\n');
                self.at += 1;
            }
            self.operations.push(Operation::Add { path, content });
        } else if let Some(raw) = line.strip_prefix(DELETE) {
            let path = self.path(raw)?;
            self.at += 1;
            self.operations.push(Operation::Delete { path });
        } else if let Some(raw) = line.strip_prefix(UPDATE) {
            let path = self.path(raw)?;
            self.at += 1;
            let move_to = match self.current().and_then(|l| l.strip_prefix(MOVE)) {
                Some(raw) => {
                    let to = self.path(raw)?;
                    self.at += 1;
                    Some(to)
                }
                None => None,
            };
            let hunks = self.hunks()?;
            if hunks.is_empty() {
                return Err(PatchError::NoHunks(path.to_string()));
            }
            self.operations.push(Operation::Update {
                path,
                move_to,
                hunks,
            });
        } else {
            return Err(self.unexpected());
        }
        Ok(())
    }

    /// The hunks of an update, up to the next operation. The first hunk
    /// may leave out its `@@` line.
    fn hunks(&mut self) -> Result<Vec<Hunk>, PatchError> {
        let mut hunks = Vec::new();
        let mut hunk = Hunk::default();
        while let Some(line) = self.current() {
            if line.trim_end() == END_OF_FILE {
                hunk.at_end = true;
                self.at += 1;
                hunks.push(std::mem::take(&mut hunk));
                continue;
            }
            if line.starts_with("*** ") {
                break;
            }
            if let Some(rest) = line.strip_prefix("@@") {
                if !hunk.is_empty() {
                    hunks.push(std::mem::take(&mut hunk));
                }
                let anchor = rest.trim();
                hunk.anchor = (!anchor.is_empty()).then(|| anchor.to_owned());
            } else if let Some(text) = line.strip_prefix(' ') {
                hunk.old.push(text.to_owned());
                hunk.new.push(text.to_owned());
            } else if let Some(text) = line.strip_prefix('-') {
                hunk.old.push(text.to_owned());
            } else if let Some(text) = line.strip_prefix('+') {
                hunk.new.push(text.to_owned());
            } else if line.is_empty() {
                // An empty context line whose leading space was lost.
                hunk.old.push(String::new());
                hunk.new.push(String::new());
            } else {
                return Err(self.unexpected());
            }
            self.at += 1;
        }
        if !hunk.is_empty() {
            hunks.push(hunk);
        }
        Ok(hunks)
    }
}

/// Applies an update's hunks to the text of `path`, each after the
/// previous one. A hunk without an anchor must match exactly one place; a
/// hunk with one takes the first match after its anchor, which itself must
/// match exactly one place. The text keeps whether it
/// ended with a newline.
///
/// # Errors
///
/// [`PatchError::NotFound`] or [`PatchError::Ambiguous`] for a hunk that
/// does not match exactly one place.
pub fn apply(path: &WorkspacePath, text: &str, hunks: &[Hunk]) -> Result<String, PatchError> {
    let ends_with_newline = text.is_empty() || text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut replacements = Vec::new();
    let mut cursor = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let number = index + 1;
        let not_found = || PatchError::NotFound {
            path: path.to_string(),
            hunk: number,
        };
        let ambiguous = |count| PatchError::Ambiguous {
            path: path.to_string(),
            hunk: number,
            count,
        };
        if let Some(anchor) = &hunk.anchor {
            let found: Vec<usize> = (cursor..lines.len())
                .filter(|&at| lines.get(at).is_some_and(|line| line.trim() == anchor))
                .collect();
            match found.as_slice() {
                [] => return Err(not_found()),
                [at] => cursor = at + 1,
                many => return Err(ambiguous(many.len())),
            }
        }
        let start = if hunk.old.is_empty() {
            // A pure addition: after the anchor, or at the end.
            if hunk.anchor.is_some() && !hunk.at_end {
                cursor
            } else {
                lines.len()
            }
        } else {
            match matches(&lines, cursor, &hunk.old, hunk.at_end).as_slice() {
                [] => return Err(not_found()),
                [at, ..] if hunk.anchor.is_some() => *at,
                [at] => *at,
                many => return Err(ambiguous(many.len())),
            }
        };
        replacements.push((start, hunk.old.len(), hunk.new.clone()));
        cursor = start + hunk.old.len();
    }
    for (start, length, new) in replacements.into_iter().rev() {
        lines.splice(start..start + length, new);
    }
    let mut out = lines.join("\n");
    if ends_with_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Where `needle` starts in `lines` at or after `from`; with `at_end`,
/// only where it ends the file.
fn matches(lines: &[String], from: usize, needle: &[String], at_end: bool) -> Vec<usize> {
    if needle.len() > lines.len() {
        return Vec::new();
    }
    (from..=lines.len() - needle.len())
        .filter(|&start| !at_end || start + needle.len() == lines.len())
        .filter(|&start| lines.get(start..start + needle.len()) == Some(needle))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::parse(value).unwrap()
    }

    fn update(patch: &str) -> Vec<Hunk> {
        match Patch::parse(patch).unwrap().operations.remove(0) {
            Operation::Update { hunks, .. } => hunks,
            other => panic!("{other:?}"),
        }
    }

    const SOURCE: &str =
        "fn main() {\n    let x = 1;\n    let y = 2;\n}\n\nfn other() {\n    let y = 2;\n}\n";

    #[test]
    fn every_operation_parses() {
        let patch = Patch::parse(
            "*** Begin Patch\n\
             *** Add File: docs/new.md\n\
             +# New\n\
             +\n\
             +text\n\
             *** Update File: src/a.rs\n\
             *** Move to: src/b.rs\n\
             @@ fn main() {\n\
             \x20    let x = 1;\n\
             -    let y = 2;\n\
             +    let y = 3;\n\
             *** Delete File: old.txt\n\
             *** End Patch\n",
        )
        .unwrap();
        assert_eq!(
            patch.operations,
            vec![
                Operation::Add {
                    path: path("docs/new.md"),
                    content: "# New\n\ntext\n".to_owned()
                },
                Operation::Update {
                    path: path("src/a.rs"),
                    move_to: Some(path("src/b.rs")),
                    hunks: vec![Hunk {
                        anchor: Some("fn main() {".to_owned()),
                        old: vec!["    let x = 1;".to_owned(), "    let y = 2;".to_owned()],
                        new: vec!["    let x = 1;".to_owned(), "    let y = 3;".to_owned()],
                        at_end: false,
                    }],
                },
                Operation::Delete {
                    path: path("old.txt")
                },
            ]
        );
    }

    #[test]
    fn an_anchor_picks_one_of_two_equal_places() {
        let hunks = update(
            "*** Begin Patch\n*** Update File: a.rs\n@@ fn other() {\n-    let y = 2;\n+    let y = 4;\n*** End Patch",
        );
        assert_eq!(
            apply(&path("a.rs"), SOURCE, &hunks).unwrap(),
            "fn main() {\n    let x = 1;\n    let y = 2;\n}\n\nfn other() {\n    let y = 4;\n}\n"
        );
    }

    #[test]
    fn an_anchor_is_found_whatever_its_indent() {
        let hunks = update(
            "*** Begin Patch\n*** Update File: a.rs\n@@ let x = 1;\n-    let y = 2;\n+    let y = 5;\n*** End Patch",
        );
        assert!(
            apply(&path("a.rs"), SOURCE, &hunks)
                .unwrap()
                .starts_with("fn main() {\n    let x = 1;\n    let y = 5;\n}")
        );
    }

    #[test]
    fn hunks_apply_in_order_and_keep_the_rest() {
        let hunks = update(
            "*** Begin Patch\n*** Update File: a.rs\n@@\n fn main() {\n+    // start\n     let x = 1;\n@@\n-fn other() {\n+fn renamed() {\n*** End Patch",
        );
        assert_eq!(
            apply(&path("a.rs"), SOURCE, &hunks).unwrap(),
            "fn main() {\n    // start\n    let x = 1;\n    let y = 2;\n}\n\nfn renamed() {\n    let y = 2;\n}\n"
        );
    }

    #[test]
    fn the_end_of_file_marker_pins_a_hunk_to_the_end() {
        let hunks = update(
            "*** Begin Patch\n*** Update File: a.rs\n@@\n     let y = 2;\n-}\n+}\n+// end\n*** End of File\n*** End Patch",
        );
        assert!(
            apply(&path("a.rs"), SOURCE, &hunks)
                .unwrap()
                .ends_with("    let y = 2;\n}\n// end\n")
        );
    }

    #[test]
    fn a_file_without_a_final_newline_keeps_it_that_way() {
        let hunks = update("*** Begin Patch\n*** Update File: a\n-b\n+c\n*** End Patch");
        assert_eq!(apply(&path("a"), "a\nb", &hunks).unwrap(), "a\nc");
    }

    #[test]
    fn missing_or_ambiguous_context_is_refused() {
        let ambiguous = update(
            "*** Begin Patch\n*** Update File: a.rs\n-    let y = 2;\n+    let y = 3;\n*** End Patch",
        );
        assert_eq!(
            apply(&path("a.rs"), SOURCE, &ambiguous).unwrap_err(),
            PatchError::Ambiguous {
                path: "a.rs".to_owned(),
                hunk: 1,
                count: 2
            }
        );
        let missing = update(
            "*** Begin Patch\n*** Update File: a.rs\n-    let z = 9;\n+    let z = 3;\n*** End Patch",
        );
        assert!(matches!(
            apply(&path("a.rs"), SOURCE, &missing),
            Err(PatchError::NotFound { hunk: 1, .. })
        ));
        // Whitespace is part of the context: no fuzzy match.
        let indented = update(
            "*** Begin Patch\n*** Update File: a.rs\n-  let x = 1;\n+  let x = 3;\n*** End Patch",
        );
        assert!(matches!(
            apply(&path("a.rs"), SOURCE, &indented),
            Err(PatchError::NotFound { .. })
        ));
    }

    #[test]
    fn a_later_hunk_does_not_match_before_an_earlier_one() {
        let hunks = update(
            "*** Begin Patch\n*** Update File: a.rs\n@@ fn other() {\n     let y = 2;\n@@\n-    let x = 1;\n+    let x = 5;\n*** End Patch",
        );
        assert!(matches!(
            apply(&path("a.rs"), SOURCE, &hunks),
            Err(PatchError::NotFound { hunk: 2, .. })
        ));
    }

    #[test]
    fn paths_outside_the_checkout_are_refused() {
        for patch in [
            "*** Begin Patch\n*** Add File: ../x\n+y\n*** End Patch",
            "*** Begin Patch\n*** Delete File: /etc/passwd\n*** End Patch",
            "*** Begin Patch\n*** Update File: a\n*** Move to: .git/hooks/pre-commit\n-a\n+b\n*** End Patch",
        ] {
            assert!(
                matches!(Patch::parse(patch), Err(PatchError::Path { .. })),
                "{patch}"
            );
        }
    }

    #[test]
    fn malformed_patches_are_refused() {
        assert_eq!(Patch::parse("just text"), Err(PatchError::Envelope));
        assert_eq!(
            Patch::parse("*** Begin Patch\n*** Add File: a\n+a"),
            Err(PatchError::Envelope)
        );
        assert_eq!(
            Patch::parse("*** Begin Patch\n*** End Patch"),
            Err(PatchError::Empty)
        );
        assert_eq!(
            Patch::parse("*** Begin Patch\n*** Update File: a\n*** End Patch"),
            Err(PatchError::NoHunks("a".to_owned()))
        );
        assert_eq!(
            Patch::parse("*** Begin Patch\n*** Delete File: a\n*** Add File: a\n+x\n*** End Patch"),
            Err(PatchError::Twice("a".to_owned()))
        );
        assert_eq!(
            Patch::parse("*** Begin Patch\n*** Update File: a\n?a\n*** End Patch"),
            Err(PatchError::Line {
                line: 3,
                text: "?a".to_owned()
            })
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        let patch =
            Patch::parse("\n  *** Begin Patch\n*** Delete File: a\n*** End Patch  \n\n").unwrap();
        assert_eq!(patch.operations.len(), 1);
    }
}
