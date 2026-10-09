//! The `henk` binary.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the CLI's output is stdout and stderr"
)]

mod address;
mod address_tools;
mod app;
mod cancel;
mod checks;
mod code_tools;
mod config;
mod coordinator;
mod dashboard;
mod doctor;
mod drafts;
mod fact_check;
mod git;
mod hooks;
mod ids;
mod lane_workspace;
mod listeners;
mod live;
mod live_session;
mod liveness;
mod mcp_server;
mod pages;
mod plan;
mod plan_tools;
mod prune;
mod recorder;
mod resume;
mod review;
mod review_many;
mod review_tools;
mod runs;
mod server;
mod skill_tools;
mod skills;
mod stages;
mod urls;
mod web_fetch;
mod workspace;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::urls::{parse_issue_url, parse_pull_request_url};
use anyhow::{Context as _, anyhow};
use clap::{Parser, Subcommand};
use henk_domain::allowlist::Platform;
use henk_llm::{ChatMessage, CompletionRequest};
use henk_mcp::McpSession as _;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::app::App;
use crate::config::{Config, Settings};

/// Meneer Henk: the team's AI colleague.
#[derive(Debug, Parser)]
#[command(name = "henk", version, about, long_about = None)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, global = true, default_value = "henk.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Work with the configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Check secrets, models, the GitHub App, MCP servers and the database.
    Doctor {
        /// Also send one short prompt to every configured model.
        #[arg(long)]
        probe: bool,
    },
    /// Serve webhooks, the API and run pages until stopped.
    Serve,
    /// Review pull requests now, from the command line: at most 50, and at
    /// most `review.max_concurrent` at a time.
    Review {
        /// Pull or merge request URLs, such as <https://github.com/owner/repo/pull/7>
        #[arg(required = true, num_args = 1..)]
        urls: Vec<String>,
        /// Review this commit instead of the current head (one URL only).
        #[arg(long)]
        commit: Option<String>,
    },
    /// Plan one issue now, from the command line (§4).
    Plan {
        /// Issue URL, such as <https://github.com/owner/repo/issues/9>
        url: String,
        /// A note for the planner, as a colleague would give in conversation.
        #[arg(long)]
        note: Option<String>,
    },
    /// Address the review feedback on one pull request: fix what is right,
    /// push one commit to its branch, reply in every thread (§3.5).
    Address {
        /// Pull request URL, such as <https://github.com/owner/repo/pull/7>
        url: String,
        /// A note, as a colleague would give in conversation.
        #[arg(long)]
        note: Option<String>,
    },
    /// Talk to a configured model once.
    Llm {
        #[command(subcommand)]
        command: LlmCommand,
    },
    /// Inspect a configured MCP server.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Read run records from the local database.
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RunsCommand {
    /// Print one run: lanes, findings and timeline.
    Show {
        /// The run id, such as `r-20261005-1a2b3c4d`.
        run: String,
        /// Print the whole conversation of this session instead, as the
        /// run's transcripts list them.
        #[arg(long, value_name = "SESSION")]
        transcript: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Load the configuration file and report what Henk would work with.
    Check,
    /// Print an example configuration file.
    Example,
}

#[derive(Debug, Subcommand)]
enum LlmCommand {
    /// Send one prompt and print the answer.
    Probe {
        /// Model id from `[models]`.
        #[arg(long)]
        model: String,
        /// The prompt.
        #[arg(long, default_value = "Say 'Not bad.' and nothing else.")]
        prompt: String,
    },
    /// List the models the endpoint of a configured model serves.
    Models {
        /// Model id from `[models]`; its base URL, provider and key are used.
        #[arg(long)]
        model: String,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Start a server, list its tools and show which a review lane would see.
    Probe {
        /// Alias from `[mcp]`.
        #[arg(long)]
        server: String,
        /// Print the input schema of these tools.
        #[arg(long = "show")]
        show: Vec<String>,
        /// Which scope table to compare against. Defaults to the platform
        /// whose settings name the alias (#61).
        #[arg(long, value_enum)]
        platform: Option<PlatformArg>,
    },
}

/// A platform as `--platform` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum PlatformArg {
    /// GitHub.
    Github,
    /// GitLab.
    Gitlab,
}

impl From<PlatformArg> for Platform {
    fn from(arg: PlatformArg) -> Self {
        match arg {
            PlatformArg::Github => Self::GitHub,
            PlatformArg::Gitlab => Self::GitLab,
        }
    }
}

fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn,henk=debug"));
    if std::env::var("HENK_LOG_JSON").is_ok_and(|v| v == "1") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("henk: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(Cli::parse())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("henk: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn load_settings(path: &Path) -> anyhow::Result<Settings> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config = Config::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut settings = config.into_settings().context("validating configuration")?;
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    skills::attach(&mut settings, config_dir)?;
    Ok(settings)
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Config {
            command: ConfigCommand::Check,
        } => {
            let settings = load_settings(&cli.config)?;
            println!("{}", settings.describe());
            // Whether each MCP client's token is set (#250); --probe is
            // doctor's.
            let mcp = doctor::check_mcp_server(&settings, false, app::env_var).await;
            if !mcp.is_empty() {
                let rendered = doctor::render(&mcp);
                // The checks' lines, without doctor's count at the end.
                for line in rendered.lines().filter(|line| line.starts_with('[')) {
                    println!("{line}");
                }
            }
            // A setting that needs git where Henk runs is only as good as
            // the git there (#254).
            let git = doctor::check_git(&settings, None).await;
            let rendered = doctor::render(std::slice::from_ref(&git));
            println!("{}", rendered.lines().next().unwrap_or_default());
            if git.is_failure() {
                return Err(anyhow!(
                    "git does not run here, and the configuration needs it"
                ));
            }
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Example,
        } => {
            print!("{}", config::EXAMPLE);
            Ok(())
        }
        Command::Doctor { probe } => {
            let settings = load_settings(&cli.config)?;
            let checks = doctor::run(&settings, probe).await;
            println!("{}", doctor::render(&checks));
            if checks.iter().any(doctor::Check::is_failure) {
                return Err(anyhow!("doctor found failing checks"));
            }
            Ok(())
        }
        Command::Serve => {
            let settings = load_settings(&cli.config)?;
            let app = Arc::new(App::build(settings, None).await?);
            server::serve(app).await
        }
        Command::Review { urls, commit } => cmd_review(&cli.config, &urls, commit).await,
        Command::Plan { url, note } => cmd_plan(&cli.config, &url, note).await,
        Command::Address { url, note } => cmd_address(&cli.config, &url, note).await,
        Command::Llm {
            command: LlmCommand::Probe { model, prompt },
        } => cmd_llm_probe(&cli.config, &model, prompt).await,
        Command::Llm {
            command: LlmCommand::Models { model },
        } => cmd_llm_models(&cli.config, &model).await,
        Command::Mcp {
            command:
                McpCommand::Probe {
                    server,
                    show,
                    platform,
                },
        } => cmd_mcp_probe(&cli.config, &server, &show, platform.map(Platform::from)).await,
        Command::Runs {
            command: RunsCommand::Show { run, transcript },
        } => cmd_runs_show(&cli.config, &run, transcript.as_deref()).await,
    }
}

async fn cmd_runs_show(config: &Path, run: &str, transcript: Option<&str>) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let store = app::open_store(&settings.database).await?;
    let id = henk_domain::run::RunId::parse(run)?;
    let record = store
        .run(&id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| anyhow!("no run {run} in {}", settings.database.describe()))?;
    let transcripts = store.transcripts(&id).await?;
    if let Some(session) = transcript {
        let Some(stored) = store.transcript(&id, session).await? else {
            let known: Vec<&str> = transcripts.iter().map(|t| t.session.as_str()).collect();
            return Err(anyhow!(
                "run {run} has no transcript of {session}; it has: {}",
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            ));
        };
        print!(
            "{}",
            runs::transcript_text(&stored.body).context("reading the stored transcript")?
        );
        return Ok(());
    }
    print!(
        "{}",
        runs::render(
            &record,
            &store.lanes(&id).await?,
            &henk_store::ToolUsage::from_calls(&store.tool_calls(&id).await?),
            &transcripts,
            &store.drafts(&id).await?,
            &store.findings(&id).await?,
            &store.events(&id).await?
        )
    );
    Ok(())
}

async fn cmd_review(config: &Path, urls: &[String], commit: Option<String>) -> anyhow::Result<()> {
    let targets = urls::parse_review_targets(urls, commit.as_deref())?;
    let commit = commit
        .map(|c| henk_domain::review::CommitSha::parse(&c))
        .transpose()?;
    let settings = load_settings(config)?;
    let app = Arc::new(App::build(settings, None).await?);
    liveness::reap_orphans(&app).await;
    interrupt_on_ctrl_c(app.shutdown.clone());
    let requester = app
        .settings
        .planning
        .as_ref()
        .map(|p| p.requester_id.to_string());
    let single = targets.len() == 1;
    let jobs: Vec<(String, henk_platform::ReviewTarget, henk_domain::run::RunId)> = targets
        .into_iter()
        .map(|(url, target)| (url, target, ids::new_run_id()))
        .collect();
    for (url, _, run) in &jobs {
        if single {
            println!("run {run}");
        } else {
            println!("run {run} {url}");
        }
    }
    let limit = app.settings.review.max_concurrent;
    let results = review_many::review_all(jobs, limit, |(_, target, run)| {
        let app = Arc::clone(&app);
        let request = review::ReviewRequest {
            target,
            commit: commit.clone(),
            trigger: "cli".to_owned(),
            requester: requester.clone(),
            acknowledge: None,
            run: Some(run.clone()),
            submitted_at: None,
            queued_check: None,
        };
        async move {
            review::run_review(&app, request, app.shutdown.child_token())
                .await
                .with_context(|| format!("run {run}"))
        }
    })
    .await
    .into_iter()
    // A crashed review task is a failed review, reported like any other.
    .map(|(job, result)| (job, result.map_err(anyhow::Error::from).and_then(|r| r)))
    .collect::<Vec<_>>();

    let mut results = results;
    if single && let Some((_, result)) = results.pop() {
        let report = result?;
        if let Some(summary) = report.summary {
            println!("{summary}");
        }
        if let Some(outcome) = report.outcome {
            println!("check conclusion: {:?}", outcome.check_conclusion());
        }
        return Ok(());
    }
    let total = results.len();
    let mut failed = 0;
    for ((url, _, run), result) in results {
        match result {
            Ok(report) => println!(
                "{url} run {}: {}",
                report.run,
                report.summary.as_deref().unwrap_or("finished")
            ),
            Err(error) => {
                failed += 1;
                println!("{url} run {run}: failed: {error:#}");
            }
        }
    }
    if failed > 0 {
        return Err(anyhow!("{failed} of {total} reviews failed"));
    }
    Ok(())
}

/// The first Ctrl-C stops the runs in flight, which close their checks and
/// end; the second quits at once (#7).
fn interrupt_on_ctrl_c(shutdown: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        eprintln!("Interrupting; the review closes its check. Press Ctrl-C again to quit at once.");
        shutdown.cancel();
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
}

async fn cmd_plan(config: &Path, url: &str, note: Option<String>) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, None).await?;
    liveness::reap_orphans(&app).await;
    interrupt_on_ctrl_c(app.shutdown.clone());
    let target = parse_issue_url(url)?;
    let report = plan::run_plan(
        &app,
        plan::PlanRequest {
            target,
            note,
            trigger: "cli".to_owned(),
            run: None,
            requester: None,
        },
        app.shutdown.child_token(),
    )
    .await?;
    println!("run {}", report.run);
    println!("plan written: {}", report.planned);
    for change in report.changes {
        println!("- {change}");
    }
    Ok(())
}

async fn cmd_address(config: &Path, url: &str, note: Option<String>) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, None).await?;
    liveness::reap_orphans(&app).await;
    interrupt_on_ctrl_c(app.shutdown.clone());
    let target = parse_pull_request_url(url)?;
    let report = address::run_address(
        &app,
        address::AddressRequest {
            target,
            note,
            trigger: "cli".to_owned(),
            run: None,
            requester: None,
        },
        app.shutdown.child_token(),
    )
    .await?;
    println!("run {}", report.run);
    match &report.commit {
        Some(commit) => println!("pushed {commit}"),
        None => println!("nothing pushed"),
    }
    println!(
        "{} fixed, {} declined, {} questions, {} not settled",
        report.fixed, report.declined, report.questions, report.unsettled
    );
    Ok(())
}

async fn cmd_llm_probe(config: &Path, model: &str, prompt: String) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, Some(Path::new(":memory:"))).await?;
    let client = app.model(model)?;
    let completion = client
        .complete(&CompletionRequest {
            system: Some(henk_agent::prompts::PERSONA.to_owned()),
            messages: vec![ChatMessage::user(prompt)],
            ..Default::default()
        })
        .await?;
    println!("{}", completion.message.text());
    eprintln!(
        "stop: {:?}; tokens in {} out {}",
        completion.stop, completion.usage.input_tokens, completion.usage.output_tokens
    );
    Ok(())
}

async fn cmd_llm_models(config: &Path, model: &str) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let entry = settings
        .models
        .get(model)
        .ok_or_else(|| anyhow!("model {model:?} is not configured"))?;
    let client_config = entry
        .to_client_config(app::env_var)
        .map_err(|variable| anyhow!("environment variable {variable} is not set"))?;
    let ids = henk_llm::list_models(&client_config).await?;
    eprintln!("{} models at {}:", ids.len(), entry.base_url);
    for id in ids {
        println!("{id}");
    }
    Ok(())
}

/// The platform whose scope table `henk mcp probe` compares `alias` against
/// (#61): GitLab when the alias is `gitlab.mcp_server` or
/// `gitlab.write_mcp_server`, GitHub when it is `github.mcp_server`.
///
/// # Errors
///
/// Fails when neither section names the alias, or both do, so the operator
/// passes `--platform`.
fn probe_platform(settings: &Settings, alias: &str) -> anyhow::Result<Platform> {
    let gitlab = settings
        .gitlab
        .as_ref()
        .is_some_and(|gitlab| gitlab.mcp_server == alias || gitlab.write_mcp_server == alias);
    let github = settings
        .github
        .as_ref()
        .is_some_and(|github| github.mcp_server == alias);
    match (github, gitlab) {
        (true, false) => Ok(Platform::GitHub),
        (false, true) => Ok(Platform::GitLab),
        (true, true) => Err(anyhow!(
            "MCP server {alias:?} is named by both [github] and [gitlab]; pass --platform github|gitlab"
        )),
        (false, false) => Err(anyhow!(
            "MCP server {alias:?} is not named by [github] or [gitlab]; pass --platform github|gitlab"
        )),
    }
}

async fn cmd_mcp_probe(
    config: &Path,
    server: &str,
    show: &[String],
    platform: Option<Platform>,
) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let server_config = settings
        .mcp
        .get(server)
        .ok_or_else(|| anyhow!("MCP server {server:?} is not configured"))?;
    let platform = match platform {
        Some(platform) => platform,
        None => probe_platform(&settings, server)?,
    };
    let session = henk_mcp::RmcpSession::connect(server, server_config, app::env_var).await?;
    let tools = session.list_tools().await?;
    println!("{} tools from {server}:", tools.len());
    for tool in &tools {
        let exposed = if henk_domain::scope::is_exposed(platform, &tool.name) {
            "lane"
        } else {
            "    "
        };
        println!("  [{exposed}] {}", tool.name);
    }
    let missing: Vec<&str> = henk_domain::scope::exposed_tools(platform)
        .filter(|wanted| !tools.iter().any(|t| t.name == *wanted))
        .collect();
    if !missing.is_empty() {
        println!(
            "expected by the guard but not offered: {}",
            missing.join(", ")
        );
    }
    for wanted in show {
        match tools.iter().find(|t| &t.name == wanted) {
            Some(tool) => {
                println!(
                    "\n{}: {}\n{}",
                    tool.name,
                    tool.description,
                    serde_json::to_string_pretty(&tool.input_schema)?
                );
            }
            None => println!("\n{wanted}: not offered"),
        }
    }
    session.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn review_takes_one_or_more_urls() {
        assert!(
            Cli::try_parse_from(["henk", "review"]).is_err(),
            "at least one"
        );
        let cli = Cli::try_parse_from([
            "henk",
            "review",
            "https://github.com/o/r/pull/1",
            "https://github.com/o/r/pull/2",
            "https://github.com/o/r/pull/3",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        let Command::Review { urls, commit } = cli.command else {
            panic!("not a review");
        };
        assert_eq!(urls.len(), 3);
        assert_eq!(commit, None);
    }

    fn example_settings() -> Settings {
        Config::parse(config::EXAMPLE)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn probe_platform_follows_the_settings_not_the_alias_text() {
        let mut settings = example_settings();
        let Some(gitlab) = settings.gitlab.as_mut() else {
            panic!("the example has a [gitlab] section");
        };
        gitlab.mcp_server = "gl".to_owned();
        gitlab.write_mcp_server = "glw".to_owned();
        let Some(github) = settings.github.as_mut() else {
            panic!("the example has a [github] section");
        };
        github.mcp_server = "gitlab-ish".to_owned();
        let platform = |alias| probe_platform(&settings, alias).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(platform("gl"), Platform::GitLab);
        assert_eq!(platform("glw"), Platform::GitLab);
        assert_eq!(platform("gitlab-ish"), Platform::GitHub);
    }

    #[test]
    fn probe_platform_rejects_an_alias_neither_section_names() {
        let settings = example_settings();
        let error = probe_platform(&settings, "other")
            .err()
            .unwrap_or_else(|| panic!("no section names it"));
        assert!(error.to_string().contains("--platform"), "{error}");
    }

    #[test]
    fn probe_takes_an_optional_platform() {
        let cli = Cli::try_parse_from(["henk", "mcp", "probe", "--server", "gl"])
            .unwrap_or_else(|e| panic!("{e}"));
        let Command::Mcp {
            command: McpCommand::Probe { platform, .. },
        } = cli.command
        else {
            panic!("not a probe");
        };
        assert_eq!(platform, None);
        let cli = Cli::try_parse_from([
            "henk",
            "mcp",
            "probe",
            "--server",
            "gl",
            "--platform",
            "gitlab",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        let Command::Mcp {
            command: McpCommand::Probe { platform, .. },
        } = cli.command
        else {
            panic!("not a probe");
        };
        assert_eq!(platform, Some(PlatformArg::Gitlab));
    }
}
