//! Henk's address-run commit (§3.5): who it is by, who it credits, and its
//! message with a trailer block `git interpret-trailers` reads.
//!
//! Every trailer value comes from an id or from configuration, never from a
//! display name or a comment (§2, §8.3).

use std::fmt;

use crate::run::RunId;

/// Why a commit identity is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// Not one plain `local@domain.tld` address.
    #[error("{0:?} is not a commit email address")]
    Email(String),
    /// Empty, or with `<`, `>` or a control character.
    #[error("{0:?} is not a usable commit name")]
    Name(String),
    /// Not a platform login or username.
    #[error("{0:?} is not a platform login")]
    Login(String),
}

/// An email address for a commit: one `@`, a non-empty local part, a domain
/// with a dot, and nothing that could end the `<...>` around it. Case is
/// kept as written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Email(String);

impl Email {
    /// Checks the syntax.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Email`] for anything else than a plain
    /// `local@domain.tld`.
    pub fn parse(value: &str) -> Result<Self, IdentityError> {
        let value = value.trim();
        let refused = || IdentityError::Email(value.to_owned());
        let (local, domain) = value.split_once('@').ok_or_else(refused)?;
        let clean = !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ','));
        let domain_ok = domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !domain.contains('@');
        if clean && !local.is_empty() && domain_ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(refused())
        }
    }

    /// The address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Email {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One line of text for a commit message or trailer: no line breaks.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A name and email as git records them, and as a trailer names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPerson {
    name: String,
    email: Email,
}

impl CommitPerson {
    /// A person under `name`, folded onto one line.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Name`] for an empty name or one with `<`,
    /// `>` or a control character, which would garble `name <email>`.
    pub fn new(name: &str, email: Email) -> Result<Self, IdentityError> {
        let name = one_line(name);
        if name.is_empty()
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>'))
        {
            return Err(IdentityError::Name(name));
        }
        Ok(Self { name, email })
    }

    /// The name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The email address.
    #[must_use]
    pub fn email(&self) -> &Email {
        &self.email
    }
}

impl fmt::Display for CommitPerson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <{}>", self.name, self.email)
    }
}

/// The platforms' private commit addresses, built from a user id and the
/// login that id has now: never from a display name (§2).
pub mod noreply {
    use super::{Email, IdentityError};

    /// GitHub: `{id}+{login}@users.noreply.github.com`. A login is letters,
    /// digits and `-`; an App's bot login ends in `[bot]`.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Login`] for anything else.
    pub fn github(id: u64, login: &str) -> Result<Email, IdentityError> {
        let base = login.strip_suffix("[bot]").unwrap_or(login);
        if !handle(base, &['-']) {
            return Err(IdentityError::Login(login.to_owned()));
        }
        Email::parse(&format!("{id}+{login}@users.noreply.github.com"))
    }

    /// GitLab: `{id}-{username}@users.noreply.gitlab.com`. A username is
    /// letters, digits, `-`, `_` and `.`.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Login`] for anything else.
    pub fn gitlab(id: u64, username: &str) -> Result<Email, IdentityError> {
        if !handle(username, &['-', '_', '.']) {
            return Err(IdentityError::Login(username.to_owned()));
        }
        Email::parse(&format!("{id}-{username}@users.noreply.gitlab.com"))
    }

    fn handle(value: &str, extra: &[char]) -> bool {
        !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    }
}

/// Which trailers the commit carries, per repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrailerPolicy {
    /// `Signed-off-by` for Henk, the committer.
    pub henk_signoff: bool,
    /// `Co-authored-by` for the colleague who asked.
    pub requester_coauthor: bool,
    /// `Signed-off-by` for the colleague who asked: they certify the DCO for
    /// a change they asked for but did not type. Off unless the operator
    /// turns it on for a repository.
    pub requester_signoff: bool,
}

impl Default for TrailerPolicy {
    fn default() -> Self {
        Self {
            henk_signoff: true,
            requester_coauthor: true,
            requester_signoff: false,
        }
    }
}

/// The message of Henk's one commit (§3.5, §8.6): a subject, what he fixed,
/// then one blank line and the trailers in a fixed order: `Henk-Run`,
/// `Requested-by`, the requester's `Co-authored-by` and `Signed-off-by`,
/// and Henk's `Signed-off-by` last, as the committer's.
///
/// A requester with Henk's own address gets no trailers of their own, and
/// no line appears twice.
#[must_use]
pub fn commit_message(
    fixed: &[String],
    run: &RunId,
    requested_by: Option<&str>,
    henk: &CommitPerson,
    requester: Option<&CommitPerson>,
    policy: TrailerPolicy,
) -> String {
    let mut message = String::from("Address review feedback\n");
    if !fixed.is_empty() {
        message.push('\n');
        for note in fixed {
            message.push_str("- ");
            message.push_str(&one_line(note));
            message.push('\n');
        }
    }

    let requester =
        requester.filter(|r| !r.email.as_str().eq_ignore_ascii_case(henk.email.as_str()));
    let mut trailers = vec![format!("Henk-Run: {}", run.as_str())];
    if let Some(by) = requested_by.map(one_line).filter(|r| !r.is_empty()) {
        trailers.push(format!("Requested-by: {by}"));
    }
    if let Some(requester) = requester {
        if policy.requester_coauthor {
            trailers.push(format!("Co-authored-by: {requester}"));
        }
        if policy.requester_signoff {
            trailers.push(format!("Signed-off-by: {requester}"));
        }
    }
    if policy.henk_signoff {
        trailers.push(format!("Signed-off-by: {henk}"));
    }

    message.push('\n');
    let mut seen = Vec::new();
    for line in trailers {
        if !seen.contains(&line) {
            message.push_str(&line);
            message.push('\n');
            seen.push(line);
        }
    }
    message
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use super::*;

    fn run() -> RunId {
        RunId::parse("r-20261006-0000beef").unwrap()
    }

    fn henk() -> CommitPerson {
        CommitPerson::new(
            "meneer-henk[bot]",
            noreply::github(1, "meneer-henk[bot]").unwrap(),
        )
        .unwrap()
    }

    fn alice() -> CommitPerson {
        CommitPerson::new("alice", noreply::github(7, "alice").unwrap()).unwrap()
    }

    /// The last paragraph, which is what git reads as the trailer block.
    fn trailer_block(message: &str) -> Vec<&str> {
        let trimmed = message.trim_end_matches('\n');
        let (_, last) = trimmed.rsplit_once("\n\n").unwrap();
        last.lines().collect()
    }

    /// `^[A-Za-z-]+: .+$`, the shape `git interpret-trailers --parse` takes.
    fn is_trailer(line: &str) -> bool {
        line.split_once(": ").is_some_and(|(key, value)| {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
                && !value.trim().is_empty()
        })
    }

    #[test]
    fn the_default_is_henks_signoff_and_the_requesters_coauthor() {
        let message = commit_message(
            &["src/a.rs: off-by-one\nin the loop".to_owned()],
            &run(),
            Some("discord:3"),
            &henk(),
            Some(&alice()),
            TrailerPolicy::default(),
        );
        assert_eq!(
            message,
            "Address review feedback\n\n\
             - src/a.rs: off-by-one in the loop\n\n\
             Henk-Run: r-20261006-0000beef\n\
             Requested-by: discord:3\n\
             Co-authored-by: alice <7+alice@users.noreply.github.com>\n\
             Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>\n"
        );
        assert!(crate::text::is_in_style(&message));
    }

    #[test]
    fn every_policy_combination_gives_exactly_its_trailers() {
        for bits in 0..8u8 {
            let policy = TrailerPolicy {
                henk_signoff: bits & 1 != 0,
                requester_coauthor: bits & 2 != 0,
                requester_signoff: bits & 4 != 0,
            };
            for requester in [None, Some(alice())] {
                let message = commit_message(
                    &[],
                    &run(),
                    Some("discord:3"),
                    &henk(),
                    requester.as_ref(),
                    policy,
                );
                let block = trailer_block(&message);
                let mut expected = vec![
                    "Henk-Run: r-20261006-0000beef".to_owned(),
                    "Requested-by: discord:3".to_owned(),
                ];
                if requester.is_some() && policy.requester_coauthor {
                    expected.push(format!("Co-authored-by: {}", alice()));
                }
                if requester.is_some() && policy.requester_signoff {
                    expected.push(format!("Signed-off-by: {}", alice()));
                }
                if policy.henk_signoff {
                    expected.push(format!("Signed-off-by: {}", henk()));
                }
                assert_eq!(block, expected, "{policy:?} {requester:?}");
                assert!(block.iter().all(|l| is_trailer(l)), "{message}");
            }
        }
    }

    #[test]
    fn the_trailer_block_is_one_paragraph_after_one_blank_line() {
        let all = TrailerPolicy {
            henk_signoff: true,
            requester_coauthor: true,
            requester_signoff: true,
        };
        for fixed in [vec![], vec!["a".to_owned(), "b".to_owned()]] {
            let message = commit_message(&fixed, &run(), None, &henk(), Some(&alice()), all);
            assert!(!message.contains("\n\n\n"), "{message}");
            let block = trailer_block(&message);
            assert_eq!(block.first(), Some(&"Henk-Run: r-20261006-0000beef"));
            assert_eq!(block.len(), 4, "{message}");
            assert!(block.iter().all(|l| is_trailer(l)), "{message}");
            assert!(message.ends_with('\n') && !message.ends_with("\n\n"));
        }
    }

    #[test]
    fn a_requester_with_henks_address_gets_no_trailers_and_none_repeat() {
        let twin = CommitPerson::new(
            "someone",
            Email::parse("1+MENEER-HENK[bot]@users.noreply.github.com").unwrap(),
        )
        .unwrap();
        let all = TrailerPolicy {
            henk_signoff: true,
            requester_coauthor: true,
            requester_signoff: true,
        };
        let message = commit_message(&[], &run(), None, &henk(), Some(&twin), all);
        assert_eq!(
            trailer_block(&message),
            [
                "Henk-Run: r-20261006-0000beef".to_owned(),
                format!("Signed-off-by: {}", henk()),
            ]
        );
    }

    #[test]
    fn a_note_that_looks_like_a_trailer_stays_in_the_body() {
        let message = commit_message(
            &["Signed-off-by: mallory <m@evil.example>".to_owned()],
            &run(),
            None,
            &henk(),
            None,
            TrailerPolicy::default(),
        );
        let block = trailer_block(&message);
        assert!(block.iter().all(|l| !l.contains("evil")), "{message}");
        assert!(message.contains("- Signed-off-by: mallory"), "{message}");
    }

    #[test]
    fn noreply_addresses_are_built_from_ids_and_handles() {
        assert_eq!(
            noreply::github(42, "octo-cat").unwrap().as_str(),
            "42+octo-cat@users.noreply.github.com"
        );
        assert_eq!(
            noreply::github(1, "meneer-henk[bot]").unwrap().as_str(),
            "1+meneer-henk[bot]@users.noreply.github.com"
        );
        assert_eq!(
            noreply::gitlab(9, "jan.de_vries").unwrap().as_str(),
            "9-jan.de_vries@users.noreply.gitlab.com"
        );
        for login in ["", "Jan de Vries", "a<b", "x@y", "a\nb", "[bot]"] {
            assert!(noreply::github(1, login).is_err(), "{login:?}");
        }
        for username in ["", "Jan de Vries", "a+b"] {
            assert!(noreply::gitlab(1, username).is_err(), "{username:?}");
        }
    }

    #[test]
    fn emails_are_checked() {
        for good in [
            "a@b.c",
            " Jan.Smit@Example.com ",
            "1+x[bot]@users.noreply.github.com",
        ] {
            assert!(Email::parse(good).is_ok(), "{good}");
        }
        assert_eq!(Email::parse("A@B.nl").unwrap().as_str(), "A@B.nl");
        for bad in [
            "",
            "a",
            "@b.c",
            "a@b",
            "a@.b",
            "a@b.",
            "a@b@c.d",
            "a b@c.d",
            "<a@b.c>",
            "a@b.c,d@e.f",
            "a@b\n.c",
            "a\u{7}@b.c",
        ] {
            assert!(Email::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_are_one_line_without_brackets() {
        let email = Email::parse("a@b.c").unwrap();
        assert_eq!(
            CommitPerson::new(" Jan\nde  Vries ", email.clone())
                .unwrap()
                .name(),
            "Jan de Vries"
        );
        for bad in ["", " \n ", "Jan <jan@x.y>", "a>b", "a\u{0}b"] {
            assert!(
                matches!(
                    CommitPerson::new(bad, email.clone()),
                    Err(IdentityError::Name(_))
                ),
                "{bad:?}"
            );
        }
    }
}
