//! Email (§6): what is answered, to whom, and the disclosure line.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The line every mail Henk sends ends with (§6).
pub const DISCLOSURE: &str = "This message has been written by an automated assistant.";

/// How long an outgoing mail is held before it is sent (§6).
pub const HOLD: Duration = Duration::from_secs(5 * 60);

/// Local parts that mark a machine sender (§6).
const MACHINE_LOCAL_PARTS: &[&str] = &[
    "no-reply",
    "noreply",
    "do-not-reply",
    "donotreply",
    "mailer-daemon",
    "postmaster",
    "bounce",
    "bounces",
];

/// Why an email address was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("email address {0:?} must be local@domain")]
pub struct EmailAddressError(String);

/// An email address, compared without regard to case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EmailAddress(String);

impl EmailAddress {
    /// Validates the shape `local@domain`. No display name, no brackets.
    ///
    /// # Errors
    ///
    /// Returns [`EmailAddressError`] when the value is not one `@` between two
    /// non-empty parts without whitespace.
    pub fn parse(value: &str) -> Result<Self, EmailAddressError> {
        let value = value.trim();
        let Some((local, domain)) = value.split_once('@') else {
            return Err(EmailAddressError(value.to_owned()));
        };
        let valid = !local.is_empty()
            && !domain.is_empty()
            && !domain.contains('@')
            && !value
                .chars()
                .any(|c| c.is_whitespace() || c == '<' || c == '>');
        if valid {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(EmailAddressError(value.to_owned()))
        }
    }

    /// The address as lowercase text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part before the `@`.
    #[must_use]
    pub fn local_part(&self) -> &str {
        self.0.split_once('@').map_or(&self.0, |(local, _)| local)
    }
}

impl TryFrom<String> for EmailAddress {
    type Error = EmailAddressError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<EmailAddress> for String {
    fn from(address: EmailAddress) -> Self {
        address.0
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The headers of a received mail that decide whether and how it is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMail {
    /// `From`.
    pub from: EmailAddress,
    /// `Reply-To`, if set.
    pub reply_to: Option<EmailAddress>,
    /// `To`.
    pub to: Vec<EmailAddress>,
    /// `Cc`.
    pub cc: Vec<EmailAddress>,
    /// `List-Id` or `List-Unsubscribe` present.
    pub has_list_headers: bool,
    /// `Auto-Submitted`, when present and not `no`.
    pub auto_submitted: bool,
    /// `Precedence: bulk`, `list` or `junk`, or `X-Auto-Response-Suppress`.
    pub bulk_precedence: bool,
}

/// Why a mail is automated and never answered (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomatedReason {
    /// Henk's own mail.
    OwnMail,
    /// A mailing list.
    MailingList,
    /// An auto-reply or other auto-submitted mail.
    AutoSubmitted,
    /// Bulk or list precedence.
    Bulk,
    /// A no-reply or machine sender.
    MachineSender,
}

impl IncomingMail {
    /// Whether this mail is automated, and why. Automated mail is marked read
    /// and never answered.
    #[must_use]
    pub fn automated(&self, henk: &EmailAddress) -> Option<AutomatedReason> {
        if &self.from == henk {
            Some(AutomatedReason::OwnMail)
        } else if self.has_list_headers {
            Some(AutomatedReason::MailingList)
        } else if self.auto_submitted {
            Some(AutomatedReason::AutoSubmitted)
        } else if self.bulk_precedence {
            Some(AutomatedReason::Bulk)
        } else if MACHINE_LOCAL_PARTS.contains(&self.from.local_part()) {
            Some(AutomatedReason::MachineSender)
        } else {
            None
        }
    }

    /// Who a reply goes to (§6): the sender (Reply-To if set) in `To`, and
    /// the other recipients in `Cc`, never Henk himself and never anyone who
    /// was not on the mail (§8.5).
    #[must_use]
    pub fn reply_recipients(&self, henk: &EmailAddress) -> Recipients {
        let to = self.reply_to.clone().unwrap_or_else(|| self.from.clone());
        let mut cc: Vec<EmailAddress> = Vec::new();
        for address in self
            .to
            .iter()
            .chain(&self.cc)
            .chain(std::iter::once(&self.from))
        {
            if address != henk && *address != to && !cc.contains(address) {
                cc.push(address.clone());
            }
        }
        Recipients { to, cc }
    }
}

/// The recipients of a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipients {
    /// The sender, or their Reply-To.
    pub to: EmailAddress,
    /// Everyone else who was on the mail.
    pub cc: Vec<EmailAddress>,
}

/// Appends the disclosure line to an outgoing body (§6).
#[must_use]
pub fn with_disclosure(body: &str) -> String {
    let body = body.trim_end();
    if body.is_empty() {
        DISCLOSURE.to_owned()
    } else {
        format!("{body}\n\n{DISCLOSURE}")
    }
}

/// Removes every disclosure line from a body Henk reads, so he never sees it (§6).
#[must_use]
pub fn without_disclosure(body: &str) -> String {
    let kept: Vec<&str> = body
        .lines()
        .filter(|line| line.trim() != DISCLOSURE)
        .collect();
    kept.join("\n").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn addr(value: &str) -> EmailAddress {
        EmailAddress::parse(value).unwrap_or_else(|e| panic!("{e}"))
    }

    fn henk() -> EmailAddress {
        addr("henk@example.com")
    }

    fn mail(from: &str) -> IncomingMail {
        IncomingMail {
            from: addr(from),
            reply_to: None,
            to: vec![henk()],
            cc: Vec::new(),
            has_list_headers: false,
            auto_submitted: false,
            bulk_precedence: false,
        }
    }

    #[test]
    fn addresses_are_validated_and_lowered() {
        assert_eq!(addr("Alice@Example.COM").as_str(), "alice@example.com");
        assert_eq!(addr("alice@example.com").local_part(), "alice");
        assert!(EmailAddress::parse("alice").is_err());
        assert!(EmailAddress::parse("<alice@example.com>").is_err());
        assert!(EmailAddress::parse("a@b@c").is_err());
    }

    #[test]
    fn automated_mail_is_recognised() {
        assert_eq!(
            mail("henk@example.com").automated(&henk()),
            Some(AutomatedReason::OwnMail)
        );
        assert_eq!(
            mail("No-Reply@shop.example").automated(&henk()),
            Some(AutomatedReason::MachineSender)
        );
        let mut list = mail("alice@example.com");
        list.has_list_headers = true;
        assert_eq!(list.automated(&henk()), Some(AutomatedReason::MailingList));
        let mut auto = mail("alice@example.com");
        auto.auto_submitted = true;
        assert_eq!(
            auto.automated(&henk()),
            Some(AutomatedReason::AutoSubmitted)
        );
        assert_eq!(mail("alice@example.com").automated(&henk()), None);
    }

    #[test]
    fn reply_goes_to_sender_and_copies_the_others_but_not_henk() {
        let mut incoming = mail("alice@example.com");
        incoming.to.push(addr("bob@example.com"));
        incoming.cc.push(addr("carol@example.com"));
        incoming.cc.push(addr("Alice@example.com"));
        let recipients = incoming.reply_recipients(&henk());
        assert_eq!(recipients.to, addr("alice@example.com"));
        assert_eq!(
            recipients.cc,
            vec![addr("bob@example.com"), addr("carol@example.com")]
        );
    }

    #[test]
    fn reply_to_wins_and_the_sender_is_copied() {
        let mut incoming = mail("alice@example.com");
        incoming.reply_to = Some(addr("team@example.com"));
        let recipients = incoming.reply_recipients(&henk());
        assert_eq!(recipients.to, addr("team@example.com"));
        assert_eq!(recipients.cc, vec![addr("alice@example.com")]);
    }

    #[test]
    fn disclosure_is_added_on_the_way_out_and_removed_on_the_way_in() {
        let sent = with_disclosure("Dear Alice,\n\nNot bad.\n");
        assert!(sent.ends_with(&format!("Not bad.\n\n{DISCLOSURE}")));
        assert_eq!(without_disclosure(&sent), "Dear Alice,\n\nNot bad.");
        assert_eq!(with_disclosure(""), DISCLOSURE);
    }
}
