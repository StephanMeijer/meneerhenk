//! Which changed files a review leaves out (§3.2): lockfiles, generated
//! changelogs and whatever else `review.ignore` names.
//!
//! Patterns follow gitignore for the cases that matter here:
//!
//! - a pattern without `/` matches the file name at any depth
//!   (`CHANGELOG.md`, `*.lock`);
//! - `**` as a whole segment matches any number of directories, none
//!   included (`**/Cargo.lock`, `docs/**/generated.md`);
//! - `*` matches within one segment, `?` one character of a segment;
//! - anything else matches the whole path, segment by segment.

/// A set of path patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathFilter {
    patterns: Vec<String>,
}

impl PathFilter {
    /// A filter over these patterns. Empty patterns match nothing.
    #[must_use]
    pub fn new(patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            patterns: patterns
                .into_iter()
                .map(Into::into)
                .filter(|p: &String| !p.trim().is_empty())
                .collect(),
        }
    }

    /// The patterns, as given.
    pub fn patterns(&self) -> impl Iterator<Item = &str> + '_ {
        self.patterns.iter().map(String::as_str)
    }

    /// Whether any pattern matches `path`.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let path = path.trim().trim_start_matches("./");
        self.patterns.iter().any(|pattern| matches(pattern, path))
    }
}

fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if pattern.contains('/') {
        let pattern: Vec<&str> = pattern.split('/').collect();
        let path: Vec<&str> = path.split('/').collect();
        segments_match(&pattern, &path)
    } else {
        let name = path.rsplit('/').next().unwrap_or(path);
        segment_matches(pattern.as_bytes(), name.as_bytes())
    }
}

/// Whole-path matching, segment by segment; `**` takes zero or more.
fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            (0..=path.len()).any(|skip| path.get(skip..).is_some_and(|p| segments_match(rest, p)))
        }
        Some((first, rest)) => path.split_first().is_some_and(|(segment, others)| {
            segment_matches(first.as_bytes(), segment.as_bytes()) && segments_match(rest, others)
        }),
    }
}

/// One segment: `*` any run of bytes, `?` one byte, anything else itself.
fn segment_matches(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => {
            (0..=name.len()).any(|skip| name.get(skip..).is_some_and(|n| segment_matches(rest, n)))
        }
        Some((b'?', rest)) => name
            .split_first()
            .is_some_and(|(_, others)| segment_matches(rest, others)),
        Some((c, rest)) => name
            .split_first()
            .is_some_and(|(n, others)| n == c && segment_matches(rest, others)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(patterns: &[&str]) -> PathFilter {
        PathFilter::new(patterns.iter().copied())
    }

    #[test]
    fn a_name_pattern_matches_at_any_depth() {
        let f = filter(&["CHANGELOG.md", "*.lock"]);
        assert!(f.matches("CHANGELOG.md"));
        assert!(f.matches("crates/henk/CHANGELOG.md"));
        assert!(f.matches("Cargo.lock"));
        assert!(f.matches("./web/yarn.lock"));
        assert!(!f.matches("docs/CHANGELOG.md.bak"));
        assert!(!f.matches("Cargo.lock.orig"));
        assert!(!f.matches("src/lock.rs"));
    }

    #[test]
    fn double_star_takes_any_number_of_directories() {
        let f = filter(&["**/Cargo.lock", "docs/**/generated.md"]);
        assert!(f.matches("Cargo.lock"), "zero directories");
        assert!(f.matches("a/b/Cargo.lock"));
        assert!(f.matches("docs/generated.md"));
        assert!(f.matches("docs/api/v1/generated.md"));
        assert!(!f.matches("src/docs/generated.md"));
        assert!(!f.matches("Cargo.lock/x"));
    }

    #[test]
    fn a_path_pattern_matches_the_whole_path() {
        let f = filter(&["vendor/*/LICENSE", "gen/v?.rs"]);
        assert!(f.matches("vendor/foo/LICENSE"));
        assert!(
            !f.matches("vendor/foo/bar/LICENSE"),
            "* stays in one segment"
        );
        assert!(!f.matches("x/vendor/foo/LICENSE"));
        assert!(f.matches("gen/v1.rs"));
        assert!(!f.matches("gen/v10.rs"));
    }

    #[test]
    fn empty_patterns_and_filters_match_nothing() {
        assert!(!PathFilter::default().matches("Cargo.lock"));
        assert!(!filter(&["", "  "]).matches("Cargo.lock"));
        assert_eq!(filter(&["", "*.lock"]).patterns().count(), 1);
    }
}
