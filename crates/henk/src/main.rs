//! The `henk` binary.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the CLI's output is stdout and stderr"
)]

mod app;
mod config;
mod coordinator;
mod doctor;
mod fact_check;
mod hooks;
mod ids;
mod listeners;
mod plan;
mod plan_tools;
mod recorder;
mod review;
mod review_tools;
mod runs;
mod server;
mod urls;
mod web_fetch;

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
    /// Review one pull request now, from the command line.
    Review {
        /// Pull request URL, such as <https://github.com/owner/repo/pull/7>
        url: String,
        /// Review this commit instead of the current head.
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
    },
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
    config.into_settings().context("validating configuration")
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Config {
            command: ConfigCommand::Check,
        } => {
            println!("{}", load_settings(&cli.config)?.describe());
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
        Command::Review { url, commit } => cmd_review(&cli.config, &url, commit).await,
        Command::Plan { url, note } => cmd_plan(&cli.config, &url, note).await,
        Command::Llm {
            command: LlmCommand::Probe { model, prompt },
        } => cmd_llm_probe(&cli.config, &model, prompt).await,
        Command::Llm {
            command: LlmCommand::Models { model },
        } => cmd_llm_models(&cli.config, &model).await,
        Command::Mcp {
            command: McpCommand::Probe { server, show },
        } => cmd_mcp_probe(&cli.config, &server, &show).await,
        Command::Runs {
            command: RunsCommand::Show { run },
        } => cmd_runs_show(&cli.config, &run),
    }
}

fn cmd_runs_show(config: &Path, run: &str) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let store = henk_store::RunStore::open(Path::new(&settings.server.database_path))?;
    let id = henk_domain::run::RunId::parse(run)?;
    let record = store
        .run(&id)
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| anyhow!("no run {run} in {}", settings.server.database_path))?;
    print!(
        "{}",
        runs::render(
            &record,
            &store.lanes(&id)?,
            &store.findings(&id)?,
            &store.events(&id)?
        )
    );
    Ok(())
}

async fn cmd_review(config: &Path, url: &str, commit: Option<String>) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, None).await?;
    let target = parse_pull_request_url(url)?;
    let commit = commit
        .map(|c| henk_domain::review::CommitSha::parse(&c))
        .transpose()?;
    let requester = app
        .settings
        .planning
        .as_ref()
        .map(|p| p.requester_id.to_string());
    let run = ids::new_run_id();
    println!("run {run}");
    let report = review::run_review(
        &app,
        review::ReviewRequest {
            target,
            commit,
            trigger: "cli".to_owned(),
            requester,
            acknowledge: None,
            run: Some(run.clone()),
        },
        CancellationToken::new(),
    )
    .await
    .with_context(|| format!("run {run}"))?;
    debug_assert_eq!(report.run, run);
    if let Some(summary) = report.summary {
        println!("{summary}");
    }
    if let Some(outcome) = report.outcome {
        println!("check conclusion: {:?}", outcome.check_conclusion());
    }
    Ok(())
}

async fn cmd_plan(config: &Path, url: &str, note: Option<String>) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, None).await?;
    let target = parse_issue_url(url)?;
    let report = plan::run_plan(
        &app,
        plan::PlanRequest {
            target,
            note,
            trigger: "cli".to_owned(),
            run: None,
        },
        CancellationToken::new(),
    )
    .await?;
    println!("run {}", report.run);
    println!("plan written: {}", report.planned);
    for change in report.changes {
        println!("- {change}");
    }
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

async fn cmd_mcp_probe(config: &Path, server: &str, show: &[String]) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let server_config = settings
        .mcp
        .get(server)
        .ok_or_else(|| anyhow!("MCP server {server:?} is not configured"))?;
    let session = henk_mcp::RmcpSession::connect(server, server_config, app::env_var).await?;
    let tools = session.list_tools().await?;
    println!("{} tools from {server}:", tools.len());
    let platform = if server.contains("gitlab") {
        Platform::GitLab
    } else {
        Platform::GitHub
    };
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
