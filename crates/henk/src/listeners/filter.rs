//! The rules every listener applies before anything else (§8.1, §8.7).

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::marker::Marker;
use henk_events::Sender;

use crate::config::Settings;

/// Henk's own login on a platform, as configured.
#[must_use]
pub fn own_login(settings: &Settings, platform: Platform) -> Option<&str> {
    match platform {
        Platform::GitHub => settings.github.as_ref().map(|g| g.bot_login.as_str()),
        Platform::GitLab => settings.gitlab.as_ref().map(|g| g.username.as_str()),
    }
}

/// Why a new-commits event for a commit Henk's address run pushed must not
/// be acted on, if it must not be: only the allowlist applies. Henk's own
/// account sent it, so the sender check would drop it, though the commit
/// is reviewed as usual (§3.5, #298); the commit, not the sender, says it
/// is his.
#[must_use]
pub fn rejected_ignoring_sender(settings: &Settings, repo: &RepoRef) -> Option<String> {
    (!settings.allowlist.allows(repo)).then(|| format!("{repo} is not on the allowlist"))
}

/// Why an event must not be acted on, if it must not be: it came from a bot
/// or from Henk himself, its text carries one of Henk's markers, or the
/// repository is outside the allowlist. `None` means proceed.
#[must_use]
pub fn rejected(
    settings: &Settings,
    repo: &RepoRef,
    sender: &Sender,
    body: Option<&str>,
) -> Option<String> {
    if sender.is_bot {
        return Some("sent by a bot".to_owned());
    }
    if own_login(settings, repo.platform())
        .is_some_and(|login| login.eq_ignore_ascii_case(&sender.login))
    {
        return Some("Henk's own words".to_owned());
    }
    if body.is_some_and(Marker::is_present) {
        return Some("carries one of Henk's markers".to_owned());
    }
    if !settings.allowlist.allows(repo) {
        return Some(format!("{repo} is not on the allowlist"));
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use henk_events::Sender;

    use super::*;
    use crate::config::Config;

    /// Henk's own account is dropped as a sender, unless the commit says
    /// the push was an address run's: then only the allowlist decides
    /// (#298). The same holds on GitLab, where Henk pushes as his user.
    #[test]
    fn only_the_allowlist_applies_to_henks_address_commit() {
        let settings = Config::parse(
            "[discord]\nchannel_id = 1\nhenk_user_id = 2\nteam_lead_ids = [3]\n[mail]\naddress = \"henk@example.com\"\n[allowlist]\ngithub_owners = [\"docspec\"]\n[mcp.github]\ncommand = \"unused\"\n[github]\napp_id = 1\ninstallation_id = 2\nbot_login = \"meneer-henk[bot]\"\n",
        )
        .and_then(Config::into_settings)
        .unwrap();
        let repo = RepoRef::parse(Platform::GitHub, "docspec/app").unwrap();
        let henk = Sender {
            login: "Meneer-Henk[bot]".into(),
            is_bot: false,
        };
        assert_eq!(
            rejected(&settings, &repo, &henk, None).as_deref(),
            Some("Henk's own words")
        );
        assert_eq!(rejected_ignoring_sender(&settings, &repo), None);
        let foreign = RepoRef::parse(Platform::GitHub, "evil/app").unwrap();
        assert!(
            rejected_ignoring_sender(&settings, &foreign).is_some_and(|r| r.contains("allowlist"))
        );
    }
}
