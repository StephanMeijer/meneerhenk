//! Taking an issue to a pull request (§4a, #329): the branch the executor's
//! work goes to, the commit's subject, and the pull request that links the
//! issue and says what was done and what was not.

/// The most characters of the title's slug in a branch name.
const MAX_SLUG: usize = 40;

/// The most characters of a commit subject.
const MAX_SUBJECT: usize = 72;

/// The branch for issue `number`: `prefix`, the number and a slug of the
/// title, such as `henk/329-the-executor-takes-an-issue`. The slug keeps
/// ASCII letters and digits, lowercased, with one `-` between words, and is
/// cut at a word to at most 40 characters.
#[must_use]
pub fn branch_name(prefix: &str, number: u64, title: &str) -> String {
    let mut slug = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let extra = usize::from(!slug.is_empty()) + word.len();
        if slug.len() + extra > MAX_SLUG {
            if slug.is_empty() {
                slug = word.chars().take(MAX_SLUG).collect();
            }
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    if slug.is_empty() {
        format!("{prefix}{number}")
    } else {
        format!("{prefix}{number}-{slug}")
    }
}

/// Whether `prefix` can start every branch name: one or more path
/// components ending in `/`, each of ASCII letters, digits, `-`, `_` and
/// `.`, and none that git refuses in a ref (`git check-ref-format`): empty,
/// `..`, starting with `.` or ending in `.lock`. The whole may not start
/// with `-`.
#[must_use]
pub fn is_branch_prefix(prefix: &str) -> bool {
    let Some(components) = prefix.strip_suffix('/') else {
        return false;
    };
    !components.starts_with('-')
        && components.split('/').all(|component| {
            !component.is_empty()
                && !component.starts_with('.')
                && component.strip_suffix(".lock").is_none()
                && !component.contains("..")
                && component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// The `n`th branch name to try for an issue whose first choice is `name`
/// (from 1): `name` itself, then `name-2`, `name-3` and on, so a run for an
/// issue that already has a branch (an earlier run's, or somebody's) takes
/// a new one instead of pushing over it.
#[must_use]
pub fn branch_candidate(name: &str, n: u32) -> String {
    if n <= 1 {
        name.to_owned()
    } else {
        format!("{name}-{n}")
    }
}

/// The subject of the executor's commit: `Resolve #9: Export runs`, on one
/// line, cut to 72 characters.
#[must_use]
pub fn commit_subject(reference: &str, title: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let subject = format!("Resolve {reference}: {title}");
    if subject.chars().count() <= MAX_SUBJECT {
        return subject;
    }
    let cut: String = subject.chars().take(MAX_SUBJECT - 3).collect();
    format!("{}...", cut.trim_end())
}

/// The pull request's body: it closes the issue, says what changed and
/// what was left out, and links the run.
#[must_use]
pub fn pull_body(reference: &str, summary: &str, left_out: &str, run_link: &str) -> String {
    let left_out = if left_out.trim().is_empty() {
        "Nothing."
    } else {
        left_out.trim()
    };
    format!(
        "Closes {reference}\n\n## What changed\n\n{}\n\n## Left out\n\n{left_out}\n\nMeneer Henk's executor made this change; his review follows. Run: {run_link}",
        summary.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_is_the_prefix_the_number_and_a_short_slug() {
        assert_eq!(
            branch_name(
                "henk/",
                329,
                "Execution stage: the executor takes an issue to a pull request"
            ),
            "henk/329-execution-stage-the-executor-takes-an"
        );
        assert_eq!(
            branch_name("henk/", 9, "Export runs!"),
            "henk/9-export-runs"
        );
        assert_eq!(branch_name("henk/", 9, "Échec: ☃"), "henk/9-chec");
        assert_eq!(branch_name("bot/", 4, "🚀"), "bot/4");
        let long = branch_name("henk/", 1, &"x".repeat(60));
        assert_eq!(long, format!("henk/1-{}", "x".repeat(40)));
    }

    #[test]
    fn a_branch_prefix_is_one_git_takes() {
        for good in ["henk/", "bots/henk/", "a.b/", "h_1-x/", "x.lockx/"] {
            assert!(is_branch_prefix(good), "{good}");
        }
        for bad in [
            "henk",
            "/henk/",
            "-x/",
            "a//b/",
            "a..b/",
            "h nk/",
            "/",
            "",
            "./",
            ".h/",
            "a/.h/",
            "a.lock/",
            "b/a.lock/",
        ] {
            assert!(!is_branch_prefix(bad), "{bad}");
        }
    }

    #[test]
    fn a_taken_branch_name_gets_a_number() {
        assert_eq!(
            branch_candidate("henk/9-export-runs", 1),
            "henk/9-export-runs"
        );
        assert_eq!(
            branch_candidate("henk/9-export-runs", 2),
            "henk/9-export-runs-2"
        );
        assert_eq!(branch_candidate("henk/9", 0), "henk/9");
    }

    #[test]
    fn the_subject_names_the_issue_on_one_line() {
        assert_eq!(
            commit_subject("#9", "Export\nruns"),
            "Resolve #9: Export runs"
        );
        let long = commit_subject("#9", &"word ".repeat(30));
        assert_eq!(long.chars().count(), 72);
        assert!(long.ends_with("..."), "{long}");
    }

    #[test]
    fn the_body_closes_the_issue_and_says_what_was_left_out() {
        let body = pull_body("#9", "Runs export as CSV.", "", "https://henk/runs/r-1");
        assert!(body.starts_with("Closes #9\n"), "{body}");
        assert!(body.contains("## Left out\n\nNothing."), "{body}");
        assert!(body.ends_with("Run: https://henk/runs/r-1"), "{body}");
        assert!(crate::text::is_in_style(&body));
        let body = pull_body("#9", "CSV.", "JSON export, out of scope.", "l");
        assert!(body.contains("JSON export, out of scope."), "{body}");
    }
}
