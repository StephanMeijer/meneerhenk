//! A request to start work, from the API or the dashboard, as an event.
//! Both build it here, so a start from the dashboard is the same event as
//! one from the API; the listeners apply the allowlist and every refusal.

use henk_domain::review::CommitSha;
use henk_events::{Event, EventKind, EventSource};

use super::new_event;
use crate::urls::{Hosts, parse_issue_url, parse_pull_request_url};

/// What is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// A review of a pull/merge request.
    Review,
    /// A plan for an issue.
    Plan,
    /// An issue taken to a pull request (#329).
    Execute,
    /// A pull/merge request rebased onto its base branch (#579).
    Rebase,
    /// An issue driven through a plan, an execute run and a review by one
    /// autonomous run (#536).
    Ticket,
}

impl Start {
    /// The kind named in a form or path: `review`, `plan`, `execute`,
    /// `rebase` or `ticket`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "review" => Some(Self::Review),
            "plan" => Some(Self::Plan),
            "execute" => Some(Self::Execute),
            "rebase" => Some(Self::Rebase),
            "ticket" => Some(Self::Ticket),
            _ => None,
        }
    }
}

/// The event for a start request, or why the request is not one. `commit`
/// applies to a review, `note` to a plan or an execute run; the URL must
/// be on one of `hosts` (#58).
///
/// # Errors
///
/// Returns a message for the person when the URL or the commit does not
/// parse, or a commit is given for something other than a review.
pub fn start_event(
    start: Start,
    url: &str,
    commit: Option<&str>,
    note: Option<String>,
    source: EventSource,
    requester: Option<String>,
    hosts: &Hosts,
) -> Result<Event, String> {
    let kind = match start {
        Start::Review => EventKind::ReviewRequested {
            target: parse_pull_request_url(url, hosts).map_err(|e| e.to_string())?,
            commit: commit
                .map(CommitSha::parse)
                .transpose()
                .map_err(|e| e.to_string())?,
            requester,
        },
        Start::Plan | Start::Execute | Start::Rebase | Start::Ticket if commit.is_some() => {
            return Err("only a review takes a commit".to_owned());
        }
        Start::Plan => EventKind::PlanRequested {
            target: parse_issue_url(url, hosts).map_err(|e| e.to_string())?,
            note,
            requester,
        },
        Start::Execute => EventKind::ExecuteRequested {
            target: parse_issue_url(url, hosts).map_err(|e| e.to_string())?,
            note,
            requester,
        },
        Start::Ticket => EventKind::TicketRequested {
            target: parse_issue_url(url, hosts).map_err(|e| e.to_string())?,
            note,
            requester,
        },
        Start::Rebase => EventKind::RebaseRequested {
            target: parse_pull_request_url(url, hosts).map_err(|e| e.to_string())?,
            requester,
        },
    };
    Ok(new_event(source, kind, None))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// #536: a ticket run is asked for by an issue's address, takes a note
    /// and no commit.
    #[test]
    fn a_ticket_run_names_an_issue_and_takes_a_note() {
        let hosts = Hosts::new(Some("github.com"), None);
        assert_eq!(Start::parse("ticket"), Some(Start::Ticket));
        let event = start_event(
            Start::Ticket,
            "https://github.com/o/r/issues/9",
            None,
            Some("keep it small".to_owned()),
            EventSource::Api { requester: None },
            Some("github:1".to_owned()),
            &hosts,
        )
        .unwrap();
        assert!(
            matches!(&event.kind, EventKind::TicketRequested { target, note, requester } if target.number == 9 && note.as_deref() == Some("keep it small") && requester.as_deref() == Some("github:1"))
        );
        for (url, commit) in [
            ("https://github.com/o/r/pull/9", None),
            (
                "https://github.com/o/r/issues/9",
                Some("0123456789abcdef0123456789abcdef01234567"),
            ),
        ] {
            assert!(
                start_event(
                    Start::Ticket,
                    url,
                    commit,
                    None,
                    EventSource::Api { requester: None },
                    None,
                    &hosts
                )
                .is_err(),
                "{url} {commit:?}"
            );
        }
    }

    /// #579: a rebase is asked for by a pull request's address, and takes
    /// no commit.
    #[test]
    fn a_rebase_names_a_pull_request() {
        let hosts = Hosts::new(Some("github.com"), None);
        assert_eq!(Start::parse("rebase"), Some(Start::Rebase));
        let event = start_event(
            Start::Rebase,
            "https://github.com/o/r/pull/7",
            None,
            None,
            EventSource::Api { requester: None },
            Some("github:1".to_owned()),
            &hosts,
        )
        .unwrap();
        assert!(
            matches!(&event.kind, EventKind::RebaseRequested { target, requester } if target.number == 7 && requester.as_deref() == Some("github:1"))
        );
        for (url, commit) in [
            ("https://github.com/o/r/issues/7", None),
            (
                "https://github.com/o/r/pull/7",
                Some("0123456789abcdef0123456789abcdef01234567"),
            ),
        ] {
            assert!(
                start_event(
                    Start::Rebase,
                    url,
                    commit,
                    None,
                    EventSource::Api { requester: None },
                    None,
                    &hosts
                )
                .is_err(),
                "{url} {commit:?}"
            );
        }
    }
}
