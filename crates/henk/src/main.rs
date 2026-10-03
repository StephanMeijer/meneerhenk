//! The `henk` binary.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod app;
mod config;
mod ids;
mod review;
mod review_tools;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, anyhow};
use clap::{Parser, Subcommand};
use henk_domain::allowlist::{Platform, RepoRef};
use henk_llm::{ChatMessage, CompletionRequest};
use henk_mcp::McpSession as _;
use henk_platform::ReviewTarget;
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
    /// Review one pull request now, from the command line.
    Review {
        /// Pull request URL, such as <https://github.com/owner/repo/pull/7>
        url: String,
        /// Review this commit instead of the current head.
        #[arg(long)]
        commit: Option<String>,
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
        Command::Review { url, commit } => cmd_review(&cli.config, &url, commit).await,
        Command::Llm {
            command: LlmCommand::Probe { model, prompt },
        } => cmd_llm_probe(&cli.config, &model, prompt).await,
        Command::Mcp {
            command: McpCommand::Probe { server, show },
        } => cmd_mcp_probe(&cli.config, &server, &show).await,
    }
}

async fn cmd_review(config: &Path, url: &str, commit: Option<String>) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, None)?;
    let target = parse_pull_request_url(url)?;
    let commit = commit
        .map(|c| henk_domain::review::CommitSha::parse(&c))
        .transpose()?;
    let requester = app
        .settings
        .planning
        .as_ref()
        .map(|p| p.requester_id.to_string());
    let report = review::run_review(
        &app,
        review::ReviewRequest {
            target,
            commit,
            trigger: "cli".to_owned(),
            requester,
            acknowledge: None,
        },
        CancellationToken::new(),
    )
    .await?;
    println!("run {}", report.run);
    if let Some(summary) = report.summary {
        println!("{summary}");
    }
    if let Some(outcome) = report.outcome {
        println!("check conclusion: {:?}", outcome.check_conclusion());
    }
    Ok(())
}

async fn cmd_llm_probe(config: &Path, model: &str, prompt: String) -> anyhow::Result<()> {
    let settings = load_settings(config)?;
    let app = App::build(settings, Some(Path::new(":memory:")))?;
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

/// Parses `https://github.com/owner/repo/pull/7`.
fn parse_pull_request_url(url: &str) -> anyhow::Result<ReviewTarget> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .ok_or_else(|| anyhow!("only github.com pull request URLs are supported for now"))?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let (Some(owner), Some(repo), Some("pull"), Some(number)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(anyhow!("expected https://github.com/owner/repo/pull/N"));
    };
    let number: u64 = number.parse().context("pull request number")?;
    Ok(ReviewTarget {
        repo: RepoRef::parse(Platform::GitHub, &format!("{owner}/{repo}"))?,
        number,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn pull_request_urls_are_parsed() {
        let target =
            parse_pull_request_url("https://github.com/StephanMeijer/scratch-repo/pull/12")
                .unwrap();
        assert_eq!(target.repo.path(), "StephanMeijer/scratch-repo");
        assert_eq!(target.number, 12);
        assert!(parse_pull_request_url("https://github.com/a/b/issues/1").is_err());
        assert!(parse_pull_request_url("https://gitlab.com/a/b/-/merge_requests/1").is_err());
    }
}
