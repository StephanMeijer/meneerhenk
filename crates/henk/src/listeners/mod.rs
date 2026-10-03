//! What reacts to events: reviews, greetings and plans (§3.1, §3.4, §4).
//!
//! Each listener decides on its own whether an event concerns it, and says
//! so in its outcome. The rules every listener applies first live in
//! [`filter`].

pub mod filter;
pub mod mention;
pub mod plan;
pub mod review;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_platform::PlatformWriter;

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
        DiffSide, ExistingFinding, ExistingSummary, PlatformError, PlatformWriter, PostedComment,
        PullRequestInfo, PullRequestState, ReviewHandle, ReviewTarget,
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
    }

    #[async_trait::async_trait]
    impl PlatformWriter for FakeWriter {
        fn platform(&self) -> Platform {
            Platform::GitHub
        }

        async fn pull_request(&self, _: &ReviewTarget) -> Result<PullRequestInfo, PlatformError> {
            *self.pull_request_calls.lock().unwrap() += 1;
            Ok(PullRequestInfo {
                title: "t".into(),
                head: CommitSha::parse(&self.head).unwrap(),
                base_ref: "main".into(),
                draft: false,
                state: PullRequestState::Open,
            })
        }

        async fn start_review(
            &self,
            _: &ReviewTarget,
            _: &CommitSha,
            _: &str,
        ) -> Result<Option<ReviewHandle>, PlatformError> {
            Ok(None)
        }

        async fn acknowledge(
            &self,
            _: &ReviewTarget,
            _: &str,
            _: bool,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        async fn existing_findings(
            &self,
            _: &ReviewTarget,
        ) -> Result<Vec<ExistingFinding>, PlatformError> {
            Ok(Vec::new())
        }

        async fn existing_summaries(
            &self,
            _: &ReviewTarget,
        ) -> Result<Vec<ExistingSummary>, PlatformError> {
            Ok(Vec::new())
        }

        async fn post_finding(
            &self,
            _: &ReviewTarget,
            _: &CommitSha,
            _: &str,
            _: u32,
            _: DiffSide,
            _: &str,
        ) -> Result<PostedComment, PlatformError> {
            Err(PlatformError::Decode("not in the fake".into()))
        }

        async fn update_finding(
            &self,
            _: &ReviewTarget,
            _: &str,
            _: &str,
        ) -> Result<(), PlatformError> {
            Ok(())
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

        async fn fold_summary(
            &self,
            _: &ReviewTarget,
            _: &ExistingSummary,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        async fn fold_finding(
            &self,
            _: &ReviewTarget,
            _: &ExistingFinding,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        async fn finish_review(
            &self,
            _: &ReviewTarget,
            _: &CommitSha,
            _: Option<&ReviewHandle>,
            _: &ReviewOutcome,
            _: &str,
        ) -> Result<(), PlatformError> {
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
