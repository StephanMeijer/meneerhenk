//! Parsing pull request and issue URLs.

use anyhow::{Context as _, anyhow};
use henk_domain::allowlist::{Platform, RepoRef};
use henk_platform::{IssueTarget, ReviewTarget};

fn split(url: &str) -> anyhow::Result<(&str, &str)> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("expected an https URL"))?;
    let (host, path) = without_scheme
        .split_once('/')
        .ok_or_else(|| anyhow!("URL has no path"))?;
    Ok((host, path.trim_end_matches('/')))
}

/// Parses `https://github.com/owner/repo/pull/7` or
/// `https://gitlab.example/group/sub/project/-/merge_requests/5`.
pub fn parse_pull_request_url(url: &str) -> anyhow::Result<ReviewTarget> {
    let (host, path) = split(url)?;
    if host == "github.com" {
        let mut parts = path.split('/');
        let (Some(owner), Some(repo), Some("pull"), Some(number)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(anyhow!("expected https://github.com/owner/repo/pull/N"));
        };
        let number: u64 = number.parse().context("pull request number")?;
        return Ok(ReviewTarget {
            repo: RepoRef::parse(Platform::GitHub, &format!("{owner}/{repo}"))?,
            number,
        });
    }
    if let Some((project, number)) = path.split_once("/-/merge_requests/") {
        let number: u64 = number
            .split('/')
            .next()
            .unwrap_or("")
            .parse()
            .context("merge request iid")?;
        return Ok(ReviewTarget {
            repo: RepoRef::parse(Platform::GitLab, project)?,
            number,
        });
    }
    Err(anyhow!(
        "expected a GitHub pull request URL or a GitLab merge request URL"
    ))
}

/// The most pull/merge requests one command reviews (§3.1).
pub const MAX_REVIEW_TARGETS: usize = 50;

/// Parses the pull/merge request URLs of one `henk review`: 1 to
/// [`MAX_REVIEW_TARGETS`], every one valid, each target once (first seen
/// first). `--commit` names one commit, so it goes with one target only.
///
/// # Errors
///
/// Too few or too many URLs, any URL that does not parse (all of them
/// named), or a commit with more than one target.
pub fn parse_review_targets(
    urls: &[String],
    commit: Option<&str>,
) -> anyhow::Result<Vec<(String, ReviewTarget)>> {
    if urls.is_empty() {
        return Err(anyhow!("give at least one pull or merge request URL"));
    }
    if urls.len() > MAX_REVIEW_TARGETS {
        return Err(anyhow!(
            "at most {MAX_REVIEW_TARGETS} pull or merge requests at once, not {}",
            urls.len()
        ));
    }
    let mut targets: Vec<(String, ReviewTarget)> = Vec::new();
    let mut bad = Vec::new();
    for url in urls {
        match parse_pull_request_url(url) {
            Ok(target) => {
                if !targets.iter().any(|(_, known)| *known == target) {
                    targets.push((url.clone(), target));
                }
            }
            Err(error) => bad.push(format!("{url}: {error:#}")),
        }
    }
    if !bad.is_empty() {
        return Err(anyhow!("cannot read {}", bad.join("; ")));
    }
    if commit.is_some() && targets.len() > 1 {
        return Err(anyhow!("--commit applies to one pull request"));
    }
    Ok(targets)
}

/// Parses `https://github.com/owner/repo/issues/9` or
/// `https://gitlab.example/group/project/-/issues/9`.
pub fn parse_issue_url(url: &str) -> anyhow::Result<IssueTarget> {
    let (host, path) = split(url)?;
    if host == "github.com" {
        let mut parts = path.split('/');
        let (Some(owner), Some(repo), Some("issues"), Some(number)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(anyhow!("expected https://github.com/owner/repo/issues/N"));
        };
        let number: u64 = number.parse().context("issue number")?;
        return Ok(IssueTarget {
            repo: RepoRef::parse(Platform::GitHub, &format!("{owner}/{repo}"))?,
            number,
        });
    }
    if let Some((project, number)) = path.split_once("/-/issues/") {
        let number: u64 = number
            .split('/')
            .next()
            .unwrap_or("")
            .parse()
            .context("issue iid")?;
        return Ok(IssueTarget {
            repo: RepoRef::parse(Platform::GitLab, project)?,
            number,
        });
    }
    Err(anyhow!("expected a GitHub or GitLab issue URL"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|u| (*u).to_owned()).collect()
    }

    #[test]
    fn review_targets_are_counted_checked_and_deduplicated() {
        assert!(parse_review_targets(&[], None).is_err());
        let many: Vec<String> = (1..=51)
            .map(|n| format!("https://github.com/o/r/pull/{n}"))
            .collect();
        let error = parse_review_targets(&many, None).unwrap_err().to_string();
        assert!(error.contains("at most 50"), "{error}");

        let error = parse_review_targets(
            &urls(&[
                "https://github.com/o/r/pull/1",
                "https://github.com/o/r/issues/2",
                "not a url",
            ]),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("https://github.com/o/r/issues/2"), "{error}");
        assert!(
            error.contains("not a url"),
            "every bad URL is named: {error}"
        );

        let targets = parse_review_targets(
            &urls(&[
                "https://github.com/o/r/pull/7",
                "https://github.com/o/r/pull/8",
                "https://github.com/o/r/pull/7/files",
            ]),
            None,
        )
        .unwrap();
        let numbers: Vec<u64> = targets.iter().map(|(_, t)| t.number).collect();
        assert_eq!(
            numbers,
            vec![7, 8],
            "the same pull request once, first seen first"
        );
    }

    #[test]
    fn a_commit_goes_with_one_target_only() {
        let two = urls(&[
            "https://github.com/o/r/pull/1",
            "https://github.com/o/r/pull/2",
        ]);
        assert!(parse_review_targets(&two, Some("abc")).is_err());
        assert!(parse_review_targets(two.get(..1).unwrap(), Some("abc")).is_ok());
    }

    #[test]
    fn pull_request_urls_are_parsed() {
        let target =
            parse_pull_request_url("https://github.com/StephanMeijer/scratch-repo/pull/12")
                .unwrap();
        assert_eq!(target.repo.path(), "StephanMeijer/scratch-repo");
        assert_eq!(target.number, 12);
        assert!(parse_pull_request_url("https://github.com/a/b/issues/1").is_err());
        let mr = parse_pull_request_url("https://gitlab.com/9xxlab/tools/cli/-/merge_requests/5")
            .unwrap();
        assert_eq!(mr.repo.path(), "9xxlab/tools/cli");
        assert_eq!(mr.repo.platform(), Platform::GitLab);
        assert_eq!(mr.number, 5);
        assert!(parse_pull_request_url("https://gitlab.com/a/b/-/issues/1").is_err());
    }

    #[test]
    fn issue_urls_are_parsed() {
        let issue = parse_issue_url("https://github.com/docspec/app/issues/9").unwrap();
        assert_eq!(issue.repo.path(), "docspec/app");
        assert_eq!(issue.number, 9);
        let gl = parse_issue_url("https://gitlab.com/9xxlab/app/-/issues/3").unwrap();
        assert_eq!(gl.repo.platform(), Platform::GitLab);
        assert_eq!(gl.number, 3);
        assert!(parse_issue_url("https://github.com/docspec/app/pull/9").is_err());
    }
}
