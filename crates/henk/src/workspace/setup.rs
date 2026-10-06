//! The setup stage (#93): before the model's first command, the profile's
//! toolchain is installed and its setup commands run, in the tree's root,
//! each on the run's timeline. Every run that gets a workspace goes through
//! it: an address run, and a review lane when its profile asks (#170).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_domain::address::WorkspacePath;
use henk_domain::workspace::{Profile, Toolchain};

use super::Workspace;
use super::toolchain::Mise;

/// Prepares `workspace` as `profile` says. The steps come from
/// configuration only; the repository's `mise.toml` is data mise reads in
/// its safe mode, which runs nothing the file defines (§8.3). The first
/// step that fails is the error. What the steps leave in the tree is then
/// the baseline, so it is never part of a change. Returns the workspace to
/// work in: with mise, one whose every command runs through it.
///
/// # Errors
///
/// Returns the first step that failed, by name and how it ended; its
/// output is on the run's timeline.
pub async fn prepare(
    workspace: &Arc<dyn Workspace>,
    profile: &Profile,
) -> anyhow::Result<Arc<dyn Workspace>> {
    let limit = Duration::from_secs(profile.limits.command_secs);
    let mut ready = Arc::clone(workspace);
    if profile.toolchain == Some(Toolchain::Mise) {
        mise_is_safe(ready.as_ref(), limit).await?;
        setup_step(ready.as_ref(), &Mise::install(), limit).await?;
        ready = Mise::wrap(ready);
    }
    for step in &profile.setup {
        setup_step(ready.as_ref(), step, limit).await?;
    }
    if profile.toolchain.is_some() || !profile.setup.is_empty() {
        ready
            .baseline()
            .await
            .context("recording the tree after setup")?;
    }
    Ok(ready)
}

/// Runs one setup step. Its output is on the run's timeline; the error
/// names the step and how it ended, not its output, which can be long and
/// goes into the failure comment otherwise.
async fn setup_step(
    workspace: &dyn Workspace,
    step: &[String],
    limit: Duration,
) -> anyhow::Result<()> {
    let result = workspace
        .exec(step, &WorkspacePath::root(), limit)
        .await
        .with_context(|| format!("setup step `{}`", step.join(" ")))?;
    if result.code == Some(0) {
        return Ok(());
    }
    let how = if result.timed_out {
        "ran out of time".to_owned()
    } else {
        match result.code {
            Some(code) => format!("exited with {code}"),
            None => "was stopped".to_owned(),
        }
    };
    Err(anyhow!(
        "setup step `{}` {how}; its output is on the run's timeline",
        step.join(" ")
    ))
}

/// Refuses a mise that cannot run in safe mode: without it, mise would run
/// what the repository's `mise.toml` defines (§8.3).
async fn mise_is_safe(workspace: &dyn Workspace, limit: Duration) -> anyhow::Result<()> {
    let step = Mise::safe_mode();
    let result = workspace
        .exec(&step, &WorkspacePath::root(), limit)
        .await
        .with_context(|| format!("setup step `{}`", step.join(" ")))?;
    let safe = result.code == Some(0) && result.output.trim_end().ends_with("true");
    if safe {
        Ok(())
    } else {
        Err(anyhow!(
            "the workspace's mise has no safe mode (MISE_SAFE), so it could run code from the repository's mise.toml; update mise there"
        ))
    }
}
