//! Scope guards (§8.5): a lane may read only its own repository, and a
//! review lane only its own pull request at its own commit.
//!
//! The guard runs in Henk's process on every MCP tool call a model makes,
//! before the call leaves for the server. It knows the tools by name and,
//! for each, which arguments to pin. Unknown tools are refused. Nothing
//! that writes is ever in these tables; writes go through Henk's own tools.

use serde_json::{Value, json};

use crate::allowlist::{Platform, RepoRef};
use crate::review::CommitSha;

/// What a task may touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One review lane: this pull/merge request at this commit.
    Review {
        /// The repository.
        repo: RepoRef,
        /// Pull/merge request number (GitLab: iid).
        number: u64,
        /// The reviewed commit.
        commit: CommitSha,
    },
    /// One planner: this issue and its relations in this repository.
    Plan {
        /// The repository.
        repo: RepoRef,
        /// Issue number (GitLab: iid).
        issue: u64,
    },
}

impl Scope {
    /// The repository of the scope.
    #[must_use]
    pub fn repo(&self) -> &RepoRef {
        match self {
            Self::Review { repo, .. } | Self::Plan { repo, .. } => repo,
        }
    }
}

/// What the guard decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Forward with these arguments.
    Allow(Value),
    /// Refuse; the reason goes back to the model.
    Deny(String),
}

/// How one argument is pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// `owner` is the repository owner (GitHub).
    Owner,
    /// `repo` is the repository name (GitHub).
    Repo,
    /// `pullNumber` is the reviewed pull request (GitHub, review scope only).
    PullNumber,
    /// `query` gets `repo:owner/name` and loses any other repo qualifier (GitHub search).
    SearchQueryRepo,
    /// `sha` defaults to the reviewed commit when neither `sha` nor `ref` is set (GitHub).
    DefaultShaToCommit,
    /// `project_id` is the project path (GitLab).
    ProjectId,
    /// `merge_request_iid` is the reviewed merge request (GitLab, review scope only).
    MergeRequestIid,
}

const GITHUB_READ: &[(&str, &[Rule])] = &[
    (
        "pull_request_read",
        &[Rule::Owner, Rule::Repo, Rule::PullNumber],
    ),
    (
        "get_file_contents",
        &[Rule::Owner, Rule::Repo, Rule::DefaultShaToCommit],
    ),
    ("get_commit", &[Rule::Owner, Rule::Repo]),
    ("list_commits", &[Rule::Owner, Rule::Repo]),
    ("list_branches", &[Rule::Owner, Rule::Repo]),
    ("search_code", &[Rule::SearchQueryRepo]),
    ("issue_read", &[Rule::Owner, Rule::Repo]),
    ("list_issues", &[Rule::Owner, Rule::Repo]),
    (
        "search_issues",
        &[Rule::Owner, Rule::Repo, Rule::SearchQueryRepo],
    ),
    ("list_pull_requests", &[Rule::Owner, Rule::Repo]),
    (
        "search_pull_requests",
        &[Rule::Owner, Rule::Repo, Rule::SearchQueryRepo],
    ),
    ("get_label", &[Rule::Owner, Rule::Repo]),
    ("list_issue_types", &[Rule::Owner, Rule::Repo]),
];

const GITLAB_READ: &[(&str, &[Rule])] = &[
    (
        "get_merge_request",
        &[Rule::ProjectId, Rule::MergeRequestIid],
    ),
    (
        "get_merge_request_diffs",
        &[Rule::ProjectId, Rule::MergeRequestIid],
    ),
    (
        "list_merge_request_diffs",
        &[Rule::ProjectId, Rule::MergeRequestIid],
    ),
    (
        "get_merge_request_file_diff",
        &[Rule::ProjectId, Rule::MergeRequestIid],
    ),
    (
        "list_merge_request_changed_files",
        &[Rule::ProjectId, Rule::MergeRequestIid],
    ),
    ("mr_discussions", &[Rule::ProjectId, Rule::MergeRequestIid]),
    ("list_merge_requests", &[Rule::ProjectId]),
    ("get_file_contents", &[Rule::ProjectId]),
    ("get_repository_tree", &[Rule::ProjectId]),
    ("list_commits", &[Rule::ProjectId]),
    ("get_commit", &[Rule::ProjectId]),
    ("get_commit_diff", &[Rule::ProjectId]),
    ("get_branch_diffs", &[Rule::ProjectId]),
    ("search_repositories", &[]),
    ("get_issue", &[Rule::ProjectId]),
    ("list_issues", &[Rule::ProjectId]),
    ("list_issue_links", &[Rule::ProjectId]),
    ("list_issue_discussions", &[Rule::ProjectId]),
    ("list_labels", &[Rule::ProjectId]),
    ("get_project", &[Rule::ProjectId]),
];

fn table(platform: Platform) -> &'static [(&'static str, &'static [Rule])] {
    match platform {
        Platform::GitHub => GITHUB_READ,
        Platform::GitLab => GITLAB_READ,
    }
}

/// The server tools a scope exposes to a model, by platform.
pub fn exposed_tools(platform: Platform) -> impl Iterator<Item = &'static str> {
    table(platform).iter().map(|(name, _)| *name)
}

/// Whether a server tool is exposed at all.
#[must_use]
pub fn is_exposed(platform: Platform, tool: &str) -> bool {
    table(platform).iter().any(|(name, _)| *name == tool)
}

/// Decides one call: refuses unknown tools, pins the arguments that name
/// the repository and target, and returns the arguments to forward.
#[must_use]
pub fn guard(platform: Platform, tool: &str, arguments: &Value, scope: &Scope) -> Verdict {
    if platform != scope.repo().platform() {
        return Verdict::Deny(format!("{tool} belongs to another platform than this task"));
    }
    let Some((_, rules)) = table(platform).iter().find(|(name, _)| *name == tool) else {
        return Verdict::Deny(format!("{tool} is not available in this task"));
    };
    let mut args = match arguments {
        Value::Object(map) => map.clone(),
        Value::Null => serde_json::Map::new(),
        _ => return Verdict::Deny("arguments must be a JSON object".to_owned()),
    };
    let repo = scope.repo();
    for rule in *rules {
        match rule {
            Rule::Owner => {
                args.insert("owner".into(), json!(repo.owner()));
            }
            Rule::Repo => {
                args.insert("repo".into(), json!(repo.name()));
            }
            Rule::ProjectId => {
                args.insert("project_id".into(), json!(repo.path()));
            }
            Rule::PullNumber => {
                if let Scope::Review { number, .. } = scope {
                    args.insert("pullNumber".into(), json!(number));
                } else if args.get("pullNumber").is_none() {
                    return Verdict::Deny(format!("{tool} needs pullNumber"));
                }
            }
            Rule::MergeRequestIid => {
                if let Scope::Review { number, .. } = scope {
                    args.insert("merge_request_iid".into(), json!(number.to_string()));
                } else if args.get("merge_request_iid").is_none() {
                    return Verdict::Deny(format!("{tool} needs merge_request_iid"));
                }
            }
            Rule::SearchQueryRepo => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                args.insert("query".into(), json!(pin_search_query(query, &repo.path())));
            }
            Rule::DefaultShaToCommit => {
                if let Scope::Review { commit, .. } = scope
                    && args.get("sha").is_none()
                    && args.get("ref").is_none()
                {
                    args.insert("sha".into(), json!(commit.as_str()));
                }
            }
        }
    }
    Verdict::Allow(Value::Object(args))
}

/// Removes `repo:`, `org:` and `user:` qualifiers and appends ours.
fn pin_search_query(query: &str, repo_path: &str) -> String {
    let mut kept: Vec<&str> = query
        .split_whitespace()
        .filter(|word| {
            let lower = word.to_ascii_lowercase();
            !(lower.starts_with("repo:") || lower.starts_with("org:") || lower.starts_with("user:"))
        })
        .collect();
    let pin = format!("repo:{repo_path}");
    kept.push(&pin);
    kept.join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn review_scope() -> Scope {
        Scope::Review {
            repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            number: 42,
            commit: CommitSha::parse(SHA).unwrap(),
        }
    }

    #[test]
    fn unknown_and_write_tools_are_refused() {
        let scope = review_scope();
        assert!(matches!(
            guard(Platform::GitHub, "merge_pull_request", &json!({}), &scope),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            guard(Platform::GitHub, "push_files", &json!({}), &scope),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            guard(
                Platform::GitHub,
                "pull_request_review_write",
                &json!({}),
                &scope
            ),
            Verdict::Deny(_)
        ));
        assert!(
            matches!(
                guard(Platform::GitLab, "get_merge_request", &json!({}), &scope),
                Verdict::Deny(_)
            ),
            "other platform"
        );
    }

    #[test]
    fn repository_and_pull_number_are_pinned() {
        let verdict = guard(
            Platform::GitHub,
            "pull_request_read",
            &json!({"owner": "evil", "repo": "other", "pullNumber": 1, "method": "get_diff"}),
            &review_scope(),
        );
        assert_eq!(
            verdict,
            Verdict::Allow(
                json!({"owner": "docspec", "repo": "app", "pullNumber": 42, "method": "get_diff"})
            )
        );
    }

    #[test]
    fn file_reads_default_to_the_reviewed_commit() {
        let verdict = guard(
            Platform::GitHub,
            "get_file_contents",
            &json!({"path": "src/main.rs"}),
            &review_scope(),
        );
        assert_eq!(
            verdict,
            Verdict::Allow(
                json!({"owner": "docspec", "repo": "app", "path": "src/main.rs", "sha": SHA})
            )
        );
        let explicit = guard(
            Platform::GitHub,
            "get_file_contents",
            &json!({"path": "x", "ref": "refs/heads/main"}),
            &review_scope(),
        );
        assert_eq!(
            explicit,
            Verdict::Allow(
                json!({"owner": "docspec", "repo": "app", "path": "x", "ref": "refs/heads/main"})
            )
        );
    }

    #[test]
    fn search_queries_are_confined_to_the_repository() {
        let verdict = guard(
            Platform::GitHub,
            "search_code",
            &json!({"query": "fn main repo:evil/x org:evil language:rust"}),
            &review_scope(),
        );
        assert_eq!(
            verdict,
            Verdict::Allow(json!({"query": "fn main language:rust repo:docspec/app"}))
        );
    }

    #[test]
    fn plan_scope_allows_any_pull_request_in_the_repository_but_requires_one() {
        let scope = Scope::Plan {
            repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            issue: 9,
        };
        assert_eq!(
            guard(
                Platform::GitHub,
                "pull_request_read",
                &json!({"pullNumber": 3, "method": "get"}),
                &scope
            ),
            Verdict::Allow(
                json!({"owner": "docspec", "repo": "app", "pullNumber": 3, "method": "get"})
            )
        );
        assert!(matches!(
            guard(
                Platform::GitHub,
                "pull_request_read",
                &json!({"method": "get"}),
                &scope
            ),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn gitlab_pins_project_and_merge_request() {
        let scope = Scope::Review {
            repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
            number: 5,
            commit: CommitSha::parse(SHA).unwrap(),
        };
        assert_eq!(
            guard(
                Platform::GitLab,
                "get_merge_request",
                &json!({"project_id": "1"}),
                &scope
            ),
            Verdict::Allow(json!({"project_id": "9xxlab/tools/cli", "merge_request_iid": "5"}))
        );
    }

    #[test]
    fn exposed_tools_lists_only_reads() {
        let write_verbs = [
            "create_", "update_", "delete_", "push_", "merge_", "approve", "publish", "fork_",
            "set_", "add_", "remove_", "resolve_",
        ];
        for tool in exposed_tools(Platform::GitHub).chain(exposed_tools(Platform::GitLab)) {
            assert!(!tool.contains("write"), "{tool} looks like a write");
            for verb in write_verbs {
                assert!(!tool.starts_with(verb), "{tool} looks like a write");
            }
        }
        assert!(is_exposed(Platform::GitHub, "pull_request_read"));
        assert!(!is_exposed(Platform::GitHub, "issue_write"));
    }
}
