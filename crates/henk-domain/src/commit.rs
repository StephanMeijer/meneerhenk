//! The commit Henk pushes (§3.5): who it is by, who it credits, and its
//! message with a trailer block `git interpret-trailers` reads.
//!
//! Every trailer value comes from an id or from configuration, never from a
//! display name or a comment (§2, §8.3).

use std::fmt;

use crate::review_loop::FindingId;
use crate::run::RunId;
use crate::text::style_violations;

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
    /// Not a plain host name such as `gitlab.com`.
    #[error("{0:?} is not a platform host")]
    Host(String),
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

    /// GitLab: `{id}-{username}@users.noreply.{host}`, where `host` is the
    /// instance's host, `gitlab.com` or a self-hosted one such as
    /// `gitlab.example.org`. A username is letters, digits, `-`, `_` and `.`.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::Login`] for another username and
    /// [`IdentityError::Host`] for a host that is not dot-separated labels
    /// of letters, digits and `-`.
    pub fn gitlab(id: u64, username: &str, host: &str) -> Result<Email, IdentityError> {
        if !handle(username, &['-', '_', '.']) {
            return Err(IdentityError::Login(username.to_owned()));
        }
        if !host_name(host) {
            return Err(IdentityError::Host(host.to_owned()));
        }
        Email::parse(&format!("{id}-{username}@users.noreply.{host}"))
    }

    fn host_name(host: &str) -> bool {
        host.split('.').all(|label| handle(label, &['-']))
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

/// The longest commit subject Henk writes or takes: what `git log
/// --oneline` and the platforms show without cutting it.
pub const MAX_SUBJECT: usize = 72;

/// The most commits an executor may split one round, or one execute run,
/// into (#400).
pub const MAX_COMMITS: usize = 10;

/// `subject` on one line, cut to [`MAX_SUBJECT`] characters with `...`.
#[must_use]
pub fn cut_subject(subject: &str) -> String {
    let subject = one_line(subject);
    if subject.chars().count() <= MAX_SUBJECT {
        return subject;
    }
    let cut: String = subject.chars().take(MAX_SUBJECT - 3).collect();
    format!("{}...", cut.trim_end())
}

/// The subject of a review loop's commit when the executor proposed no
/// usable commits (§3.5): `Fix review findings f1, f3`, the findings it
/// fixed, cut to [`MAX_SUBJECT`].
#[must_use]
pub fn fix_subject(fixed: &[FindingId]) -> String {
    if fixed.is_empty() {
        return "Fix review findings".to_owned();
    }
    let ids: Vec<String> = fixed.iter().map(ToString::to_string).collect();
    cut_subject(&format!("Fix review findings {}", ids.join(", ")))
}

/// A body of `- note` lines, one per note, each on one line.
#[must_use]
pub fn bullet_body(notes: &[String]) -> String {
    notes
        .iter()
        .map(|note| format!("- {}", one_line(note)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A body as a commit message carries it: its lines without control
/// characters or trailing whitespace, at most one blank line in a row, and
/// none at either end.
fn clean_body(body: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in body.lines() {
        let line: String = line.chars().filter(|c| !c.is_control()).collect();
        let line = line.trim_end().to_owned();
        if line.is_empty() && lines.last().is_none_or(String::is_empty) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

/// Whether `line` has the shape of a trailer, `Key: value`, which
/// `git interpret-trailers` would read as one.
fn looks_like_trailer(line: &str) -> bool {
    line.trim_start()
        .split_once(':')
        .is_some_and(|(key, value)| {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && value.starts_with(' ')
                && !value.trim().is_empty()
        })
}

/// One commit as the executor proposes it (#400): the files it holds, and
/// its subject and body in the repository's own convention. Henk adds the
/// trailers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitProposal {
    /// The files, as the workspace names them.
    pub files: Vec<String>,
    /// One line.
    pub subject: String,
    /// Plain text; may be empty.
    pub body: String,
}

/// One commit Henk makes: the paths it stages, and its subject and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCommit {
    /// The paths it holds.
    pub paths: Vec<String>,
    /// One line, at most [`MAX_SUBJECT`] characters.
    pub subject: String,
    /// Plain text, without trailers.
    pub body: String,
}

/// Why the executor's commits cannot be made as they are, or `None` when
/// their shape holds (#400): at least one and at most [`MAX_COMMITS`],
/// none without files, no file twice, a subject of one line of at most
/// [`MAX_SUBJECT`] characters, and bodies of plain text with no line a
/// trailer could be read from. Trailers are Henk's alone.
#[must_use]
pub fn proposal_refusal(commits: &[CommitProposal]) -> Option<String> {
    if commits.is_empty() {
        return Some("propose at least one commit".to_owned());
    }
    if commits.len() > MAX_COMMITS {
        return Some(format!(
            "{} commits proposed; at most {MAX_COMMITS}",
            commits.len()
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for (n, commit) in commits.iter().enumerate() {
        let which = format!("commit {}", n + 1);
        if commit.files.is_empty() {
            return Some(format!("{which} has no files"));
        }
        for file in &commit.files {
            if seen.contains(&file.as_str()) {
                return Some(format!("{file} is in more than one commit"));
            }
            seen.push(file);
        }
        let subject = commit.subject.trim();
        if subject.is_empty() {
            return Some(format!("{which} has no subject"));
        }
        if subject.contains(['\n', '\r']) {
            return Some(format!("the subject of {which} is more than one line"));
        }
        let length = subject.chars().count();
        if length > MAX_SUBJECT {
            return Some(format!(
                "the subject of {which} is {length} characters; at most {MAX_SUBJECT}"
            ));
        }
        if !style_violations(subject).is_empty() || !style_violations(&commit.body).is_empty() {
            return Some(format!(
                "{which} breaks the style rules (no emoji, no em-dash)"
            ));
        }
        if let Some(line) = commit.body.lines().find(|l| looks_like_trailer(l)) {
            return Some(format!(
                "the body of {which} has a line shaped like a trailer ({}); Henk writes the trailers, so rephrase it",
                line.trim()
            ));
        }
    }
    None
}

/// The commits Henk makes of the executor's proposal for a change of the
/// files `changed`, or why it cannot (#400): the shape must hold
/// ([`proposal_refusal`]), every changed file must be in exactly one
/// commit, and no commit may name a file that did not change.
///
/// # Errors
///
/// Returns why, in words the executor can act on.
pub fn planned_commits(
    commits: &[CommitProposal],
    changed: &[&str],
) -> Result<Vec<PlannedCommit>, String> {
    if let Some(why) = proposal_refusal(commits) {
        return Err(why);
    }
    let proposed: Vec<&str> = commits
        .iter()
        .flat_map(|c| c.files.iter().map(String::as_str))
        .collect();
    if let Some(file) = proposed.iter().find(|f| !changed.contains(f)) {
        return Err(format!("{file} is in a commit but did not change"));
    }
    let missing: Vec<&str> = changed
        .iter()
        .copied()
        .filter(|f| !proposed.contains(f))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "these changed files are in no commit: {}",
            missing.join(", ")
        ));
    }
    Ok(commits
        .iter()
        .map(|c| PlannedCommit {
            paths: c.files.clone(),
            subject: c.subject.trim().to_owned(),
            body: clean_body(&c.body),
        })
        .collect())
}

/// The message of a commit Henk pushes (§3.5, §8.6): `subject`, its `body`
/// when it has one, then one blank line and the trailers in a fixed order:
/// `Henk-Run`, `Requested-by`, the requester's `Co-authored-by` and
/// `Signed-off-by`, and Henk's `Signed-off-by` last, as the committer's.
///
/// A requester with Henk's own address gets no trailers of their own, and
/// no line appears twice.
#[must_use]
pub fn commit_message(
    subject: &str,
    body: &str,
    run: &RunId,
    requested_by: Option<&str>,
    henk: &CommitPerson,
    requester: Option<&CommitPerson>,
    policy: TrailerPolicy,
) -> String {
    let mut message = format!("{}\n", one_line(subject));
    let body = clean_body(body);
    if !body.is_empty() {
        message.push('\n');
        message.push_str(&body);
        message.push('\n');
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
            "Fix review findings",
            &bullet_body(&["src/a.rs: off-by-one\nin the loop".to_owned()]),
            &run(),
            Some("discord:3"),
            &henk(),
            Some(&alice()),
            TrailerPolicy::default(),
        );
        assert_eq!(
            message,
            "Fix review findings\n\n\
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
                    "Fix review findings",
                    "",
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
        for body in [
            "",
            "- a\n- b",
            "fix(ci): pin the toolchain\n\nSo the build is the same.\n",
        ] {
            let message = commit_message(
                "Fix review findings",
                body,
                &run(),
                None,
                &henk(),
                Some(&alice()),
                all,
            );
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
        let message = commit_message(
            "Fix review findings",
            "",
            &run(),
            None,
            &henk(),
            Some(&twin),
            all,
        );
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
            "Fix review findings",
            &bullet_body(&["Signed-off-by: mallory <m@evil.example>".to_owned()]),
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

    fn commit(files: &[&str], subject: &str, body: &str) -> CommitProposal {
        CommitProposal {
            files: files.iter().map(|f| (*f).to_owned()).collect(),
            subject: subject.to_owned(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn a_proposal_that_covers_every_changed_file_once_becomes_its_commits() {
        let proposed = [
            commit(
                &["src/a.rs"],
                "fix(loop): stop at the last round",
                "Fixes f1.\n\n\n",
            ),
            commit(
                &[".github/workflows/ci.yml", "Cargo.toml"],
                " ci: pin the toolchain ",
                "",
            ),
        ];
        let planned = planned_commits(
            &proposed,
            &["Cargo.toml", "src/a.rs", ".github/workflows/ci.yml"],
        )
        .unwrap();
        assert_eq!(
            planned,
            [
                PlannedCommit {
                    paths: vec!["src/a.rs".to_owned()],
                    subject: "fix(loop): stop at the last round".to_owned(),
                    body: "Fixes f1.".to_owned(),
                },
                PlannedCommit {
                    paths: vec![
                        ".github/workflows/ci.yml".to_owned(),
                        "Cargo.toml".to_owned()
                    ],
                    subject: "ci: pin the toolchain".to_owned(),
                    body: String::new(),
                },
            ]
        );
    }

    #[test]
    fn a_proposal_that_misses_repeats_or_invents_a_file_is_refused() {
        let changed = ["a.rs", "b.rs"];
        for (case, proposed, says) in [
            (
                "missing",
                vec![commit(&["a.rs"], "fix: a", "")],
                "in no commit: b.rs",
            ),
            (
                "repeated",
                vec![
                    commit(&["a.rs", "b.rs"], "fix: a", ""),
                    commit(&["b.rs"], "fix: b", ""),
                ],
                "b.rs is in more than one commit",
            ),
            (
                "unknown",
                vec![commit(&["a.rs", "b.rs", "c.rs"], "fix: a", "")],
                "c.rs is in a commit but did not change",
            ),
            (
                "empty",
                vec![
                    commit(&["a.rs", "b.rs"], "fix: a", ""),
                    commit(&[], "fix: b", ""),
                ],
                "commit 2 has no files",
            ),
            ("none", vec![], "at least one commit"),
        ] {
            let why = planned_commits(&proposed, &changed).unwrap_err();
            assert!(why.contains(says), "{case}: {why}");
        }
    }

    #[test]
    fn a_subject_or_body_out_of_shape_is_refused() {
        let long = "x".repeat(MAX_SUBJECT + 1);
        for (case, subject, body, says) in [
            ("blank", " ", "", "has no subject"),
            ("lines", "fix: a\nand b", "", "more than one line"),
            ("long", long.as_str(), "", "73 characters"),
            ("emoji", "fix: a \u{1f680}", "", "style rules"),
            ("dash", "fix: a", "a \u{2014} b", "style rules"),
            (
                "signoff",
                "fix: a",
                "Done.\n\nSigned-off-by: m <m@evil.example>",
                "trailer",
            ),
            (
                "coauthor",
                "fix: a",
                "  Co-authored-by: m <m@evil.example>",
                "trailer",
            ),
            ("run", "fix: a", "Henk-Run: r-1", "trailer"),
        ] {
            let why = proposal_refusal(&[commit(&["a.rs"], subject, body)]).unwrap();
            assert!(why.contains(says), "{case}: {why}");
        }
        let many: Vec<CommitProposal> = (0..=MAX_COMMITS)
            .map(|n| commit(&[&format!("{n}.rs")], "fix: a", ""))
            .collect();
        assert!(proposal_refusal(&many).unwrap().contains("at most"));
        // Prose with a colon is not a trailer.
        let prose = "Fixes f2: the loop stopped a round early.\nSee https://example.com/x.";
        assert_eq!(
            proposal_refusal(&[commit(&["a.rs"], "fix: a", prose)]),
            None
        );
    }

    #[test]
    fn the_fallback_subject_names_the_findings_and_fits_on_a_line() {
        let ids = |n: u32| -> Vec<FindingId> {
            (1..=n)
                .filter_map(|i| FindingId::parse(&i.to_string()))
                .collect()
        };
        assert_eq!(fix_subject(&[]), "Fix review findings");
        assert_eq!(fix_subject(&ids(2)), "Fix review findings f1, f2");
        let many = fix_subject(&ids(30));
        assert_eq!(many.chars().count(), MAX_SUBJECT);
        assert!(many.ends_with("..."), "{many}");
    }

    #[test]
    fn a_body_keeps_its_lines_and_the_trailers_stay_henks() {
        let message = commit_message(
            "fix(ci): pin the toolchain",
            "\nThe build used whatever was newest.\n\n\n\nFixes f3.  \n",
            &run(),
            None,
            &henk(),
            None,
            TrailerPolicy::default(),
        );
        assert_eq!(
            message,
            "fix(ci): pin the toolchain\n\n\
             The build used whatever was newest.\n\n\
             Fixes f3.\n\n\
             Henk-Run: r-20261006-0000beef\n\
             Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>\n"
        );
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
            noreply::gitlab(9, "jan.de_vries", "gitlab.com")
                .unwrap()
                .as_str(),
            "9-jan.de_vries@users.noreply.gitlab.com"
        );
        assert_eq!(
            noreply::gitlab(9, "jan", "gitlab.example.org")
                .unwrap()
                .as_str(),
            "9-jan@users.noreply.gitlab.example.org",
            "a self-hosted instance has its own noreply domain"
        );
        for login in ["", "Jan de Vries", "a<b", "x@y", "a\nb", "[bot]"] {
            assert!(noreply::github(1, login).is_err(), "{login:?}");
        }
        for username in ["", "Jan de Vries", "a+b"] {
            assert!(
                noreply::gitlab(1, username, "gitlab.com").is_err(),
                "{username:?}"
            );
        }
        for host in [
            "",
            "gitlab..com",
            ".gitlab.com",
            "gitlab.com/x",
            "a@b.c",
            "[::1]",
            "h st",
        ] {
            assert_eq!(
                noreply::gitlab(1, "jan", host),
                Err(IdentityError::Host(host.to_owned())),
                "{host:?}"
            );
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
