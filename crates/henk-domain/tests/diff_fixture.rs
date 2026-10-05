//! The parser on a real pull request diff (meneerhenk PR 3, 30 files).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use henk_domain::diff::{DiffSide, FileStatus, ReviewDiff};

const PR3: &str = include_str!("fixtures/meneerhenk-pr3.diff");

#[test]
fn parses_every_file_of_a_real_diff() {
    let diff = ReviewDiff::from_unified(PR3);
    assert_eq!(diff.files().len(), 30);
    let added = diff
        .files()
        .iter()
        .filter(|f| f.status == FileStatus::Added)
        .count();
    assert_eq!(added, 3);
    // Line counts agree with a plain count of +/- lines in the hunks.
    let (mut plus, mut minus) = (0, 0);
    let mut in_hunk = false;
    for line in PR3.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if in_hunk {
            if line.starts_with('+') {
                plus += 1;
            } else if line.starts_with('-') {
                minus += 1;
            }
        }
    }
    let additions: u32 = diff.files().iter().map(|f| f.additions).sum();
    let deletions: u32 = diff.files().iter().map(|f| f.deletions).sum();
    assert_eq!((additions, deletions), (plus, minus));
    // Every added line is commentable on the right side; every removed one
    // on the left; and the list fits in a few hundred characters per file.
    for file in diff.files() {
        for line in file.hunks.iter().flat_map(|h| &h.lines) {
            if let Some(n) = line.new_no {
                assert_eq!(
                    diff.commentable(&file.path, n, DiffSide::Right),
                    Ok(()),
                    "{}:{n}",
                    file.path
                );
            }
            if let Some(n) = line.old_no {
                assert_eq!(
                    diff.commentable(&file.path, n, DiffSide::Left),
                    Ok(()),
                    "{}:{n}",
                    file.path
                );
            }
        }
    }
    assert!(diff.render_list().len() < 30 * 120);
    let rendered: usize = diff.files().iter().map(|f| f.render().len()).sum();
    assert!(rendered > PR3.len(), "numbers add to the text");
}

#[test]
fn ignored_files_are_listed_but_not_reviewed() {
    let ignore = henk_domain::ignore::PathFilter::new(["**/Cargo.lock", "CHANGELOG.md"]);
    let patches = henk_domain::diff::split_unified(PR3);
    let diff = ReviewDiff::from_patches_filtered(&patches, &ignore);
    assert_eq!(diff.files().len(), 29, "Cargo.lock is left out");
    assert_eq!(diff.ignored(), ["Cargo.lock"]);
    assert!(diff.file("Cargo.lock").is_none());
    assert!(diff.paths().all(|p| p != "Cargo.lock"));
    assert!(
        diff.commentable("Cargo.lock", 1, DiffSide::Right).is_err(),
        "no finding on an ignored file"
    );
    assert!(
        diff.render_list()
            .ends_with("Not reviewed (review.ignore): Cargo.lock\n")
    );
    assert!(!diff.is_empty());
}

#[test]
fn a_change_of_only_ignored_files_has_nothing_to_review() {
    let lock_only = "\
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1 +1 @@
-old
+new
";
    let ignore = henk_domain::ignore::PathFilter::new(["**/Cargo.lock"]);
    let diff =
        ReviewDiff::from_patches_filtered(&henk_domain::diff::split_unified(lock_only), &ignore);
    assert!(diff.is_empty());
    assert!(diff.has_changes());
    assert!(diff.render_list().starts_with("No file to review"));
    assert!(!ReviewDiff::default().has_changes());
}
