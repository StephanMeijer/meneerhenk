//! The planner's workspace (#172): the repository at its default branch,
//! fetched by its sha, set up as the profile says and never exported (§8.2).
//!
//! A workspace that cannot be had costs the planner its workspace, not the
//! plan: the planner reads through its MCP session, and the run record says
//! why.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use henk_domain::allowlist::RepoRef;
use henk_domain::review::CommitSha;
use henk_domain::run::RunId;
use henk_domain::workspace::{EnvLane, Limits, Profile};
use henk_platform::address::AddressWriter;
use henk_store::RunStore;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::app::App;
use crate::git::{Checkout, ScratchDir};
use crate::workspace::traced::Traced;
use crate::workspace::{Workspace, WorkspaceProvider, for_lane, setup};

/// The name the planner's workspace goes by on the run record.
const PLANNER: &str = "planner";

/// `commit` of `remote`, fetched by its sha with the platform's git
/// credential, which stays in git's environment on Henk's side and never
/// reaches a workspace (§8.4).
async fn fetched(
    writer: &dyn AddressWriter,
    remote: &str,
    commit: &CommitSha,
    run: &RunId,
) -> anyhow::Result<Checkout> {
    let credential = writer
        .git_credential()
        .await
        .context("getting a credential for git")?;
    let dir =
        ScratchDir::new(&format!("henk-plan-{run}")).context("making the checkout directory")?;
    Ok(Checkout::fetch_at(dir, remote, commit, credential).await?)
}

/// The planner's workspace (#172), when the repository's profile says
/// `plan = true`: the repository at its default branch, set up as the
/// profile says, never exported, with the profile's limits for `bash`, and
/// which branch and commit it holds. Whatever fails is a warning on the run
/// and the planner works without it; a cancelled plan stops waiting at once.
pub async fn open_for_plan(
    app: &App,
    repo: &RepoRef,
    run: &RunId,
    cancel: &CancellationToken,
) -> Option<PlanWorkspace> {
    let profile = app.settings.workspace.profile_for(&repo.path());
    if !profile.serves(EnvLane::Plan) {
        return None;
    }
    let opening = async {
        let writer = app.address_writer(repo.platform())?;
        let head = writer
            .repo_head(repo)
            .await
            .context("reading the repository's default branch")?;
        let checkout = fetched(writer.as_ref(), &head.remote, &head.head, run).await?;
        let (_, workspace) = open_one(
            Arc::clone(&app.workspace_provider),
            Arc::clone(&app.store),
            checkout.path().to_path_buf(),
            profile.clone(),
            run.clone(),
            PLANNER.to_owned(),
            EnvLane::Plan,
        )
        .await;
        anyhow::Ok((workspace?, head))
    };
    let opened = tokio::select! {
        opened = opening => opened,
        () = cancel.cancelled() => return None,
    };
    match opened {
        Ok((workspace, head)) => {
            note(
                app,
                run,
                "info",
                &format!(
                    "planner workspace: ready on {} at {} {}",
                    profile.backend,
                    head.default_branch,
                    head.head.short()
                ),
            )
            .await;
            Some(PlanWorkspace {
                workspace,
                limits: profile.limits.clone(),
                branch: head.default_branch,
                commit: head.head,
            })
        }
        Err(error) => {
            note(
                app,
                run,
                "warn",
                &format!("planner workspace: none, {error:#}"),
            )
            .await;
            None
        }
    }
}

/// The planner's workspace and what it holds.
pub struct PlanWorkspace {
    /// The workspace, never exported.
    pub workspace: Arc<dyn Workspace>,
    /// The profile's limits, for `bash`.
    pub limits: Limits,
    /// The default branch it holds.
    pub branch: String,
    /// The commit it holds.
    pub commit: CommitSha,
}

/// One workspace: opened from the checkout, recorded on the run under
/// `name`, set up as the profile says and wrapped for `lane`, which for a
/// planner refuses export. A workspace whose setup failed is
/// closed again.
async fn open_one(
    provider: Arc<dyn WorkspaceProvider>,
    store: Arc<dyn RunStore>,
    source: PathBuf,
    profile: Profile,
    run: RunId,
    name: String,
    lane: EnvLane,
) -> (String, anyhow::Result<Arc<dyn Workspace>>) {
    let opened = match provider.open(&source, &profile).await {
        Ok(opened) => opened,
        Err(error) => {
            return (
                name,
                Err(anyhow::Error::new(error).context("opening the workspace")),
            );
        }
    };
    let traced: Arc<dyn Workspace> =
        Arc::new(Traced::new(opened, store, run).labelled(name.clone()));
    match setup::prepare(&traced, &profile).await {
        Ok(ready) => (name, Ok(for_lane(ready, lane))),
        Err(error) => {
            traced.close().await;
            (name, Err(error))
        }
    }
}

async fn note(app: &App, run: &RunId, level: &str, text: &str) {
    if let Err(error) = app.store.event(run, level, text).await {
        warn!(%error, "could not record the planner's workspace");
    }
}
