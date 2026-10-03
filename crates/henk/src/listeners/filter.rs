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
