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
    wildcard(
        pattern,
        path,
        |p| *p == "**",
        |p, segment| segment_matches(p.as_bytes(), segment.as_bytes()),
    )
}

/// One segment: `*` any run of bytes, `?` one byte, anything else itself.
fn segment_matches(pattern: &[u8], name: &[u8]) -> bool {
    wildcard(pattern, name, |p| *p == b'*', |p, n| *p == b'?' || p == n)
}

/// Whether `pattern` matches all of `text`, where a `star` item takes any
/// run of items and every other item takes one that `one` accepts. Greedy
/// with one point to go back to, the last star: a later star can always
/// take what an earlier one would have, so going back further never finds
/// a match this misses. At most `pattern.len() * text.len()` steps, however
/// many stars, where trying every split was exponential in them.
fn wildcard<P, T>(
    pattern: &[P],
    text: &[T],
    star: impl Fn(&P) -> bool,
    one: impl Fn(&P, &T) -> bool,
) -> bool {
    let (mut p, mut t) = (0, 0);
    // The last star's position and the text position it was taken at.
    let mut back: Option<(usize, usize)> = None;
    while let Some(item) = text.get(t) {
        match pattern.get(p) {
            Some(next) if star(next) => {
                back = Some((p, t));
                p += 1;
                continue;
            }
            Some(next) if one(next, item) => {
                p += 1;
                t += 1;
                continue;
            }
            _ => {}
        }
        let Some((at, taken)) = back else {
            return false;
        };
        // The star takes one more item and the rest is tried again.
        back = Some((at, taken + 1));
        p = at + 1;
        t = taken + 1;
    }
    pattern.get(p..).is_some_and(|rest| rest.iter().all(&star))
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
    fn stars_and_question_marks_within_a_segment() {
        let f = filter(&["*a*b?", "x*", "*", "a*?*c"]);
        assert!(f.matches("src/aXbY"));
        assert!(f.matches("ab1"));
        assert!(f.matches("xenon"));
        assert!(f.matches("abc"));
        let g = filter(&["*a*b?"]);
        assert!(!g.matches("ab"));
        assert!(!g.matches("aXb"));
        assert!(!filter(&["a*?*c"]).matches("ac"));
        assert!(filter(&["src/**/**/x.rs"]).matches("src/x.rs"));
        assert!(!filter(&["src/**/y/**/x.rs"]).matches("src/a/x.rs"));
    }

    #[test]
    fn a_glob_full_of_stars_is_cheap() {
        let name = "a".repeat(64);
        let start = std::time::Instant::now();
        assert!(!filter(&["*a*a*a*a*a*a*a*b"]).matches(&name));
        let deep = vec!["a"; 64].join("/");
        assert!(!filter(&["**/a/**/a/**/a/**/a/**/a/**/a/**/a/**/b"]).matches(&deep));
        assert!(filter(&["**/a/**/a/**/a/**/a/**/a/**/a/**/a/**/a"]).matches(&deep));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn empty_patterns_and_filters_match_nothing() {
        assert!(!PathFilter::default().matches("Cargo.lock"));
        assert!(!filter(&["", "  "]).matches("Cargo.lock"));
        assert_eq!(filter(&["", "*.lock"]).patterns().count(), 1);
    }
}
