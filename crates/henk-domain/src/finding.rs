//! Findings (§1.1, §3.2, §3.3): what lanes report, and how they avoid
//! repeating each other.

use std::collections::BTreeMap;

use crate::review::LaneName;

/// Where a finding sits: one line of one file at the reviewed commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingKey {
    /// Path in the repository.
    pub path: String,
    /// Line in the new version of the file.
    pub line: u32,
}

/// A finding that exists on the pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Where.
    pub key: FindingKey,
    /// The platform's id of the comment.
    pub comment_id: String,
    /// The visible text.
    pub body: String,
    /// The lane that wrote it, when known from its marker.
    pub lane: Option<LaneName>,
    /// Whether a person has answered in its thread. Such a comment is
    /// never rewritten (§3.2).
    pub answered_by_person: bool,
    /// Whether its thread is resolved. Resolved findings do not count
    /// (§3.3; open question 5 decided as "no").
    pub resolved: bool,
    /// Whether its line is still in the diff at the reviewed commit.
    pub in_diff: bool,
}

impl Finding {
    /// Whether this finding counts towards the summary's N (§3.3).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.in_diff && !self.resolved
    }
}

/// The result of claiming a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim<'a> {
    /// Nobody has this line yet; the caller may post.
    New,
    /// Somebody does. Improve it or stay quiet (§3.2).
    Exists(&'a Finding),
}

/// The findings on one pull/merge request, shared by all lanes of a review.
///
/// Two lanes can find the same problem in the same second (open question 3).
/// Whoever claims the key first posts; the other is told what exists.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FindingRegistry {
    by_key: BTreeMap<FindingKey, Finding>,
}

impl FindingRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the registry with findings already on the pull/merge request.
    #[must_use]
    pub fn seeded(findings: impl IntoIterator<Item = Finding>) -> Self {
        let mut registry = Self::new();
        for finding in findings {
            registry.by_key.insert(finding.key.clone(), finding);
        }
        registry
    }

    /// Looks up a key without claiming it.
    #[must_use]
    pub fn get(&self, key: &FindingKey) -> Option<&Finding> {
        self.by_key.get(key)
    }

    /// Reports whether `key` is free. Does not reserve it; call
    /// [`FindingRegistry::record`] once the comment is posted, under the
    /// same lock.
    #[must_use]
    pub fn claim(&self, key: &FindingKey) -> Claim<'_> {
        self.by_key.get(key).map_or(Claim::New, Claim::Exists)
    }

    /// Records a posted or updated finding.
    pub fn record(&mut self, finding: Finding) {
        self.by_key.insert(finding.key.clone(), finding);
    }

    /// Replaces the body of an existing finding.
    pub fn improve(&mut self, key: &FindingKey, body: String, lane: Option<LaneName>) -> bool {
        match self.by_key.get_mut(key) {
            Some(finding) => {
                finding.body = body;
                finding.lane = lane;
                true
            }
            None => false,
        }
    }

    /// Every finding, in path and line order.
    pub fn iter(&self) -> impl Iterator<Item = &Finding> + '_ {
        self.by_key.values()
    }

    /// The number of findings that count towards the summary (§3.3).
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.by_key.values().filter(|f| f.is_open()).count()
    }

    /// Number of findings known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether nothing is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn finding(path: &str, line: u32) -> Finding {
        Finding {
            key: FindingKey {
                path: path.into(),
                line,
            },
            comment_id: format!("{path}:{line}"),
            body: "Off by one.".into(),
            lane: Some(LaneName::new("a")),
            answered_by_person: false,
            resolved: false,
            in_diff: true,
        }
    }

    #[test]
    fn first_claim_wins_and_the_second_sees_the_existing_finding() {
        let mut registry = FindingRegistry::new();
        let key = FindingKey {
            path: "src/a.rs".into(),
            line: 10,
        };
        assert_eq!(registry.claim(&key), Claim::New);
        registry.record(finding("src/a.rs", 10));
        assert!(matches!(registry.claim(&key), Claim::Exists(f) if f.comment_id == "src/a.rs:10"));
    }

    #[test]
    fn open_count_excludes_resolved_and_outdated() {
        let mut resolved = finding("a", 1);
        resolved.resolved = true;
        let mut outdated = finding("b", 2);
        outdated.in_diff = false;
        let registry = FindingRegistry::seeded([finding("c", 3), resolved, outdated]);
        assert_eq!(registry.len(), 3);
        assert_eq!(registry.open_count(), 1);
    }

    #[test]
    fn improve_replaces_the_body() {
        let mut registry = FindingRegistry::seeded([finding("a", 1)]);
        let key = FindingKey {
            path: "a".into(),
            line: 1,
        };
        assert!(registry.improve(&key, "Clearer.".into(), Some(LaneName::new("b"))));
        assert_eq!(registry.get(&key).unwrap().body, "Clearer.");
        assert!(!registry.improve(
            &FindingKey {
                path: "zz".into(),
                line: 9
            },
            String::new(),
            None
        ));
    }
}
