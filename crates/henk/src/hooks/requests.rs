//! A request to start work, from the API or the dashboard, as an event.
//! Both build it here, so a start from the dashboard is the same event as
//! one from the API; the listeners apply the allowlist and every refusal.

use henk_domain::review::CommitSha;
use henk_events::{Event, EventKind, EventSource};

use super::new_event;
use crate::urls::{parse_issue_url, parse_pull_request_url};

/// What is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// A review of a pull/merge request.
    Review,
    /// A plan for an issue.
    Plan,
    /// Addressing a pull request's review feedback.
    Address,
}

impl Start {
    /// The kind named in a form or path: `review`, `plan` or `address`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "review" => Some(Self::Review),
            "plan" => Some(Self::Plan),
            "address" => Some(Self::Address),
            _ => None,
        }
    }
}

/// The event for a start request, or why the request is not one. `commit`
/// applies to a review, `note` to a plan or an address run.
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
) -> Result<Event, String> {
    let kind = match start {
        Start::Review => EventKind::ReviewRequested {
            target: parse_pull_request_url(url).map_err(|e| e.to_string())?,
            commit: commit
                .map(CommitSha::parse)
                .transpose()
                .map_err(|e| e.to_string())?,
            requester,
        },
        Start::Plan | Start::Address if commit.is_some() => {
            return Err("only a review takes a commit".to_owned());
        }
        Start::Plan => EventKind::PlanRequested {
            target: parse_issue_url(url).map_err(|e| e.to_string())?,
            note,
            requester,
        },
        Start::Address => EventKind::AddressRequested {
            target: parse_pull_request_url(url).map_err(|e| e.to_string())?,
            note,
            requester,
        },
    };
    Ok(new_event(source, kind, None))
}
