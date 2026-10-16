//! Signing in with a ChatGPT plan instead of an API key (#620).
//!
//! OpenAI lets an app that runs on the user's own machine or server (the
//! docs say "open-source and locally hosted apps") sign the user in with
//! OAuth and call the public Responses API on the user's plan quota. This
//! module is the half that keeps the sign-in alive: the credential file a
//! sign-in leaves behind, and a [`TokenManager`] that renews the access
//! token before it expires and hands it to a model client as a bearer.
//!
//! - **One renewal at a time.** Callers that find the token stale wait for
//!   the one renewal that is running and take its result.
//! - **Saved before used.** Each renewal returns a replacement refresh token;
//!   whether the old one stops working at once is not documented, so the new
//!   pair is on disk before the new access token goes out.
//! - **No loop on a revoked sign-in.** When OpenAI says the refresh token is
//!   gone, the endpoint is signed out: every later call fails at once with a
//!   message naming the endpoint, without asking OpenAI again, until the
//!   credential file changes on disk (a new sign-in revives a running Henk).
//! - **A network failure is not a sign-out.** The file is left alone and the
//!   error is the kind the caller retries.
//!
//! A token value is never in an error, a log line or a `Debug` text.

mod credentials;
mod manager;

pub use credentials::Credentials;
pub use manager::{Clock, TOKEN_URL, TokenManager};
