//! Parsing pull request and issue URLs, on the hosts Henk is configured
//! for (#58).

use anyhow::{Context as _, anyhow};
use henk_domain::allowlist::{Platform, RepoRef};
use henk_platform::{IssueTarget, ReviewTarget};

use crate::config::Settings;

/// The web addresses of the configured platforms: a URL parses only on one
/// of them, with its scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosts {
    github: Option<Origin>,
    gitlab: Option<Origin>,
}

/// The scheme and host (with its port, lowercase) of a web address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: Scheme,
    host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Https,
    Http,
}

impl Scheme {
    const fn name(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
        }
    }
}

impl Origin {
    fn https(host: &str) -> Self {
        Self {
            scheme: Scheme::Https,
            host: host.to_ascii_lowercase(),
        }
    }

    /// The origin of a configured web address; one without a scheme is
    /// https.
    fn of(url: &str) -> Self {
        let (scheme, rest) = split_scheme(url).unwrap_or((Scheme::Https, url));
        Self {
            scheme,
            host: rest.split('/').next().unwrap_or(rest).to_ascii_lowercase(),
        }
    }
}

impl Hosts {
    /// These hosts, served over https; each may be absent.
    #[cfg(test)]
    #[must_use]
    pub fn new(github: Option<&str>, gitlab: Option<&str>) -> Self {
        Self {
            github: github.map(Origin::https),
            gitlab: gitlab.map(Origin::https),
        }
    }

    /// The web addresses the settings name, derived as Henk's own links
    /// and clone URLs are: GitHub's from `api_base` by
    /// [`henk_platform::github::web_base`] (`https://github.com` without a
    /// `[github]` table), GitLab's from `api_url` by
    /// [`henk_platform::gitlab::web_base`], none without a `[gitlab]`
    /// table. Each keeps the scheme its API address uses.
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        let github = settings.github.as_ref().map_or_else(
            || Origin::https("github.com"),
            |github| Origin::of(&henk_platform::github::web_base(&github.api_base)),
        );
        let gitlab = settings
            .gitlab
            .as_ref()
            .map(|gitlab| Origin::of(henk_platform::gitlab::web_base(&gitlab.api_url)));
        Self {
            github: Some(github),
            gitlab,
        }
    }

    /// The platform `host` belongs to, compared without regard to case,
    /// when `scheme` is the one that platform is configured with.
    fn platform(&self, scheme: Scheme, host: &str) -> anyhow::Result<Platform> {
        let host = host.to_ascii_lowercase();
        for (platform, origin) in [
            (Platform::GitHub, self.github.as_ref()),
            (Platform::GitLab, self.gitlab.as_ref()),
        ] {
            if let Some(origin) = origin.filter(|origin| origin.host == host) {
                if origin.scheme != scheme {
                    return Err(anyhow!("expected an {} URL", origin.scheme.name()));
                }
                return Ok(platform);
            }
        }
        let configured = |name: &str, origin: Option<&Origin>| match origin {
            Some(origin) => format!("the configured {name} host ({})", origin.host),
            None => format!("a {name} host: none is configured"),
        };
        Err(anyhow!(
            "{host} is neither {} nor {}",
            configured("GitHub", self.github.as_ref()),
            configured("GitLab", self.gitlab.as_ref())
        ))
    }
}

/// The scheme of an http or https URL, and the rest after `://`.
fn split_scheme(url: &str) -> Option<(Scheme, &str)> {
    url.strip_prefix("https://")
        .map(|rest| (Scheme::Https, rest))
        .or_else(|| url.strip_prefix("http://").map(|rest| (Scheme::Http, rest)))
}

fn split(url: &str) -> anyhow::Result<(Scheme, &str, &str)> {
    let (scheme, without_scheme) =
        split_scheme(url).ok_or_else(|| anyhow!("expected an https URL"))?;
    let (host, path) = without_scheme
        .split_once('/')
        .ok_or_else(|| anyhow!("URL has no path"))?;
    Ok((scheme, host, path.trim_end_matches('/')))
}

/// Parses `https://github.com/owner/repo/pull/7` or
/// `https://gitlab.example/group/sub/project/-/merge_requests/5`, on one of
/// `hosts`.
///
/// # Errors
///
/// A URL that is not http(s), is on another host or another scheme than
/// the one configured for it, or is not a pull or merge request.
pub fn parse_pull_request_url(url: &str, hosts: &Hosts) -> anyhow::Result<ReviewTarget> {
    let (scheme, host, path) = split(url)?;
    match hosts.platform(scheme, host)? {
        Platform::GitHub => {
            let mut parts = path.split('/');
            let (Some(owner), Some(repo), Some("pull"), Some(number)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(anyhow!(
                    "expected {}://{host}/owner/repo/pull/N",
                    scheme.name()
                ));
            };
            let number: u64 = number.parse().context("pull request number")?;
            Ok(ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, &format!("{owner}/{repo}"))?,
                number,
            })
        }
        Platform::GitLab => {
            let Some((project, number)) = path.split_once("/-/merge_requests/") else {
                return Err(anyhow!(
                    "expected {}://{host}/group/project/-/merge_requests/N",
                    scheme.name()
                ));
            };
            let number: u64 = number
                .split('/')
                .next()
                .unwrap_or("")
                .parse()
                .context("merge request iid")?;
            Ok(ReviewTarget {
                repo: RepoRef::parse(Platform::GitLab, project)?,
                number,
            })
        }
    }
}

/// The most pull/merge requests one command reviews (§3.1).
pub const MAX_REVIEW_TARGETS: usize = 50;

/// Parses the pull/merge request URLs of one `henk review`: 1 to
/// [`MAX_REVIEW_TARGETS`], every one valid on one of `hosts`, each target
/// once (first seen first). `--commit` names one commit, so it goes with
/// one target only.
///
/// # Errors
///
/// Too few or too many URLs, any URL that does not parse (all of them
/// named), or a commit with more than one target.
pub fn parse_review_targets(
    urls: &[String],
    commit: Option<&str>,
    hosts: &Hosts,
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
        match parse_pull_request_url(url, hosts) {
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
/// `https://gitlab.example/group/project/-/issues/9`, on one of `hosts`.
///
/// # Errors
///
/// A URL that is not http(s), is on another host or another scheme than
/// the one configured for it, or is not an issue.
pub fn parse_issue_url(url: &str, hosts: &Hosts) -> anyhow::Result<IssueTarget> {
    let (scheme, host, path) = split(url)?;
    match hosts.platform(scheme, host)? {
        Platform::GitHub => {
            let mut parts = path.split('/');
            let (Some(owner), Some(repo), Some("issues"), Some(number)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(anyhow!(
                    "expected {}://{host}/owner/repo/issues/N",
                    scheme.name()
                ));
            };
            let number: u64 = number.parse().context("issue number")?;
            Ok(IssueTarget {
                repo: RepoRef::parse(Platform::GitHub, &format!("{owner}/{repo}"))?,
                number,
            })
        }
        Platform::GitLab => {
            let Some((project, number)) = path.split_once("/-/issues/") else {
                return Err(anyhow!(
                    "expected {}://{host}/group/project/-/issues/N",
                    scheme.name()
                ));
            };
            let number: u64 = number
                .split('/')
                .next()
                .unwrap_or("")
                .parse()
                .context("issue iid")?;
            Ok(IssueTarget {
                repo: RepoRef::parse(Platform::GitLab, project)?,
                number,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// github.com and gitlab.com, as most deployments have them.
    fn hosts() -> Hosts {
        Hosts::new(Some("github.com"), Some("gitlab.com"))
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|u| (*u).to_owned()).collect()
    }

    #[test]
    fn review_targets_are_counted_checked_and_deduplicated() {
        assert!(parse_review_targets(&[], None, &hosts()).is_err());
        let many: Vec<String> = (1..=51)
            .map(|n| format!("https://github.com/o/r/pull/{n}"))
            .collect();
        let error = parse_review_targets(&many, None, &hosts())
            .unwrap_err()
            .to_string();
        assert!(error.contains("at most 50"), "{error}");

        let error = parse_review_targets(
            &urls(&[
                "https://github.com/o/r/pull/1",
                "https://github.com/o/r/issues/2",
                "not a url",
            ]),
            None,
            &hosts(),
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
            &hosts(),
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
        assert!(parse_review_targets(&two, Some("abc"), &hosts()).is_err());
        assert!(parse_review_targets(two.get(..1).unwrap(), Some("abc"), &hosts()).is_ok());
    }

    #[test]
    fn pull_request_urls_are_parsed() {
        let target = parse_pull_request_url(
            "https://github.com/StephanMeijer/scratch-repo/pull/12",
            &hosts(),
        )
        .unwrap();
        assert_eq!(target.repo.path(), "StephanMeijer/scratch-repo");
        assert_eq!(target.number, 12);
        assert!(parse_pull_request_url("https://github.com/a/b/issues/1", &hosts()).is_err());
        let mr = parse_pull_request_url(
            "https://gitlab.com/9xxlab/tools/cli/-/merge_requests/5",
            &hosts(),
        )
        .unwrap();
        assert_eq!(mr.repo.path(), "9xxlab/tools/cli");
        assert_eq!(mr.repo.platform(), Platform::GitLab);
        assert_eq!(mr.number, 5);
        assert!(parse_pull_request_url("https://gitlab.com/a/b/-/issues/1", &hosts()).is_err());
    }

    #[test]
    fn only_the_configured_hosts_parse_compared_without_case() {
        let hosts = Hosts::new(Some("github.com"), Some("gitlab.example"));
        let target = parse_pull_request_url("https://GitHub.COM/o/r/pull/7", &hosts).unwrap();
        assert_eq!(target.repo.platform(), Platform::GitHub);
        let mr = parse_pull_request_url("https://gitlab.example/g/p/-/merge_requests/2", &hosts)
            .unwrap();
        assert_eq!(mr.repo.platform(), Platform::GitLab);

        let foreign = "https://evil.example/a/b/-/merge_requests/1";
        let error = parse_pull_request_url(foreign, &hosts)
            .unwrap_err()
            .to_string();
        assert!(error.contains("evil.example is neither"), "{error}");
        assert!(error.contains("(gitlab.example)"), "{error}");
        let error = parse_issue_url("https://gitlab.com/g/p/-/issues/1", &hosts)
            .unwrap_err()
            .to_string();
        assert!(error.contains("gitlab.com is neither"), "{error}");

        let no_gitlab = Hosts::new(Some("github.com"), None);
        let error =
            parse_pull_request_url("https://gitlab.example/g/p/-/merge_requests/2", &no_gitlab)
                .unwrap_err()
                .to_string();
        assert!(error.contains("none is configured"), "{error}");

        let error = parse_pull_request_url("http://github.com/o/r/pull/7", &hosts)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "expected an https URL");

        let error = parse_review_targets(
            &urls(&["https://github.com/o/r/pull/1", foreign]),
            None,
            &hosts,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(foreign) && error.contains("evil.example is neither"),
            "{error}"
        );
    }

    #[test]
    fn hosts_come_from_the_configured_api_bases() {
        let base = "[discord]\nchannel_id = 1\nhenk_user_id = 2\nteam_lead_ids = [3]\n[mail]\naddress = \"henk@example.com\"\n[allowlist]\ngithub_owners = [\"o\"]\n";
        let settings = |extra: &str| {
            crate::config::Config::parse(&format!("{base}{extra}"))
                .and_then(crate::config::Config::into_settings)
                .unwrap_or_else(|e| panic!("{e}"))
        };
        assert_eq!(
            Hosts::from_settings(&settings("")),
            Hosts::new(Some("github.com"), None),
            "github.com without a [github] table, no GitLab"
        );
        let github = "[mcp.github]\ncommand = \"unused\"\n[github]\napp_id = 1\ninstallation_id = 2\nbot_login = \"h[bot]\"\n";
        assert_eq!(
            Hosts::from_settings(&settings(github)),
            Hosts::new(Some("github.com"), None),
            "api.github.com is github.com"
        );
        let enterprise = format!("{github}api_base = \"https://GHE.acme.example/api/v3\"\n");
        let hosts = Hosts::from_settings(&settings(&enterprise));
        let target = parse_pull_request_url("https://ghe.acme.example/o/r/pull/7", &hosts).unwrap();
        assert_eq!(target.repo.platform(), Platform::GitHub);
        assert!(parse_pull_request_url("https://github.com/o/r/pull/7", &hosts).is_err());
        let gitlab = "[mcp.gitlab]\ncommand = \"unused\"\n[mcp.gitlab-write]\ncommand = \"unused\"\n[gitlab]\napi_url = \"https://gitlab.example/api/v4\"\nusername = \"henk\"\n";
        assert_eq!(
            Hosts::from_settings(&settings(gitlab)),
            Hosts::new(Some("github.com"), Some("gitlab.example"))
        );
    }

    /// Settings from the minimal config plus `extra`.
    fn settings(extra: &str) -> Settings {
        let base = "[discord]\nchannel_id = 1\nhenk_user_id = 2\nteam_lead_ids = [3]\n[mail]\naddress = \"henk@example.com\"\n[allowlist]\ngithub_owners = [\"o\"]\n";
        crate::config::Config::parse(&format!("{base}{extra}"))
            .and_then(crate::config::Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    const GITHUB: &str = "[mcp.github]\ncommand = \"unused\"\n[github]\napp_id = 1\ninstallation_id = 2\nbot_login = \"h[bot]\"\n";

    #[test]
    fn an_enterprise_api_host_is_the_web_host_as_henks_links_have_it() {
        let enterprise = format!("{GITHUB}api_base = \"https://api.ghe.corp.com/api/v3\"\n");
        let settings = settings(&enterprise);
        let hosts = Hosts::from_settings(&settings);
        let link = "https://api.ghe.corp.com/o/r/pull/7";
        assert_eq!(
            format!(
                "{}/o/r/pull/7",
                henk_platform::github::web_base("https://api.ghe.corp.com/api/v3")
            ),
            link,
            "the dashboard link and clone URL keep the api. host"
        );
        let target = parse_pull_request_url(link, &hosts).unwrap();
        assert_eq!(target.repo.platform(), Platform::GitHub);
        assert_eq!(target.number, 7);
        assert!(parse_pull_request_url("https://ghe.corp.com/o/r/pull/7", &hosts).is_err());

        let public = Hosts::from_settings(&settings_with_api_base("https://api.github.com/"));
        assert!(parse_pull_request_url("https://github.com/o/r/pull/7", &public).is_ok());
        assert!(parse_pull_request_url("https://api.github.com/o/r/pull/7", &public).is_err());
    }

    fn settings_with_api_base(api_base: &str) -> Settings {
        settings(&format!("{GITHUB}api_base = \"{api_base}\"\n"))
    }

    #[test]
    fn a_gitlab_on_http_takes_the_http_urls_henk_links_to() {
        let gitlab = "[mcp.gitlab]\ncommand = \"unused\"\n[mcp.gitlab-write]\ncommand = \"unused\"\n[gitlab]\napi_url = \"http://GitLab.internal/api/v4/\"\nusername = \"henk\"\n";
        let settings = settings(gitlab);
        let hosts = Hosts::from_settings(&settings);
        for issue in [false, true] {
            let what = if issue { "issues" } else { "merge_requests" };
            let web = henk_platform::gitlab::web_base(&settings.gitlab.as_ref().unwrap().api_url);
            let link = format!("{web}/g/p/-/{what}/1");
            assert!(link.starts_with("http://GitLab.internal/g/p/-/"), "{link}");
            let platform = if issue {
                parse_issue_url(&link, &hosts).unwrap().repo.platform()
            } else {
                parse_pull_request_url(&link, &hosts)
                    .unwrap()
                    .repo
                    .platform()
            };
            assert_eq!(platform, Platform::GitLab);
        }
        let error =
            parse_pull_request_url("https://gitlab.internal/g/p/-/merge_requests/1", &hosts)
                .unwrap_err()
                .to_string();
        assert_eq!(error, "expected an http URL", "the configured scheme only");
        let error = parse_pull_request_url("http://github.com/o/r/pull/7", &hosts)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "expected an https URL", "GitHub stays https");
    }

    #[test]
    fn issue_urls_are_parsed() {
        let issue = parse_issue_url("https://github.com/docspec/app/issues/9", &hosts()).unwrap();
        assert_eq!(issue.repo.path(), "docspec/app");
        assert_eq!(issue.number, 9);
        let gl = parse_issue_url("https://gitlab.com/9xxlab/app/-/issues/3", &hosts()).unwrap();
        assert_eq!(gl.repo.platform(), Platform::GitLab);
        assert_eq!(gl.number, 3);
        assert!(parse_issue_url("https://github.com/docspec/app/pull/9", &hosts()).is_err());
    }
}
