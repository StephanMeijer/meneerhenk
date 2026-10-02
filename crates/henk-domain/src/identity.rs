//! People, standing and permissions (§2).
//!
//! Identity is always a stable id: a Discord user id, a Discord role id or a
//! platform account id. Never a display name.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// A Discord user id (snowflake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiscordUserId(u64);

impl DiscordUserId {
    /// Wraps a raw snowflake.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw snowflake.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DiscordUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A Discord role id (snowflake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiscordRoleId(u64);

impl DiscordRoleId {
    /// Wraps a raw snowflake.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw snowflake.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DiscordRoleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A Discord channel id (snowflake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiscordChannelId(u64);

impl DiscordChannelId {
    /// Wraps a raw snowflake.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The raw snowflake.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DiscordChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a Discord user may have Henk do (§2, §5.3).
///
/// Standing is cumulative: a Team Lead may do everything a Colleague may,
/// and a Colleague everything anyone may. The spec's table reads
/// "Additionally" at each step; this type takes that literally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Standing {
    /// Any user in Henk's channel.
    Anyone,
    /// Holder of the "Collega van Henk" role.
    Colleague,
    /// Listed by user id. Their decisions are final; `sudo!` is an order (§5.4).
    TeamLead,
}

impl Standing {
    /// Whether this standing permits `action`.
    #[must_use]
    pub fn allows(self, action: Action) -> bool {
        self >= action.required_standing()
    }
}

/// Something Henk can be asked to do from Discord (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// List and read pull/merge requests and their diffs.
    ReadPullRequests,
    /// Read issues.
    ReadIssues,
    /// Search the web and read web pages.
    ReadWeb,
    /// Read the channel history.
    ReadChannelHistory,
    /// Start a code review (§3).
    StartReview,
    /// Start a plan for an issue (§4).
    StartPlan,
    /// Create, comment on, retitle, re-describe, close, reopen and label issues.
    ManageIssues,
    /// Read, write and send mail; notes, contacts and calendar.
    UseMailbox,
    /// Give Henk an order he carries out without objection (§5.4).
    Sudo,
}

impl Action {
    /// The lowest standing that permits this action.
    #[must_use]
    pub const fn required_standing(self) -> Standing {
        match self {
            Self::ReadPullRequests
            | Self::ReadIssues
            | Self::ReadWeb
            | Self::ReadChannelHistory => Standing::Anyone,
            Self::StartReview | Self::StartPlan | Self::ManageIssues | Self::UseMailbox => {
                Standing::Colleague
            }
            Self::Sudo => Standing::TeamLead,
        }
    }
}

/// A person Henk knows by id (§2, "Known people").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// How Henk addresses them.
    pub name: String,
    /// What they do on the team, in Henk's words.
    pub role: String,
}

/// The roster: who is a Team Lead, which role makes a Colleague, and who is known.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct People {
    team_leads: BTreeSet<DiscordUserId>,
    colleague_role: Option<DiscordRoleId>,
    known: BTreeMap<DiscordUserId, Person>,
}

impl People {
    /// Builds a roster. `colleague_role` is the id of "Collega van Henk".
    #[must_use]
    pub fn new(
        team_leads: impl IntoIterator<Item = DiscordUserId>,
        colleague_role: Option<DiscordRoleId>,
        known: impl IntoIterator<Item = (DiscordUserId, Person)>,
    ) -> Self {
        Self {
            team_leads: team_leads.into_iter().collect(),
            colleague_role,
            known: known.into_iter().collect(),
        }
    }

    /// Whether `user` is a Team Lead.
    #[must_use]
    pub fn is_team_lead(&self, user: DiscordUserId) -> bool {
        self.team_leads.contains(&user)
    }

    /// The Team Leads, in id order.
    pub fn team_leads(&self) -> impl Iterator<Item = DiscordUserId> + '_ {
        self.team_leads.iter().copied()
    }

    /// The standing of `user`, given the roles Discord reports for them.
    #[must_use]
    pub fn standing_of(&self, user: DiscordUserId, roles: &[DiscordRoleId]) -> Standing {
        if self.is_team_lead(user) {
            Standing::TeamLead
        } else if self
            .colleague_role
            .is_some_and(|wanted| roles.contains(&wanted))
        {
            Standing::Colleague
        } else {
            Standing::Anyone
        }
    }

    /// What Henk knows about `user`, if anything.
    #[must_use]
    pub fn person(&self, user: DiscordUserId) -> Option<&Person> {
        self.known.get(&user)
    }
}

/// The person whose request started a run (§1.1).
///
/// Their standing bounds the run (§8.3) and is resolved once, for that one
/// request. Nothing carries over (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Requester {
    /// Who asked.
    pub user: DiscordUserId,
    /// Their standing at the moment they asked.
    pub standing: Standing,
}

impl Requester {
    /// Resolves a requester from the roster and the roles Discord reports.
    #[must_use]
    pub fn resolve(people: &People, user: DiscordUserId, roles: &[DiscordRoleId]) -> Self {
        Self {
            user,
            standing: people.standing_of(user, roles),
        }
    }

    /// Whether this requester may have Henk perform `action`.
    #[must_use]
    pub fn may(&self, action: Action) -> bool {
        self.standing.allows(action)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const LEAD: DiscordUserId = DiscordUserId::new(1);
    const COLLEAGUE: DiscordUserId = DiscordUserId::new(2);
    const STRANGER: DiscordUserId = DiscordUserId::new(3);
    const ROLE: DiscordRoleId = DiscordRoleId::new(100);
    const OTHER_ROLE: DiscordRoleId = DiscordRoleId::new(101);

    fn roster() -> People {
        People::new([LEAD], Some(ROLE), [])
    }

    #[test]
    fn team_lead_is_recognised_by_id_regardless_of_roles() {
        assert_eq!(roster().standing_of(LEAD, &[]), Standing::TeamLead);
    }

    #[test]
    fn colleague_is_recognised_by_role_id() {
        let people = roster();
        assert_eq!(
            people.standing_of(COLLEAGUE, &[OTHER_ROLE, ROLE]),
            Standing::Colleague
        );
        assert_eq!(
            people.standing_of(COLLEAGUE, &[OTHER_ROLE]),
            Standing::Anyone
        );
    }

    #[test]
    fn without_a_colleague_role_nobody_is_a_colleague() {
        let people = People::new([LEAD], None, []);
        assert_eq!(people.standing_of(COLLEAGUE, &[ROLE]), Standing::Anyone);
    }

    #[test]
    fn standing_is_cumulative() {
        for action in [
            Action::ReadPullRequests,
            Action::ReadIssues,
            Action::ReadWeb,
            Action::ReadChannelHistory,
        ] {
            assert!(Standing::Anyone.allows(action), "{action:?}");
        }
        for action in [
            Action::StartReview,
            Action::StartPlan,
            Action::ManageIssues,
            Action::UseMailbox,
        ] {
            assert!(!Standing::Anyone.allows(action), "{action:?}");
            assert!(Standing::Colleague.allows(action), "{action:?}");
            assert!(Standing::TeamLead.allows(action), "{action:?}");
        }
        assert!(!Standing::Colleague.allows(Action::Sudo));
        assert!(Standing::TeamLead.allows(Action::Sudo));
    }

    #[test]
    fn requester_resolves_for_one_request() {
        let people = roster();
        let requester = Requester::resolve(&people, STRANGER, &[]);
        assert!(requester.may(Action::ReadIssues));
        assert!(!requester.may(Action::StartReview));
    }
}
