//! Repository allowlist (§2, §8.7).
//!
//! Henk works only in allowlisted repositories. Outside them he says he does
//! not work there.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A code hosting platform Henk works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// GitHub: pull requests, review comments, checks.
    GitHub,
    /// GitLab: merge requests, diff discussions, commit statuses.
    GitLab,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
        })
    }
}

/// Why a repository reference could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepoRefError {
    /// The reference has no `/` separating owner and name.
    #[error("repository reference {0:?} must be owner/name")]
    MissingSeparator(String),
    /// A path segment is empty.
    #[error("repository reference {0:?} has an empty segment")]
    EmptySegment(String),
    /// A segment contains whitespace or another forbidden character.
    #[error("repository reference {0:?} contains an invalid character")]
    InvalidCharacter(String),
}

/// A repository on a platform.
///
/// On GitHub `owner` is a user or organisation. On GitLab it is the full
/// namespace path, so `9xxlab/tools/cli` has owner `9xxlab/tools` and name
/// `cli`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    platform: Platform,
    owner: String,
    name: String,
}

impl RepoRef {
    /// Parses `owner/name` (GitLab: `group/subgroup/name`).
    ///
    /// # Errors
    ///
    /// Returns [`RepoRefError`] when the path has no separator, an empty
    /// segment, or a character that cannot appear in a repository path.
    pub fn parse(platform: Platform, path: &str) -> Result<Self, RepoRefError> {
        let path = path.trim().trim_matches('/');
        let Some((owner, name)) = path.rsplit_once('/') else {
            return Err(RepoRefError::MissingSeparator(path.to_owned()));
        };
        if owner.split('/').any(str::is_empty) || name.is_empty() {
            return Err(RepoRefError::EmptySegment(path.to_owned()));
        }
        if path
            .chars()
            .any(|c| c.is_whitespace() || c == '@' || c == ':' || c == '\\')
        {
            return Err(RepoRefError::InvalidCharacter(path.to_owned()));
        }
        Ok(Self {
            platform,
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The platform.
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// The owner, organisation or namespace path.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The repository name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/name`.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.platform, self.owner, self.name)
    }
}

/// One allowlist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowRule {
    /// Exactly this repository.
    Repository(RepoRef),
    /// Every repository under this owner. On GitLab this includes subgroups.
    Owner {
        /// The platform.
        platform: Platform,
        /// The GitHub account or GitLab group path.
        owner: String,
    },
}

impl AllowRule {
    fn matches(&self, repo: &RepoRef) -> bool {
        match self {
            Self::Repository(allowed) => {
                allowed.platform == repo.platform
                    && allowed.owner.eq_ignore_ascii_case(&repo.owner)
                    && allowed.name.eq_ignore_ascii_case(&repo.name)
            }
            Self::Owner { platform, owner } => {
                if *platform != repo.platform {
                    return false;
                }
                let repo_owner = repo.owner.to_ascii_lowercase();
                let owner = owner.to_ascii_lowercase();
                match platform {
                    Platform::GitHub => repo_owner == owner,
                    Platform::GitLab => {
                        repo_owner == owner
                            || repo_owner
                                .strip_prefix(&owner)
                                .is_some_and(|rest| rest.starts_with('/'))
                    }
                }
            }
        }
    }
}

/// The set of places Henk works (§2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Allowlist {
    rules: Vec<AllowRule>,
}

impl Allowlist {
    /// Builds an allowlist from rules.
    #[must_use]
    pub fn new(rules: impl IntoIterator<Item = AllowRule>) -> Self {
        Self {
            rules: rules.into_iter().collect(),
        }
    }

    /// Whether Henk works in `repo`.
    #[must_use]
    pub fn allows(&self, repo: &RepoRef) -> bool {
        self.rules.iter().any(|rule| rule.matches(repo))
    }

    /// The rules, in order.
    #[must_use]
    pub fn rules(&self) -> &[AllowRule] {
        &self.rules
    }

    /// Whether the allowlist is empty. An empty allowlist means Henk works nowhere.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn gh(path: &str) -> RepoRef {
        RepoRef::parse(Platform::GitHub, path).unwrap_or_else(|e| panic!("{e}"))
    }

    fn gl(path: &str) -> RepoRef {
        RepoRef::parse(Platform::GitLab, path).unwrap_or_else(|e| panic!("{e}"))
    }

    fn team_allowlist() -> Allowlist {
        Allowlist::new([
            AllowRule::Repository(gh("StephanMeijer/scratch-repo")),
            AllowRule::Owner {
                platform: Platform::GitHub,
                owner: "docspec".into(),
            },
            AllowRule::Owner {
                platform: Platform::GitHub,
                owner: "NotedThat".into(),
            },
            AllowRule::Owner {
                platform: Platform::GitLab,
                owner: "9xxlab".into(),
            },
        ])
    }

    #[test]
    fn parses_github_and_gitlab_paths() {
        let repo = gh("StephanMeijer/scratch-repo");
        assert_eq!(repo.owner(), "StephanMeijer");
        assert_eq!(repo.name(), "scratch-repo");

        let repo = gl("9xxlab/tools/cli");
        assert_eq!(repo.owner(), "9xxlab/tools");
        assert_eq!(repo.name(), "cli");
        assert_eq!(repo.path(), "9xxlab/tools/cli");
    }

    #[test]
    fn rejects_malformed_paths() {
        assert!(matches!(
            RepoRef::parse(Platform::GitHub, "scratch-repo"),
            Err(RepoRefError::MissingSeparator(_))
        ));
        assert!(matches!(
            RepoRef::parse(Platform::GitHub, "a//b"),
            Err(RepoRefError::EmptySegment(_))
        ));
        assert!(matches!(
            RepoRef::parse(Platform::GitHub, "a/b c"),
            Err(RepoRefError::InvalidCharacter(_))
        ));
    }

    #[test]
    fn exact_repository_matches_case_insensitively() {
        let list = team_allowlist();
        assert!(list.allows(&gh("stephanmeijer/Scratch-Repo")));
        assert!(!list.allows(&gh("StephanMeijer/other")));
    }

    #[test]
    fn owner_rule_matches_every_repository_of_the_account() {
        let list = team_allowlist();
        assert!(list.allows(&gh("docspec/anything")));
        assert!(list.allows(&gh("notedthat/app")));
        assert!(!list.allows(&gh("docspec-fork/anything")));
    }

    #[test]
    fn gitlab_group_rule_includes_subgroups() {
        let list = team_allowlist();
        assert!(list.allows(&gl("9xxlab/app")));
        assert!(list.allows(&gl("9xxlab/tools/cli")));
        assert!(!list.allows(&gl("9xxlab-archive/app")));
        assert!(!list.allows(&gh("9xxlab/app")), "platform matters");
    }

    #[test]
    fn empty_allowlist_allows_nothing() {
        assert!(!Allowlist::default().allows(&gh("docspec/anything")));
    }
}
