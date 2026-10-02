//! Discord (§5): when Henk speaks, and what `sudo!` means.
//!
//! The hard rules live here. The judgement of whether to join in uninvited
//! (§5.1) is the model's, and this module only says when that judgement is
//! even asked for.

use crate::identity::{DiscordChannelId, DiscordRoleId, DiscordUserId, People};

/// Henk's own Discord identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HenkIdentity {
    /// Henk's own user id. He never reacts to himself (§5.1, §8.1).
    pub user: DiscordUserId,
    /// Henk's role, if he has one; mentioning it mentions him.
    pub role: Option<DiscordRoleId>,
    /// The one channel he works in (§5.1, §8.7).
    pub channel: DiscordChannelId,
}

/// The parts of a Discord message that decide whether Henk reacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    /// Where it was posted.
    pub channel: DiscordChannelId,
    /// Who posted it.
    pub author: DiscordUserId,
    /// Whether the author is a bot account (including Henk himself).
    pub author_is_bot: bool,
    /// The text, after Discord's markup for mentions.
    pub content: String,
    /// Users mentioned explicitly.
    pub mentioned_users: Vec<DiscordUserId>,
    /// Roles mentioned explicitly.
    pub mentioned_roles: Vec<DiscordRoleId>,
    /// The author of the message this one replies to, if it is a reply.
    pub reply_to_author: Option<DiscordUserId>,
}

/// Why Henk never reacts to a message (§5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Posted outside Henk's channel.
    OtherChannel,
    /// Posted by a bot, Henk included.
    Bot,
    /// The message has no text.
    NoText,
    /// The message mentions someone else and not him.
    ForSomeoneElse,
}

/// What a message asks of Henk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Never react.
    Ignore(IgnoreReason),
    /// A mention: by name, by role or by reply. He always answers (§5.1, §8.8).
    Mention,
    /// Not a mention. Henk judges whether to join in, and staying out wins.
    Judge,
}

/// Decides, by the hard rules of §5.1, what a message asks of Henk.
#[must_use]
pub fn attention(henk: &HenkIdentity, message: &IncomingMessage) -> Attention {
    if message.channel != henk.channel {
        return Attention::Ignore(IgnoreReason::OtherChannel);
    }
    if message.author_is_bot || message.author == henk.user {
        return Attention::Ignore(IgnoreReason::Bot);
    }
    if message.content.trim().is_empty() {
        return Attention::Ignore(IgnoreReason::NoText);
    }

    let mentions_user = message.mentioned_users.contains(&henk.user);
    let mentions_role = henk
        .role
        .is_some_and(|role| message.mentioned_roles.contains(&role));
    let replies_to_henk = message.reply_to_author == Some(henk.user);
    if mentions_user || mentions_role || replies_to_henk || names_henk(&message.content) {
        return Attention::Mention;
    }

    let mentions_others = message
        .mentioned_users
        .iter()
        .any(|user| *user != henk.user)
        || !message.mentioned_roles.is_empty();
    if mentions_others {
        return Attention::Ignore(IgnoreReason::ForSomeoneElse);
    }

    Attention::Judge
}

/// Whether the text names Henk: the word "henk", in any case, on its own.
#[must_use]
pub fn names_henk(content: &str) -> bool {
    content
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("henk"))
}

/// Whether a message is a `sudo!` order (§5.4): it contains `sudo!` and the
/// author is a Team Lead by user id. From anyone else, `sudo!` means nothing.
#[must_use]
pub fn is_sudo_order(people: &People, message: &IncomingMessage) -> bool {
    people.is_team_lead(message.author) && message.content.contains("sudo!")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const HENK: DiscordUserId = DiscordUserId::new(10);
    const HENK_ROLE: DiscordRoleId = DiscordRoleId::new(11);
    const CHANNEL: DiscordChannelId = DiscordChannelId::new(12);
    const LEAD: DiscordUserId = DiscordUserId::new(1);
    const ALICE: DiscordUserId = DiscordUserId::new(2);
    const BOB: DiscordUserId = DiscordUserId::new(3);

    fn henk() -> HenkIdentity {
        HenkIdentity {
            user: HENK,
            role: Some(HENK_ROLE),
            channel: CHANNEL,
        }
    }

    fn message(content: &str) -> IncomingMessage {
        IncomingMessage {
            channel: CHANNEL,
            author: ALICE,
            author_is_bot: false,
            content: content.to_owned(),
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            reply_to_author: None,
        }
    }

    #[test]
    fn ignores_other_channels_bots_and_empty_messages() {
        let mut other = message("hello");
        other.channel = DiscordChannelId::new(99);
        assert_eq!(
            attention(&henk(), &other),
            Attention::Ignore(IgnoreReason::OtherChannel)
        );

        let mut bot = message("hello");
        bot.author_is_bot = true;
        assert_eq!(
            attention(&henk(), &bot),
            Attention::Ignore(IgnoreReason::Bot)
        );

        let mut himself = message("hello");
        himself.author = HENK;
        assert_eq!(
            attention(&henk(), &himself),
            Attention::Ignore(IgnoreReason::Bot)
        );

        assert_eq!(
            attention(&henk(), &message("  \n")),
            Attention::Ignore(IgnoreReason::NoText)
        );
    }

    #[test]
    fn mentions_by_user_role_reply_or_name() {
        let mut by_user = message("look at this");
        by_user.mentioned_users.push(HENK);
        assert_eq!(attention(&henk(), &by_user), Attention::Mention);

        let mut by_role = message("look at this");
        by_role.mentioned_roles.push(HENK_ROLE);
        assert_eq!(attention(&henk(), &by_role), Attention::Mention);

        let mut by_reply = message("why?");
        by_reply.reply_to_author = Some(HENK);
        assert_eq!(attention(&henk(), &by_reply), Attention::Mention);

        assert_eq!(
            attention(&henk(), &message("Henk, what do you think?")),
            Attention::Mention
        );
        assert_eq!(
            attention(&henk(), &message("Henkie is a cat")),
            Attention::Judge
        );
    }

    #[test]
    fn mentioning_someone_else_and_not_him_is_ignored() {
        let mut for_bob = message("can you look?");
        for_bob.mentioned_users.push(BOB);
        assert_eq!(
            attention(&henk(), &for_bob),
            Attention::Ignore(IgnoreReason::ForSomeoneElse)
        );

        let mut for_both = message("can you both look?");
        for_both.mentioned_users.extend([BOB, HENK]);
        assert_eq!(attention(&henk(), &for_both), Attention::Mention);
    }

    #[test]
    fn plain_messages_are_judged() {
        assert_eq!(
            attention(&henk(), &message("the build is red again")),
            Attention::Judge
        );
    }

    #[test]
    fn sudo_is_only_an_order_from_a_team_lead() {
        let people = People::new([LEAD], None, []);
        let mut order = message("sudo! close issue 12");
        order.author = LEAD;
        assert!(is_sudo_order(&people, &order));

        order.author = ALICE;
        assert!(!is_sudo_order(&people, &order));

        let mut plain = message("close issue 12");
        plain.author = LEAD;
        assert!(!is_sudo_order(&people, &plain));
    }
}
