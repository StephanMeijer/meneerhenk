//! Drafts (§3.2, #189): what review lanes want written, held until a
//! fact-check has judged all of them together after the lanes, and what
//! then happens to each.
//!
//! A lane's `post_finding`, `improve_finding` and `withdraw_finding` add a
//! draft here instead of writing. When every lane has ended, the drafts are
//! checked in chunks, one session per checking model, and [`settle`] turns
//! the verdicts into what Henk's code writes: nothing reaches the platform
//! before its verdict.

use std::collections::BTreeMap;
use std::fmt;

use crate::diff::DiffSide;
use crate::finding::FindingKey;
use crate::marker::ModelId;
use crate::review::LaneName;

/// The most drafts one checking session judges, so later verdicts are not
/// given deep in a long context.
pub const CHUNK: usize = 10;

/// A draft's number within its review: `d1`, `d2`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DraftId(u32);

impl DraftId {
    /// Reads `d3` (or `3`) back.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.trim().strip_prefix('d').unwrap_or(text.trim());
        digits.parse().ok().filter(|n| *n > 0).map(Self)
    }
}

impl fmt::Display for DraftId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "d{}", self.0)
    }
}

/// What a draft would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftKind {
    /// A new finding on a line of the diff.
    Finding {
        /// The side of the diff the line is on.
        side: DiffSide,
    },
    /// A better text for an existing finding.
    Rewrite {
        /// The finding's comment.
        comment_id: String,
        /// Its visible text now.
        current: String,
    },
    /// Withdrawing an existing finding as wrong; the draft's text is why.
    Withdrawal {
        /// The finding's comment.
        comment_id: String,
        /// Its visible text now.
        finding: String,
    },
}

impl DraftKind {
    /// `finding`, `rewrite` or `withdrawal`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Finding { .. } => "finding",
            Self::Rewrite { .. } => "rewrite",
            Self::Withdrawal { .. } => "withdrawal",
        }
    }

    /// The existing comment a rewrite or withdrawal is about.
    #[must_use]
    pub fn comment_id(&self) -> Option<&str> {
        match self {
            Self::Finding { .. } => None,
            Self::Rewrite { comment_id, .. } | Self::Withdrawal { comment_id, .. } => {
                Some(comment_id)
            }
        }
    }
}

/// One thing a lane wants written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// Its number.
    pub id: DraftId,
    /// The lane that wrote it.
    pub lane: LaneName,
    /// The lane's model, which never checks its own draft first.
    pub model: ModelId,
    /// What it would write.
    pub kind: DraftKind,
    /// The line it is about.
    pub key: FindingKey,
    /// The finding, the new text, or the reason for a withdrawal.
    pub text: String,
}

/// Why a draft could not be added: another lane's draft holds the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken {
    /// That draft.
    pub id: DraftId,
    /// Its lane.
    pub lane: LaneName,
}

/// The drafts of one review, shared by its lanes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DraftBook {
    drafts: BTreeMap<DraftId, Draft>,
    by_key: BTreeMap<FindingKey, DraftId>,
}

impl DraftBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a draft and returns its number. A line holds one draft: another
    /// lane's draft there makes this one [`Taken`], and the lane's own is
    /// replaced, keeping its number.
    ///
    /// # Errors
    ///
    /// Returns [`Taken`] when another lane's draft holds `key`.
    pub fn add(
        &mut self,
        lane: &LaneName,
        model: &ModelId,
        kind: DraftKind,
        key: FindingKey,
        text: &str,
    ) -> Result<DraftId, Taken> {
        if let Some(id) = self.by_key.get(&key).copied()
            && let Some(draft) = self.drafts.get_mut(&id)
        {
            if &draft.lane != lane {
                return Err(Taken {
                    id,
                    lane: draft.lane.clone(),
                });
            }
            draft.kind = kind;
            text.clone_into(&mut draft.text);
            return Ok(id);
        }
        let next = u32::try_from(self.drafts.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let id = DraftId(next);
        self.by_key.insert(key.clone(), id);
        self.drafts.insert(
            id,
            Draft {
                id,
                lane: lane.clone(),
                model: model.clone(),
                kind,
                key,
                text: text.to_owned(),
            },
        );
        Ok(id)
    }

    /// One draft.
    #[must_use]
    pub fn get(&self, id: DraftId) -> Option<&Draft> {
        self.drafts.get(&id)
    }

    /// Every draft, in number order.
    pub fn iter(&self) -> impl Iterator<Item = &Draft> {
        self.drafts.values()
    }

    /// How many drafts there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.drafts.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
    }

    /// The drafts of `ids` grouped by the model that checks them first,
    /// in number order, at most `size` per chunk. `first` names the
    /// checker of a draft, or none when nobody is left to check it.
    #[must_use]
    pub fn chunks(
        &self,
        ids: &[DraftId],
        first: impl Fn(&Draft) -> Option<ModelId>,
        size: usize,
    ) -> Vec<(ModelId, Vec<DraftId>)> {
        let mut by_checker: Vec<(ModelId, Vec<DraftId>)> = Vec::new();
        for draft in ids.iter().filter_map(|id| self.drafts.get(id)) {
            let Some(checker) = first(draft) else {
                continue;
            };
            match by_checker.iter_mut().find(|(model, _)| *model == checker) {
                Some((_, drafts)) => drafts.push(draft.id),
                None => by_checker.push((checker, vec![draft.id])),
            }
        }
        by_checker
            .into_iter()
            .flat_map(|(model, drafts)| {
                drafts
                    .chunks(size.max(1))
                    .map(|chunk| (model.clone(), chunk.to_vec()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// What a draft repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Original {
    /// An earlier draft of this review.
    Draft(DraftId),
    /// A finding already on the pull request, by its comment id.
    Comment(String),
}

impl Original {
    /// Reads `d3`, or anything else as a comment id.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        match text.strip_prefix('d').and_then(|_| DraftId::parse(text)) {
            Some(id) => Self::Draft(id),
            None => Self::Comment(text.to_owned()),
        }
    }
}

impl fmt::Display for Original {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Draft(id) => id.fmt(f),
            Self::Comment(id) => write!(f, "comment {id}"),
        }
    }
}

/// What the fact-check said about a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// It holds.
    Confirmed {
        /// The checking model.
        by: ModelId,
        /// Why.
        reason: String,
    },
    /// It does not.
    Rejected {
        /// The checking model.
        by: ModelId,
        /// Why.
        reason: String,
    },
    /// It says what an earlier draft or an existing finding says.
    SameAs {
        /// The checking model.
        by: ModelId,
        /// What it repeats.
        of: Original,
        /// Why.
        reason: String,
    },
    /// No model could check it; it goes out unchecked and says so (§3.2).
    Unchecked {
        /// Why not.
        why: String,
    },
    /// No fact-check is configured.
    NoCheck,
}

/// What Henk's code does with one draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// Write it.
    Publish {
        /// The draft.
        id: DraftId,
        /// The model that confirmed it, for the comment's marker.
        checked_by: Option<ModelId>,
        /// Why it went out unchecked, when it did.
        unchecked: Option<String>,
    },
    /// Do not write it.
    Reject {
        /// The draft.
        id: DraftId,
        /// The model that rejected it, or that rejected what it repeats.
        by: ModelId,
        /// Why.
        reason: String,
    },
    /// Do not write it: it repeats something that is or will be there.
    Merge {
        /// The draft.
        id: DraftId,
        /// The model that said so.
        by: ModelId,
        /// What it repeats, followed to the end of a chain.
        into: Original,
    },
}

impl Settled {
    /// The draft this is about.
    #[must_use]
    pub fn id(&self) -> DraftId {
        match self {
            Self::Publish { id, .. } | Self::Reject { id, .. } | Self::Merge { id, .. } => *id,
        }
    }
}

/// Turns the verdicts into what happens to each draft, in number order.
///
/// A `same_as` follows what it repeats: a draft repeating one that is
/// published, or a finding already there, is merged into it; one repeating a
/// rejected draft is rejected with it. A `same_as` names an earlier draft
/// only (see [`same_as_allowed`]); one that names anything else, or a draft
/// without a verdict, goes out unchecked.
#[must_use]
pub fn settle(book: &DraftBook, verdicts: &BTreeMap<DraftId, Verdict>) -> Vec<Settled> {
    let mut settled: BTreeMap<DraftId, Settled> = BTreeMap::new();
    for draft in book.iter() {
        let id = draft.id;
        let outcome = match verdicts.get(&id) {
            Some(Verdict::Confirmed { by, .. }) => Settled::Publish {
                id,
                checked_by: Some(by.clone()),
                unchecked: None,
            },
            Some(Verdict::Rejected { by, reason }) => Settled::Reject {
                id,
                by: by.clone(),
                reason: reason.clone(),
            },
            Some(Verdict::SameAs {
                by,
                of: Original::Comment(comment),
                ..
            }) => Settled::Merge {
                id,
                by: by.clone(),
                into: Original::Comment(comment.clone()),
            },
            Some(Verdict::SameAs {
                by,
                of: Original::Draft(earlier),
                reason,
            }) if *earlier < id => match settled.get(earlier) {
                Some(Settled::Publish { id: target, .. }) => Settled::Merge {
                    id,
                    by: by.clone(),
                    into: Original::Draft(*target),
                },
                Some(Settled::Merge { into, .. }) => Settled::Merge {
                    id,
                    by: by.clone(),
                    into: into.clone(),
                },
                Some(Settled::Reject { .. }) => Settled::Reject {
                    id,
                    by: by.clone(),
                    reason: format!("It repeats {earlier}, which was rejected. {reason}"),
                },
                None => unchecked(id, "it repeats a draft that is not there"),
            },
            Some(Verdict::SameAs { .. }) => unchecked(id, "it named a later draft as its original"),
            Some(Verdict::Unchecked { why }) => unchecked(id, why),
            Some(Verdict::NoCheck) => Settled::Publish {
                id,
                checked_by: None,
                unchecked: None,
            },
            None => unchecked(id, "no verdict was given"),
        };
        settled.insert(id, outcome);
    }
    settled.into_values().collect()
}

fn unchecked(id: DraftId, why: &str) -> Settled {
    Settled::Publish {
        id,
        checked_by: None,
        unchecked: Some(why.to_owned()),
    }
}

/// Whether `draft` may be called a repeat of `of`: an earlier draft that
/// exists, or a comment among `comments`.
#[must_use]
pub fn same_as_allowed(book: &DraftBook, draft: DraftId, of: &Original, comments: &[&str]) -> bool {
    match of {
        Original::Draft(earlier) => *earlier < draft && book.get(*earlier).is_some(),
        Original::Comment(comment) => comments.contains(&comment.as_str()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::too_many_lines
    )]

    use super::*;

    fn model(name: &str) -> ModelId {
        ModelId::parse(name).unwrap()
    }

    fn key(line: u32) -> FindingKey {
        FindingKey {
            path: "src/a.rs".into(),
            line,
        }
    }

    fn finding() -> DraftKind {
        DraftKind::Finding {
            side: DiffSide::Right,
        }
    }

    fn book(lines: &[(&str, &str, u32)]) -> DraftBook {
        let mut book = DraftBook::new();
        for (lane, lane_model, line) in lines {
            book.add(
                &LaneName::new(*lane),
                &model(lane_model),
                finding(),
                key(*line),
                "x is never set.",
            )
            .unwrap();
        }
        book
    }

    fn id(n: u32) -> DraftId {
        DraftId(n)
    }

    #[test]
    fn ids_read_back() {
        assert_eq!(DraftId::parse("d3"), Some(id(3)));
        assert_eq!(DraftId::parse(" 3 "), Some(id(3)));
        assert_eq!(DraftId::parse("d0"), None);
        assert_eq!(DraftId::parse("dx"), None);
        assert_eq!(id(12).to_string(), "d12");
        assert_eq!(Original::parse("d2"), Original::Draft(id(2)));
        assert_eq!(Original::parse("12345"), Original::Comment("12345".into()));
        assert_eq!(
            Original::parse("disc-1"),
            Original::Comment("disc-1".into()),
            "a comment id that starts with d stays a comment id"
        );
    }

    #[test]
    fn a_line_holds_one_draft_and_its_own_lane_may_replace_it() {
        let mut book = book(&[("lane-a", "m", 4)]);
        let taken = book
            .add(
                &LaneName::new("lane-b"),
                &model("n"),
                finding(),
                key(4),
                "other",
            )
            .unwrap_err();
        assert_eq!(
            taken,
            Taken {
                id: id(1),
                lane: LaneName::new("lane-a")
            }
        );
        let again = book
            .add(
                &LaneName::new("lane-a"),
                &model("m"),
                finding(),
                key(4),
                "better",
            )
            .unwrap();
        assert_eq!(again, id(1));
        assert_eq!(book.len(), 1);
        assert_eq!(book.get(id(1)).unwrap().text, "better");
        let next = book
            .add(
                &LaneName::new("lane-b"),
                &model("n"),
                finding(),
                key(5),
                "y",
            )
            .unwrap();
        assert_eq!(next, id(2));
    }

    #[test]
    fn chunks_go_by_first_checker_in_order_and_at_most_the_size() {
        let mut lines = Vec::new();
        for line in 1..=11 {
            lines.push(("lane-a", "m", line));
        }
        lines.push(("lane-b", "n", 20));
        let book = book(&lines);
        let ids: Vec<DraftId> = book.iter().map(|d| d.id).collect();
        let other = |d: &Draft| {
            Some(if d.model.as_str() == "m" {
                model("n")
            } else {
                model("m")
            })
        };
        let chunks = book.chunks(&ids, other, CHUNK);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, model("n"));
        assert_eq!(chunks[0].1.len(), 10);
        assert_eq!(chunks[1], (model("n"), vec![id(11)]));
        assert_eq!(chunks[2], (model("m"), vec![id(12)]));
        assert!(
            book.chunks(&ids, |_| None, CHUNK).is_empty(),
            "a draft nobody can check is in no chunk"
        );
    }

    #[test]
    fn verdicts_settle_into_writes_rejections_and_merges() {
        let book = book(&[
            ("lane-a", "m", 1),
            ("lane-b", "n", 2),
            ("lane-a", "m", 3),
            ("lane-b", "n", 4),
            ("lane-a", "m", 5),
            ("lane-a", "m", 6),
            ("lane-b", "n", 7),
            ("lane-b", "n", 8),
        ]);
        let by = model("n");
        let verdicts = BTreeMap::from([
            (
                id(1),
                Verdict::Confirmed {
                    by: by.clone(),
                    reason: "holds".into(),
                },
            ),
            (
                id(2),
                Verdict::Rejected {
                    by: by.clone(),
                    reason: "x is set on line 9".into(),
                },
            ),
            (
                id(3),
                Verdict::SameAs {
                    by: by.clone(),
                    of: Original::Draft(id(1)),
                    reason: "same bug".into(),
                },
            ),
            (
                id(4),
                Verdict::SameAs {
                    by: by.clone(),
                    of: Original::Draft(id(3)),
                    reason: "same again".into(),
                },
            ),
            (
                id(5),
                Verdict::SameAs {
                    by: by.clone(),
                    of: Original::Draft(id(2)),
                    reason: "same claim".into(),
                },
            ),
            (
                id(6),
                Verdict::SameAs {
                    by: by.clone(),
                    of: Original::Comment("c-9".into()),
                    reason: "already there".into(),
                },
            ),
            (
                id(7),
                Verdict::Unchecked {
                    why: "no verdict from m or n".into(),
                },
            ),
        ]);
        let settled = settle(&book, &verdicts);
        assert_eq!(
            settled,
            vec![
                Settled::Publish {
                    id: id(1),
                    checked_by: Some(by.clone()),
                    unchecked: None
                },
                Settled::Reject {
                    id: id(2),
                    by: by.clone(),
                    reason: "x is set on line 9".into()
                },
                Settled::Merge {
                    id: id(3),
                    by: by.clone(),
                    into: Original::Draft(id(1))
                },
                Settled::Merge {
                    id: id(4),
                    by: by.clone(),
                    into: Original::Draft(id(1)),
                },
                Settled::Reject {
                    id: id(5),
                    by: by.clone(),
                    reason: "It repeats d2, which was rejected. same claim".into()
                },
                Settled::Merge {
                    id: id(6),
                    by: by.clone(),
                    into: Original::Comment("c-9".into())
                },
                Settled::Publish {
                    id: id(7),
                    checked_by: None,
                    unchecked: Some("no verdict from m or n".into())
                },
                Settled::Publish {
                    id: id(8),
                    checked_by: None,
                    unchecked: Some("no verdict was given".into())
                },
            ]
        );
    }

    #[test]
    fn a_same_as_names_an_earlier_draft_or_a_known_comment() {
        let book = book(&[("lane-a", "m", 1), ("lane-b", "n", 2)]);
        assert!(same_as_allowed(&book, id(2), &Original::Draft(id(1)), &[]));
        assert!(!same_as_allowed(&book, id(1), &Original::Draft(id(2)), &[]));
        assert!(!same_as_allowed(&book, id(1), &Original::Draft(id(1)), &[]));
        assert!(!same_as_allowed(&book, id(2), &Original::Draft(id(9)), &[]));
        assert!(same_as_allowed(
            &book,
            id(1),
            &Original::Comment("c-1".into()),
            &["c-1"]
        ));
        assert!(!same_as_allowed(
            &book,
            id(1),
            &Original::Comment("c-2".into()),
            &["c-1"]
        ));
        let later = BTreeMap::from([(
            id(1),
            Verdict::SameAs {
                by: model("n"),
                of: Original::Draft(id(2)),
                reason: "r".into(),
            },
        )]);
        assert!(matches!(
            settle(&book, &later)[0],
            Settled::Publish {
                unchecked: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn without_a_checker_every_draft_is_published_unmarked() {
        let book = book(&[("lane-a", "m", 1)]);
        let verdicts = BTreeMap::from([(id(1), Verdict::NoCheck)]);
        assert_eq!(
            settle(&book, &verdicts),
            vec![Settled::Publish {
                id: id(1),
                checked_by: None,
                unchecked: None
            }]
        );
    }
}
