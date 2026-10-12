//! What reacts to events: reviews, greetings, plans and execute runs (§3.1,
//! §3.4, §4, §4a).
//!
//! Each listener decides on its own whether an event concerns it, and says
//! so in its outcome. The rules every listener applies first live in
//! [`filter`].

pub mod execute;
pub mod filter;
pub mod mention;
pub mod plan;
pub mod review;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_platform::PlatformWriter;

pub use execute::ExecuteListener;
pub use mention::MentionListener;
pub use plan::PlanListener;
pub use review::ReviewListener;

/// Where listeners get a platform writer. The application implements it;
/// tests substitute a fake.
pub trait Writers: Send + Sync {
    /// The writer for a platform.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    fn writer(&self, platform: Platform) -> anyhow::Result<Arc<dyn PlatformWriter>>;
}

impl Writers for crate::app::App {
    fn writer(&self, platform: Platform) -> anyhow::Result<Arc<dyn PlatformWriter>> {
        Self::writer(self, platform)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A fake platform writer for listener tests.

    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use std::sync::{Arc, Mutex};

    use henk_domain::allowlist::Platform;
    use henk_domain::review::{CommitSha, ReviewOutcome};
    use henk_platform::{
        PlatformError, PlatformWriter, PostedComment, PullRequestInfo, PullRequestState,
        ReviewHandle, ReviewTarget,
    };

    use super::Writers;

    /// A recorded reply.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Reply {
        pub comment_id: String,
        pub is_review_comment: bool,
        pub body: String,
    }

    /// Answers `pull_request` with a fixed open head and records replies.
    #[derive(Debug, Default)]
    pub struct FakeWriter {
        pub head: String,
        pub replies: Mutex<Vec<Reply>>,
        pub pull_request_calls: Mutex<u32>,
        /// What `diff` returns.
        pub patches: Vec<henk_platform::FilePatch>,
        /// Every outcome `finish_review` was given.
        pub finished: Mutex<Vec<ReviewOutcome>>,
        /// The check id each `finish_review` closed, in order.
        pub finished_checks: Mutex<Vec<Option<String>>>,
        /// Each check `queue_review` opened: its id, title and summary
        /// (#262). Ids are `queued-1`, `queued-2`, and so on.
        pub queued: Mutex<Vec<(String, String, String)>>,
        /// `queue_review` fails instead, as an unreachable platform would.
        pub fail_queue: bool,
        /// `queue_review` for this pull request number answers only after
        /// this long, as a slow platform would.
        pub slow_queue: Option<(u64, std::time::Duration)>,
        /// The queued check each `start_review` was given, in order.
        pub started: Mutex<Vec<Option<String>>>,
        /// The link each `finish_review` was given, in order.
        pub finished_links: Mutex<Vec<String>>,
        /// The commit each `finish_review` reported on, in order.
        pub finished_commits: Mutex<Vec<String>>,
        /// The state `pull_request` answers; open when unset.
        pub state: Option<PullRequestState>,
        /// `pull_request` fails instead, with this error text.
        pub fail_pull_request: Option<String>,
        /// `start_review` fails instead, after recording what it was given.
        pub fail_start: bool,
    }

    #[async_trait::async_trait]
    impl PlatformWriter for FakeWriter {
        fn platform(&self) -> Platform {
            Platform::GitHub
        }

        async fn pull_request(&self, _: &ReviewTarget) -> Result<PullRequestInfo, PlatformError> {
            *self.pull_request_calls.lock().unwrap() += 1;
            if let Some(error) = &self.fail_pull_request {
                return Err(PlatformError::Decode(error.clone()));
            }
            Ok(PullRequestInfo {
                title: "t".into(),
                head: CommitSha::parse(&self.head).unwrap(),
                base_ref: "main".into(),
                draft: false,
                state: self.state.unwrap_or(PullRequestState::Open),
            })
        }

        async fn queue_review(
            &self,
            target: &ReviewTarget,
            _: &CommitSha,
            title: &str,
            summary: &str,
            _: &str,
        ) -> Result<Option<ReviewHandle>, PlatformError> {
            if let Some((number, delay)) = self.slow_queue
                && number == target.number
            {
                tokio::time::sleep(delay).await;
            }
            if self.fail_queue {
                return Err(PlatformError::Decode("the platform is away".to_owned()));
            }
            let mut queued = self.queued.lock().unwrap();
            let id = format!("queued-{}", queued.len() + 1);
            queued.push((id.clone(), title.to_owned(), summary.to_owned()));
            Ok(Some(ReviewHandle(id)))
        }

        async fn start_review(
            &self,
            _: &ReviewTarget,
            _: &CommitSha,
            _: &str,
            queued: Option<&ReviewHandle>,
        ) -> Result<Option<ReviewHandle>, PlatformError> {
            self.started
                .lock()
                .unwrap()
                .push(queued.map(|h| h.0.clone()));
            if self.fail_start {
                return Err(PlatformError::Decode("the check would not move".to_owned()));
            }
            Ok(queued.cloned())
        }

        async fn acknowledge(
            &self,
            _: &ReviewTarget,
            _: &str,
            _: bool,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        async fn diff(
            &self,
            _: &ReviewTarget,
            _: &CommitSha,
            _: &str,
        ) -> Result<Vec<henk_platform::FilePatch>, PlatformError> {
            Ok(self.patches.clone())
        }

        async fn post_comment(
            &self,
            _: &ReviewTarget,
            body: &str,
        ) -> Result<PostedComment, PlatformError> {
            self.replies.lock().unwrap().push(Reply {
                comment_id: String::new(),
                is_review_comment: false,
                body: body.to_owned(),
            });
            Ok(PostedComment {
                id: "1".into(),
                node_id: None,
                url: String::new(),
            })
        }

        async fn reply(
            &self,
            _: &ReviewTarget,
            comment_id: &str,
            is_review_comment: bool,
            body: &str,
        ) -> Result<PostedComment, PlatformError> {
            self.replies.lock().unwrap().push(Reply {
                comment_id: comment_id.to_owned(),
                is_review_comment,
                body: body.to_owned(),
            });
            Ok(PostedComment {
                id: "2".into(),
                node_id: None,
                url: String::new(),
            })
        }

        async fn finish_review(
            &self,
            _: &ReviewTarget,
            commit: &CommitSha,
            handle: Option<&ReviewHandle>,
            outcome: &ReviewOutcome,
            link: &str,
        ) -> Result<(), PlatformError> {
            self.finished_links.lock().unwrap().push(link.to_owned());
            self.finished_commits
                .lock()
                .unwrap()
                .push(commit.as_str().to_owned());
            self.finished.lock().unwrap().push(outcome.clone());
            self.finished_checks
                .lock()
                .unwrap()
                .push(handle.map(|h| h.0.clone()));
            Ok(())
        }
    }

    /// Hands out one fake writer for GitHub and nothing for GitLab.
    pub struct FakeWriters(pub Arc<FakeWriter>);

    impl Writers for FakeWriters {
        fn writer(&self, platform: Platform) -> anyhow::Result<Arc<dyn PlatformWriter>> {
            match platform {
                Platform::GitHub => Ok(Arc::clone(&self.0) as Arc<dyn PlatformWriter>),
                Platform::GitLab => Err(anyhow::anyhow!("GitLab is not configured")),
            }
        }
    }
}
