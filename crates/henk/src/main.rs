//! The `henk` binary.
//!
//! Today it loads and checks a configuration file. The surfaces of
//! `docs/SPEC.md` (GitHub, GitLab, Discord, email) are added as subcommands
//! and services on top of this entry point.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod config;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use crate::config::Config;

/// Meneer Henk: the team's AI colleague.
#[derive(Debug, Parser)]
#[command(name = "henk", version, about, long_about = None)]
struct Cli {
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
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Load a configuration file and report what Henk would work with.
    Check {
        /// Path to the TOML configuration file.
        #[arg(long, default_value = "henk.toml")]
        path: PathBuf,
    },
    /// Print an example configuration file.
    Example,
}

fn main() -> ExitCode {
    // Every HTTPS client in the process (model APIs, MCP over HTTP, GitHub)
    // shares rustls with the ring provider. Installing it once here avoids
    // a panic inside the first client that needs a provider. A second
    // install attempt only reports that one is already set.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("henk: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Config {
            command: ConfigCommand::Check { path },
        } => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let config =
                Config::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
            let settings = config.into_settings().context("validating configuration")?;
            println!("{}", settings.describe());
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Example,
        } => {
            print!("{}", config::EXAMPLE);
            Ok(())
        }
    }
}
