//! GitHub: App authentication, a small REST and GraphQL client, and the writer.

pub mod api;
pub mod app;
pub mod writer;

pub use api::GitHubApi;
pub use app::{AppCredentials, GitHubAuth};
pub use writer::GitHubWriter;
