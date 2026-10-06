//! Domain model for Meneer Henk.
//!
//! This crate holds the vocabulary and the rules of `docs/SPEC.md` that do not
//! depend on any platform, model or transport. Everything that talks to
//! GitHub, GitLab, Discord, a mailbox or a model lives in other crates and
//! uses these types. Section numbers in doc comments refer to the spec.
//!
//! The crate is deliberately free of I/O, async and credentials (§8.4).

pub mod address;
pub mod allowlist;
pub mod diff;
pub mod discord;
pub mod finding;
pub mod identity;
pub mod ignore;
pub mod mail;
pub mod marker;
pub mod plan;
pub mod queue;
pub mod review;
pub mod run;
pub mod scope;
pub mod text;

/// Henk's full name as it appears in platform accounts and summaries.
pub const DISPLAY_NAME: &str = "Meneer Henk";

/// Henk's GitHub login, used in review commands and mentions (§3.1).
pub const GITHUB_HANDLE: &str = "meneer-henk";

/// Henk's GitLab username, used in review commands and mentions (§3.1).
pub const GITLAB_HANDLE: &str = "meneerhenk";
