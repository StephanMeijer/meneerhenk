//! Workspaces for the lanes that only read and try: a review's lanes and
//! fact-checker (#170) and the planner (#172), each opened from a commit
//! fetched by its sha, set up as the profile says and never exported.
//!
//! Reviews (#170): when the repository's profile says
//! `review = true`, every lane and the fact-checker get a workspace of their
//! own, holding the reviewed commit and prepared by the setup stage before
//! the lanes start. A lane may change files in its copy, so copies are never
//! shared, and nothing in one is ever exported (§8.2): each is wrapped by
//! [`for_lane`] as a review's.
//!
//! A workspace that cannot be had costs that lane its workspace, not the
//! review: the lane reads through its MCP session as before, and the run
//! record says why.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use henk_domain::allowlist::RepoRef;
use henk_domain::review::{CommitSha, LaneName};
use henk_domain::run::RunId;
use henk_domain::workspace::{EnvLane, Limits, Profile};
use henk_platform::ReviewTarget;
use henk_platform::address::AddressWriter;
use henk_store::RunStore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::app::App;
use crate::git::{Checkout, ScratchDir};
use crate::workspace::traced::Traced;
use crate::workspace::{Workspace, WorkspaceProvider, for_lane, setup};

/// The name the fact-checker's workspace goes by on the run record.
const FACT_CHECK: &str = "fact-check";
/// The name the planner's workspace goes by on the run record.
const PLANNER: &str = "planner";

/// The workspaces of one review, by lane, and the fact-checker's.
#[derive(Default)]
pub struct ReviewWorkspaces {
    lanes: BTreeMap<String, Arc<dyn Workspace>>,
    fact_check: Option<Arc<dyn Workspace>>,
    /// The profile's limits, for `bash` (#85).
    limits: Limits,
}

impl ReviewWorkspaces {
    /// Opens a workspace for each configured lane, and one for the
    /// fact-checker when it is configured, all at `commit` and in parallel.
    /// None when the repository's profile does not review in a workspace.
    /// Whatever fails is a warning on the run and leaves that lane without
    /// one; this never fails the review. A cancelled review stops waiting
    /// at once: what is still being opened is dropped, which destroys it,
    /// and what is open is closed.
    pub async fn open(
        app: &App,
        target: &ReviewTarget,
        commit: &CommitSha,
        run: &RunId,
        cancel: &CancellationToken,
    ) -> Self {
        let repo = target.repo.path();
        let profile = app.settings.workspace.profile_for(&repo);
        if !profile.serves(EnvLane::Review) {
            return Self::default();
        }
        let mut names: Vec<String> = app
            .settings
            .lanes
            .iter()
            .map(|lane| lane.name.as_str().to_owned())
            .collect();
        if app.settings.review.fact_check.is_some() {
            names.push(FACT_CHECK.to_owned());
        }
        let checkout = match source(app, target, commit, run).await {
            Ok(checkout) => checkout,
            Err(error) => {
                note(
                    app,
                    run,
                    "warn",
                    &format!("review workspaces: none, the reviewed commit could not be checked out: {error:#}"),
                )
                .await;
                return Self::default();
            }
        };
        let mut opening = JoinSet::new();
        for name in &names {
            // The fact-checker's checks share one copy but each get the
            // profile's run time of their own (`workspace::metered`), so its
            // copy does not hold them to one run's time together.
            let mut profile = profile.clone();
            if name == FACT_CHECK {
                profile.limits.run_secs = u64::MAX;
            }
            opening.spawn(open_one(
                Arc::clone(&app.workspace_provider),
                Arc::clone(&app.store),
                checkout.path().to_path_buf(),
                profile,
                run.clone(),
                name.clone(),
                EnvLane::Review,
            ));
        }
        let mut workspaces = Self {
            limits: profile.limits.clone(),
            ..Self::default()
        };
        let mut failed = Vec::new();
        loop {
            let joined = tokio::select! {
                joined = opening.join_next() => joined,
                () = cancel.cancelled() => {
                    opening.abort_all();
                    while let Some(joined) = opening.join_next().await {
                        if let Ok((_, Ok(workspace))) = joined {
                            workspace.close().await;
                        }
                    }
                    workspaces.close_all().await;
                    return Self::default();
                }
            };
            let Some(joined) = joined else { break };
            match joined {
                Ok((name, Ok(workspace))) if name == FACT_CHECK => {
                    workspaces.fact_check = Some(workspace);
                }
                Ok((name, Ok(workspace))) => {
                    workspaces.lanes.insert(name, workspace);
                }
                Ok((name, Err(error))) => failed.push(format!("{name}: {error:#}")),
                Err(error) => failed.push(format!("a lane: {error}")),
            }
        }
        failed.sort();
        drop(checkout);
        let ready = names.len() - failed.len();
        let mut text = format!(
            "review workspaces: {ready} of {} ready on {} at {}",
            names.len(),
            profile.backend,
            commit.short()
        );
        for why in &failed {
            text.push_str("\nno workspace for ");
            text.push_str(why);
        }
        note(
            app,
            run,
            if failed.is_empty() { "info" } else { "warn" },
            &text,
        )
        .await;
        workspaces
    }

    /// Takes `lane`'s workspace, if it has one.
    pub fn take_lane(&mut self, lane: &LaneName) -> Option<Arc<dyn Workspace>> {
        self.lanes.remove(lane.as_str())
    }

    /// The fact-checker's workspace, if it has one. It stays held, so
    /// [`Self::close_all`] closes it.
    pub fn fact_check(&self) -> Option<Arc<dyn Workspace>> {
        self.fact_check.clone()
    }

    /// The profile's limits on commands in these workspaces.
    pub fn limits(&self) -> Limits {
        self.limits.clone()
    }

    /// Closes every workspace still held. Closing one twice does nothing,
    /// so a lane may close its own first.
    pub async fn close_all(self) {
        let mut closing = JoinSet::new();
        for workspace in self.lanes.into_values().chain(self.fact_check) {
            closing.spawn(async move { workspace.close().await });
        }
        while closing.join_next().await.is_some() {}
    }
}

/// The reviewed commit, fetched by its sha from the pull request's head
/// repository with the platform's git credential, which stays in git's
/// environment on Henk's side and never reaches a workspace (§8.4).
async fn source(
    app: &App,
    target: &ReviewTarget,
    commit: &CommitSha,
    run: &RunId,
) -> anyhow::Result<Checkout> {
    let writer = app.address_writer(target.repo.platform())?;
    let facts = writer
        .pull_facts(target)
        .await
        .context("reading the pull request")?;
    fetched(writer.as_ref(), &facts.remote, commit, run).await
}

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
        ScratchDir::new(&format!("henk-lane-{run}")).context("making the checkout directory")?;
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
/// review or a planner refuses export. A workspace whose setup failed is
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
        warn!(%error, "could not record a lane's workspace");
    }
}
