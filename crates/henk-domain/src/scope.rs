//! Scope guards (§8.5): a lane may read only its own repository, and a
//! review lane only its own pull request. In review scope, file and tree
//! reads are pinned to the reviewed commit on both platforms; commit
//! history (`get_commit`, `list_commits`) is not.
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
    /// `query` gets `repo:owner/name`; a qualifier naming anything else is
    /// refused (GitHub search). The pin is joined to the query by an
    /// implicit AND, which boolean syntax can escape, so parentheses, `OR`,
    /// `NOT` and unclosed or escaped quotes are refused outright rather than
    /// judged by context. Text inside a quoted phrase is literal.
    SearchQueryRepo,
    /// Reads happen at the reviewed commit: `ref` is dropped and `sha`
    /// defaults to the commit (GitHub, review scope). An explicit `sha` is
    /// kept, so the base can still be read.
    PinToCommit,
    /// These `method` values are refused in review scope, because Henk's
    /// own tools serve the same data per file (GitHub `pull_request_read`).
    DenyWholeDiffMethods,
    /// `project_id` is the project path (GitLab).
    ProjectId,
    /// `merge_request_iid` is the reviewed merge request (GitLab, review scope only).
    MergeRequestIid,
    /// Reads happen at the reviewed commit: `ref` is set to the commit,
    /// replacing any `ref` the model gave (GitLab `get_file_contents` and
    /// `get_repository_tree`, review scope). Plan scope has no commit, so
    /// `ref` is kept there. `list_commits` and `get_commit` are not pinned;
    /// they read history of the same project.
    PinRef,
}

const GITHUB_READ: &[(&str, &[Rule])] = &[
    (
        "pull_request_read",
        &[
            Rule::Owner,
            Rule::Repo,
            Rule::PullNumber,
            Rule::DenyWholeDiffMethods,
        ],
    ),
    (
        "get_file_contents",
        &[Rule::Owner, Rule::Repo, Rule::PinToCommit],
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

// Whole-diff tools (get_merge_request_diffs, get_commit_diff, ...) are not
// here on purpose: the review serves its own diff per file, numbered, and
// the planner reads merge requests through get_merge_request.
const GITLAB_READ: &[(&str, &[Rule])] = &[
    (
        "get_merge_request",
        &[Rule::ProjectId, Rule::MergeRequestIid],
    ),
    ("mr_discussions", &[Rule::ProjectId, Rule::MergeRequestIid]),
    ("list_merge_requests", &[Rule::ProjectId]),
    ("get_file_contents", &[Rule::ProjectId, Rule::PinRef]),
    ("get_repository_tree", &[Rule::ProjectId, Rule::PinRef]),
    ("list_commits", &[Rule::ProjectId]),
    ("get_commit", &[Rule::ProjectId]),
    ("get_issue", &[Rule::ProjectId]),
    ("list_issues", &[Rule::ProjectId]),
    ("list_issue_links", &[Rule::ProjectId]),
    ("list_issue_discussions", &[Rule::ProjectId]),
    // Work items: GitLab issues and tasks with hierarchy, weight, dates and health.
    ("get_work_item", &[Rule::ProjectId]),
    ("list_work_items", &[Rule::ProjectId]),
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
    // A value the model gave that names something else is refused rather
    // than replaced: a silent swap answers a different question than the
    // one asked, and the model cannot tell.
    for rule in *rules {
        match rule {
            Rule::Owner => {
                if let Some(other) = differs(&args, "owner", repo.owner(), Match::IgnoreCase) {
                    return outside(repo, "owner", &other);
                }
                args.insert("owner".into(), json!(repo.owner()));
            }
            Rule::Repo => {
                if let Some(other) = differs(&args, "repo", repo.name(), Match::IgnoreCase) {
                    return outside(repo, "repo", &other);
                }
                args.insert("repo".into(), json!(repo.name()));
            }
            Rule::ProjectId => {
                // A numeric project id cannot be checked here; it is pinned.
                let numeric = args
                    .get("project_id")
                    .is_some_and(|v| v.is_u64() || v.as_str().is_some_and(is_number));
                if !numeric
                    && let Some(other) =
                        differs(&args, "project_id", &repo.path(), Match::IgnoreCase)
                {
                    return outside(repo, "project_id", &other);
                }
                args.insert("project_id".into(), json!(repo.path()));
            }
            Rule::PullNumber => {
                if let Scope::Review { number, .. } = scope {
                    if let Some(other) =
                        differs(&args, "pullNumber", &number.to_string(), Match::Exact)
                    {
                        return Verdict::Deny(format!(
                            "this review reads only pull request {number}; {other} is outside it. Leave pullNumber out."
                        ));
                    }
                    args.insert("pullNumber".into(), json!(number));
                } else if args.get("pullNumber").is_none() {
                    return Verdict::Deny(format!("{tool} needs pullNumber"));
                }
            }
            Rule::MergeRequestIid => {
                if let Scope::Review { number, .. } = scope {
                    if let Some(other) = differs(
                        &args,
                        "merge_request_iid",
                        &number.to_string(),
                        Match::Exact,
                    ) {
                        return Verdict::Deny(format!(
                            "this review reads only merge request {number}; {other} is outside it. Leave merge_request_iid out."
                        ));
                    }
                    args.insert("merge_request_iid".into(), json!(number.to_string()));
                } else if args.get("merge_request_iid").is_none() {
                    return Verdict::Deny(format!("{tool} needs merge_request_iid"));
                }
            }
            Rule::SearchQueryRepo => {
                if let Err(refused) = pin_search(&mut args, repo) {
                    return refused;
                }
            }
            Rule::PinToCommit => {
                if let Scope::Review { commit, .. } = scope {
                    args.remove("ref");
                    if args.get("sha").is_none() {
                        args.insert("sha".into(), json!(commit.as_str()));
                    }
                }
            }
            Rule::PinRef => {
                if let Scope::Review { commit, .. } = scope {
                    args.insert("ref".into(), json!(commit.as_str()));
                }
            }
            Rule::DenyWholeDiffMethods => {
                if matches!(scope, Scope::Review { .. })
                    && let Some(method) = args.get("method").and_then(Value::as_str)
                    && matches!(method, "get_diff" | "get_files")
                {
                    return Verdict::Deny(format!(
                        "{method} is not used here: call list_changed_files for the files and get_file_diff for one file's numbered diff"
                    ));
                }
            }
        }
    }
    Verdict::Allow(Value::Object(args))
}

#[derive(Clone, Copy)]
enum Match {
    Exact,
    IgnoreCase,
}

/// The value of `key` as text when the model gave one and it is not
/// `expected`. Absent and null values do not differ: they are pinned.
fn differs(
    args: &serde_json::Map<String, Value>,
    key: &str,
    expected: &str,
    how: Match,
) -> Option<String> {
    let given = match args.get(key)? {
        Value::Null => return None,
        Value::String(text) => text.trim().to_owned(),
        other => other.to_string(),
    };
    let same = match how {
        Match::Exact => given == expected,
        Match::IgnoreCase => given.eq_ignore_ascii_case(expected),
    };
    (!same).then_some(given)
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn outside(repo: &RepoRef, what: &str, given: &str) -> Verdict {
    Verdict::Deny(format!(
        "this task reads only {}; {what} {given:?} is outside it. Leave {what} out to read this repository.",
        repo.path()
    ))
}

/// Pins `query` to the repository, or the refusal to send back.
fn pin_search(args: &mut serde_json::Map<String, Value>, repo: &RepoRef) -> Result<(), Verdict> {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    match pin_search_query(query, repo) {
        Ok(pinned) => {
            args.insert("query".into(), json!(pinned));
            Ok(())
        }
        Err(SearchRefusal::Qualifier(qualifier)) => {
            Err(outside(repo, "search qualifier", &qualifier))
        }
        Err(SearchRefusal::Syntax(reason)) => Err(Verdict::Deny(reason.to_owned())),
    }
}

/// Why a search query is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchRefusal {
    /// A `repo:`, `org:`, `user:` or `owner:` qualifier naming something else.
    Qualifier(String),
    /// Syntax that could escape the appended pin.
    Syntax(&'static str),
}

const UNCLOSED_QUOTE: &str =
    "the search query has an unclosed or escaped quote; close every phrase in plain double quotes";
const PARENTHESES: &str = "parentheses are not used in search queries here; put code with parentheses in double quotes, for example \"parse(\"";
const OPERATORS: &str =
    "OR and NOT are not used in search queries here; search for one thing at a time";

/// One whitespace-separated word of a query. `plain` is the part outside
/// double quotes; a word with no plain part is a quoted phrase.
struct SearchWord {
    text: String,
    plain: String,
}

fn search_words(query: &str) -> Result<Vec<SearchWord>, SearchRefusal> {
    if query.contains("\\\"") {
        return Err(SearchRefusal::Syntax(UNCLOSED_QUOTE));
    }
    let mut words = Vec::new();
    let mut text = String::new();
    let mut plain = String::new();
    let mut quoted = false;
    for c in query.chars() {
        if c == '"' {
            quoted = !quoted;
            text.push(c);
        } else if quoted {
            text.push(c);
        } else if c.is_whitespace() {
            if !text.is_empty() {
                words.push(SearchWord {
                    text: std::mem::take(&mut text),
                    plain: std::mem::take(&mut plain),
                });
            }
        } else {
            text.push(c);
            plain.push(c);
        }
    }
    if quoted {
        return Err(SearchRefusal::Syntax(UNCLOSED_QUOTE));
    }
    if !text.is_empty() {
        words.push(SearchWord { text, plain });
    }
    Ok(words)
}

/// Replaces `repo:`, `org:`, `user:` and `owner:` qualifiers naming this
/// repository or its owner with one `repo:owner/name`, appended. Refuses a
/// qualifier naming anything else, and any syntax that could escape the
/// pin (see [`Rule::SearchQueryRepo`]). Quoted phrases are kept as they are.
fn pin_search_query(query: &str, repo: &RepoRef) -> Result<String, SearchRefusal> {
    let path = repo.path();
    let mut kept: Vec<String> = Vec::new();
    for word in search_words(query)? {
        if word.plain.is_empty() {
            kept.push(word.text);
            continue;
        }
        if word.plain.contains(['(', ')']) {
            return Err(SearchRefusal::Syntax(PARENTHESES));
        }
        if matches!(word.text.as_str(), "OR" | "NOT") {
            return Err(SearchRefusal::Syntax(OPERATORS));
        }
        let body = word.text.strip_prefix('-').unwrap_or(&word.text);
        let Some((key, value)) = body.split_once(':') else {
            kept.push(word.text);
            continue;
        };
        let value = value.trim_matches('"');
        let ours = match key.to_ascii_lowercase().as_str() {
            "repo" => value.eq_ignore_ascii_case(&path),
            "org" | "user" | "owner" => value.eq_ignore_ascii_case(repo.owner()),
            _ => {
                kept.push(word.text);
                continue;
            }
        };
        if !ours {
            return Err(SearchRefusal::Qualifier(word.text));
        }
    }
    kept.push(format!("repo:{path}"));
    Ok(kept.join(" "))
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

    fn plan_scope() -> Scope {
        Scope::Plan {
            repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            issue: 9,
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
    fn repository_and_pull_number_are_pinned_when_left_out_or_matching() {
        let expected = Verdict::Allow(
            json!({"owner": "docspec", "repo": "app", "pullNumber": 42, "method": "get_commits"}),
        );
        for given in [
            json!({"method": "get_commits"}),
            json!({"owner": "DocSpec", "repo": "App", "pullNumber": 42, "method": "get_commits"}),
            json!({"owner": null, "pullNumber": "42", "method": "get_commits"}),
        ] {
            assert_eq!(
                guard(
                    Platform::GitHub,
                    "pull_request_read",
                    &given,
                    &review_scope()
                ),
                expected,
                "{given}"
            );
        }
    }

    #[test]
    fn another_repository_or_pull_request_is_refused_not_swapped() {
        for given in [
            json!({"owner": "actions", "repo": "checkout", "sha": "main"}),
            json!({"owner": "docspec", "repo": "other", "sha": "main"}),
        ] {
            let Verdict::Deny(reason) =
                guard(Platform::GitHub, "get_commit", &given, &review_scope())
            else {
                panic!("{given} must be refused");
            };
            assert!(reason.contains("docspec/app"), "{reason}");
        }
        assert!(matches!(
            guard(
                Platform::GitHub,
                "pull_request_read",
                &json!({"pullNumber": 1, "method": "get"}),
                &review_scope()
            ),
            Verdict::Deny(_)
        ));
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
        // A ref is dropped: the lane reads the commit under review, not a
        // branch that may have moved on.
        let with_ref = guard(
            Platform::GitHub,
            "get_file_contents",
            &json!({"path": "x", "ref": "refs/pull/7/head"}),
            &review_scope(),
        );
        assert_eq!(
            with_ref,
            Verdict::Allow(json!({"owner": "docspec", "repo": "app", "path": "x", "sha": SHA}))
        );
        // An explicit sha is kept, so the base version can be read.
        let explicit = guard(
            Platform::GitHub,
            "get_file_contents",
            &json!({"path": "x", "sha": "abc"}),
            &review_scope(),
        );
        assert_eq!(
            explicit,
            Verdict::Allow(json!({"owner": "docspec", "repo": "app", "path": "x", "sha": "abc"}))
        );
    }

    #[test]
    fn whole_diff_methods_are_refused_in_review_scope() {
        for method in ["get_diff", "get_files"] {
            let verdict = guard(
                Platform::GitHub,
                "pull_request_read",
                &json!({"method": method}),
                &review_scope(),
            );
            assert!(
                matches!(verdict, Verdict::Deny(ref reason) if reason.contains("get_file_diff")),
                "{method}: {verdict:?}"
            );
        }
        let get = guard(
            Platform::GitHub,
            "pull_request_read",
            &json!({"method": "get"}),
            &review_scope(),
        );
        assert!(matches!(get, Verdict::Allow(_)));
        let plan = guard(
            Platform::GitHub,
            "pull_request_read",
            &json!({"method": "get_diff", "pullNumber": 3}),
            &plan_scope(),
        );
        assert!(
            matches!(plan, Verdict::Allow(_)),
            "planning may read whole diffs"
        );
    }

    #[test]
    fn search_queries_are_confined_to_the_repository() {
        let verdict = guard(
            Platform::GitHub,
            "search_code",
            &json!({"query": "fn main repo:DocSpec/app org:docspec language:rust"}),
            &review_scope(),
        );
        assert_eq!(
            verdict,
            Verdict::Allow(json!({"query": "fn main language:rust repo:docspec/app"}))
        );
        for query in ["fn main repo:evil/x", "fn main org:evil", "user:someone x"] {
            assert!(
                matches!(
                    guard(
                        Platform::GitHub,
                        "search_code",
                        &json!({"query": query}),
                        &review_scope()
                    ),
                    Verdict::Deny(_)
                ),
                "{query}"
            );
        }
    }

    #[test]
    fn search_syntax_that_could_escape_the_pin_is_refused() {
        let search = |tool: &str, query: &str| {
            guard(
                Platform::GitHub,
                tool,
                &json!({"query": query}),
                &review_scope(),
            )
        };
        for query in [
            "x (repo:evil/secret)",
            "x OR (org:evil)",
            "x OR (repo:evil/x)",
            "x OR y",
            "NOT x",
            "owner:evil x",
            "-repo:evil/x",
            "Repo:\"evil/x\"",
            "\"x",
            "\"a\\\" b\"",
            "parse(",
        ] {
            for tool in ["search_code", "search_issues", "search_pull_requests"] {
                assert!(
                    matches!(search(tool, query), Verdict::Deny(_)),
                    "{tool}: {query}"
                );
            }
        }
        let Verdict::Deny(reason) = search("search_code", "parse(") else {
            panic!("parentheses are refused");
        };
        assert!(reason.contains("double quotes"), "{reason}");
        assert_eq!(
            search("search_issues", "x OR (repo:evil/x)"),
            Verdict::Deny(OPERATORS.to_owned())
        );
        for (query, pinned) in [
            (
                "\"parse(\" language:rust",
                "\"parse(\" language:rust repo:docspec/app",
            ),
            (
                "\"repo:evil/x\" readme",
                "\"repo:evil/x\" readme repo:docspec/app",
            ),
            (
                "label:\"good first issue\" is:open",
                "label:\"good first issue\" is:open repo:docspec/app",
            ),
            ("a AND b", "a AND b repo:docspec/app"),
            ("owner:DocSpec -repo:docspec/app x", "x repo:docspec/app"),
            ("", "repo:docspec/app"),
        ] {
            assert_eq!(
                search("search_code", query),
                Verdict::Allow(json!({"query": pinned})),
                "{query}"
            );
        }
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
        assert!(matches!(
            guard(
                Platform::GitLab,
                "get_merge_request",
                &json!({"project_id": "other/project"}),
                &scope
            ),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            guard(
                Platform::GitLab,
                "search_repositories",
                &json!({"search": "secret"}),
                &scope
            ),
            Verdict::Deny(_)
        ));
        assert_eq!(
            guard(
                Platform::GitLab,
                "get_file_contents",
                &json!({"file_path": "a.rs", "ref": "other-branch"}),
                &scope
            ),
            Verdict::Allow(
                json!({"project_id": "9xxlab/tools/cli", "file_path": "a.rs", "ref": SHA})
            )
        );
        assert_eq!(
            guard(
                Platform::GitLab,
                "get_repository_tree",
                &json!({"path": "src"}),
                &scope
            ),
            Verdict::Allow(json!({"project_id": "9xxlab/tools/cli", "path": "src", "ref": SHA}))
        );
        let plan = Scope::Plan {
            repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
            issue: 3,
        };
        assert_eq!(
            guard(
                Platform::GitLab,
                "get_file_contents",
                &json!({"file_path": "a.rs", "ref": "main"}),
                &plan
            ),
            Verdict::Allow(
                json!({"project_id": "9xxlab/tools/cli", "file_path": "a.rs", "ref": "main"})
            ),
            "a plan has no commit to pin to"
        );
    }

    #[test]
    fn a_gitlab_planner_reads_work_items_of_its_own_project_only() {
        let scope = Scope::Plan {
            repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
            issue: 9,
        };
        for tool in ["get_work_item", "list_work_items"] {
            assert_eq!(
                guard(
                    Platform::GitLab,
                    tool,
                    &json!({"project_id": "1", "iid": 3}),
                    &scope
                ),
                Verdict::Allow(json!({"project_id": "9xxlab/tools/cli", "iid": 3})),
                "{tool}"
            );
            assert!(
                matches!(
                    guard(
                        Platform::GitLab,
                        tool,
                        &json!({"project_id": "other/project"}),
                        &scope
                    ),
                    Verdict::Deny(_)
                ),
                "{tool}"
            );
        }
        assert!(!is_exposed(Platform::GitLab, "update_work_item"));
        assert!(!is_exposed(Platform::GitLab, "create_work_item"));
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
        assert!(
            !is_exposed(Platform::GitLab, "search_repositories"),
            "a lane does not list other projects"
        );
    }
}
