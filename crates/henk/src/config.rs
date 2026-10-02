//! Configuration: the roster, the allowlist and Henk's own identities.
//!
//! No credential lives here (§8.4). Tokens are read from the environment by
//! the tools that need them, never by the model.

use std::fmt::Write as _;

use serde::Deserialize;

use henk_domain::allowlist::{AllowRule, Allowlist, Platform, RepoRef};
use henk_domain::discord::HenkIdentity;
use henk_domain::identity::{DiscordChannelId, DiscordRoleId, DiscordUserId, People, Person};
use henk_domain::mail::EmailAddress;

/// An example configuration, printed by `henk config example`.
pub const EXAMPLE: &str = include_str!("../../../henk.example.toml");

/// The configuration file as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Discord ids.
    pub discord: DiscordConfig,
    /// Henk's mailbox.
    pub mail: MailConfig,
    /// Where Henk works.
    pub allowlist: AllowlistConfig,
    /// People Henk knows by id.
    #[serde(default)]
    pub people: Vec<PersonConfig>,
}

/// Discord ids.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    /// The one channel Henk works in.
    pub channel_id: DiscordChannelId,
    /// Henk's own user id.
    pub henk_user_id: DiscordUserId,
    /// Henk's role id, if he has one.
    pub henk_role_id: Option<DiscordRoleId>,
    /// The "Collega van Henk" role id.
    pub colleague_role_id: Option<DiscordRoleId>,
    /// Team Lead user ids.
    #[serde(default)]
    pub team_lead_ids: Vec<DiscordUserId>,
}

/// Henk's mailbox.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    /// The address Henk sends from and never replies to.
    pub address: EmailAddress,
}

/// Where Henk works.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AllowlistConfig {
    /// Single GitHub repositories, as `owner/name`.
    #[serde(default)]
    pub github_repositories: Vec<String>,
    /// GitHub users or organisations whose every repository is allowed.
    #[serde(default)]
    pub github_owners: Vec<String>,
    /// Single GitLab projects, as `group/subgroup/name`.
    #[serde(default)]
    pub gitlab_projects: Vec<String>,
    /// GitLab groups whose every project, including subgroups, is allowed.
    #[serde(default)]
    pub gitlab_groups: Vec<String>,
}

/// A known person.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonConfig {
    /// Discord user id.
    pub discord_id: DiscordUserId,
    /// How Henk addresses them.
    pub name: String,
    /// What they do on the team.
    pub role: String,
}

/// The configuration, validated and turned into domain values.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Henk on Discord.
    pub henk: HenkIdentity,
    /// Henk's mail address.
    pub mail_address: EmailAddress,
    /// The roster.
    pub people: People,
    /// Where Henk works.
    pub allowlist: Allowlist,
}

/// Why a configuration is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The TOML did not parse or had the wrong shape.
    #[error("invalid configuration: {0}")]
    Syntax(#[from] toml::de::Error),
    /// An allowlist entry is not a repository path.
    #[error("allowlist entry: {0}")]
    Allowlist(#[from] henk_domain::allowlist::RepoRefError),
    /// The allowlist is empty, so Henk would work nowhere.
    #[error("the allowlist is empty; Henk would work nowhere")]
    EmptyAllowlist,
    /// Henk is listed as his own Team Lead or colleague.
    #[error("Henk's own user id {0} appears in the roster")]
    HenkInRoster(DiscordUserId),
}

impl Config {
    /// Parses the TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Syntax`] when the text is not valid or has unknown fields.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Validates the configuration and builds domain values from it.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] for an unparsable allowlist entry, an empty
    /// allowlist, or a roster that lists Henk himself.
    pub fn into_settings(self) -> Result<Settings, ConfigError> {
        let henk_id = self.discord.henk_user_id;
        if self.discord.team_lead_ids.contains(&henk_id)
            || self
                .people
                .iter()
                .any(|person| person.discord_id == henk_id)
        {
            return Err(ConfigError::HenkInRoster(henk_id));
        }

        let mut rules = Vec::new();
        for path in &self.allowlist.github_repositories {
            rules.push(AllowRule::Repository(RepoRef::parse(
                Platform::GitHub,
                path,
            )?));
        }
        for owner in &self.allowlist.github_owners {
            rules.push(AllowRule::Owner {
                platform: Platform::GitHub,
                owner: owner.trim().to_owned(),
            });
        }
        for path in &self.allowlist.gitlab_projects {
            rules.push(AllowRule::Repository(RepoRef::parse(
                Platform::GitLab,
                path,
            )?));
        }
        for group in &self.allowlist.gitlab_groups {
            rules.push(AllowRule::Owner {
                platform: Platform::GitLab,
                owner: group.trim().trim_matches('/').to_owned(),
            });
        }
        let allowlist = Allowlist::new(rules);
        if allowlist.is_empty() {
            return Err(ConfigError::EmptyAllowlist);
        }

        let people = People::new(
            self.discord.team_lead_ids.iter().copied(),
            self.discord.colleague_role_id,
            self.people.into_iter().map(|person| {
                (
                    person.discord_id,
                    Person {
                        name: person.name,
                        role: person.role,
                    },
                )
            }),
        );

        Ok(Settings {
            henk: HenkIdentity {
                user: henk_id,
                role: self.discord.henk_role_id,
                channel: self.discord.channel_id,
            },
            mail_address: self.mail.address,
            people,
            allowlist,
        })
    }
}

impl Settings {
    /// A plain-text description of what Henk would work with.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Discord channel: {}", self.henk.channel);
        let _ = writeln!(out, "Henk's user id:  {}", self.henk.user);
        let _ = writeln!(out, "Mail address:    {}", self.mail_address);
        let leads: Vec<String> = self.people.team_leads().map(|id| id.to_string()).collect();
        let _ = writeln!(
            out,
            "Team leads:      {}",
            if leads.is_empty() {
                "none".to_owned()
            } else {
                leads.join(", ")
            }
        );
        let _ = writeln!(out, "Allowlist:");
        for rule in self.allowlist.rules() {
            match rule {
                AllowRule::Repository(repo) => {
                    let _ = writeln!(out, "  {} repository {}", repo.platform(), repo.path());
                }
                AllowRule::Owner { platform, owner } => {
                    let _ = writeln!(out, "  {platform} everything under {owner}");
                }
            }
        }
        out.trim_end().to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn example_config_is_valid() {
        let settings = Config::parse(EXAMPLE)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(settings.allowlist.rules().len(), 4);
        assert!(settings.people.team_leads().next().is_some());
        let scratch = RepoRef::parse(Platform::GitHub, "StephanMeijer/scratch-repo")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(settings.allowlist.allows(&scratch));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let text = format!("{EXAMPLE}\n[surprise]\nkey = 1\n");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Syntax(_))));
    }

    #[test]
    fn empty_allowlist_is_rejected() {
        let text = r#"
[discord]
channel_id = 1
henk_user_id = 2
[mail]
address = "henk@example.com"
[allowlist]
"#;
        let result = Config::parse(text).and_then(Config::into_settings);
        assert!(matches!(result, Err(ConfigError::EmptyAllowlist)));
    }

    #[test]
    fn henk_cannot_be_his_own_team_lead() {
        let text = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [2]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
"#;
        let result = Config::parse(text).and_then(Config::into_settings);
        assert!(matches!(result, Err(ConfigError::HenkInRoster(id)) if id.get() == 2));
    }
}
