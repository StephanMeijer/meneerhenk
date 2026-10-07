//! Transcripts of sessions: what a model was told, what it answered, and
//! every tool call with its result (#191). Every session's transcript is
//! stored with its run ([`json`]); `HENK_TRANSCRIPT_DIR` also writes a
//! local copy as a file, on the machine that ran it.

use std::path::{Path, PathBuf};

use henk_agent::AgentOutcome;
use henk_domain::run::RunId;
use serde::Serialize;

/// The environment variable naming the directory.
pub const ENV_VAR: &str = "HENK_TRANSCRIPT_DIR";

/// The directory from the environment, when set and not empty.
#[must_use]
pub fn directory_from_env() -> Option<PathBuf> {
    std::env::var_os(ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// What a transcript holds, in the store and in a file.
#[derive(Debug, Serialize)]
struct Transcript<'a> {
    run: &'a str,
    session: &'a str,
    model: &'a str,
    stop: String,
    turns: u32,
    usage: henk_llm::Usage,
    system: &'a str,
    messages: &'a [henk_llm::ChatMessage],
}

/// The transcript of one session as compact JSON, as the store keeps it.
///
/// # Errors
///
/// Returns the serialisation error.
pub fn json(
    run: &RunId,
    session: &str,
    model: &str,
    system: &str,
    outcome: &AgentOutcome,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&transcript(run, session, model, system, outcome))
}

fn transcript<'a>(
    run: &'a RunId,
    session: &'a str,
    model: &'a str,
    system: &'a str,
    outcome: &'a AgentOutcome,
) -> Transcript<'a> {
    Transcript {
        run: run.as_str(),
        session,
        model,
        stop: format!("{:?}", outcome.stop),
        turns: outcome.turns,
        usage: outcome.usage,
        system,
        messages: &outcome.messages,
    }
}

/// Writes `<dir>/<run>/<session>.json` and returns its path.
///
/// # Errors
///
/// Returns the I/O or serialisation error.
pub fn write(
    dir: &Path,
    run: &RunId,
    session: &str,
    model: &str,
    system: &str,
    outcome: &AgentOutcome,
) -> Result<PathBuf, std::io::Error> {
    let run_dir = dir.join(run.as_str());
    std::fs::create_dir_all(&run_dir)?;
    let path = run_dir.join(format!("{}.json", file_safe(session)));
    let text = serde_json::to_string_pretty(&transcript(run, session, model, system, outcome))
        .map_err(std::io::Error::other)?;
    std::fs::write(&path, text)?;
    Ok(path)
}

/// Lane names are configuration, but a file name must not be a path.
fn file_safe(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "session".to_owned()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_agent::StopCause;
    use henk_llm::{ChatMessage, Usage};

    use super::*;

    #[test]
    fn writes_one_json_file_per_session_under_the_run() {
        let dir = std::env::temp_dir().join(format!(
            "henk-transcript-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let outcome = AgentOutcome {
            final_text: "done".into(),
            turns: 2,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Usage::default()
            },
            stop: StopCause::EndTurn,
            messages: vec![ChatMessage::user("go"), ChatMessage::assistant("done")],
            repeats: Vec::new(),
        };
        let run = RunId::parse("r-1").unwrap();
        let path = write(&dir, &run, "lane a/../x", "m", "system prompt", &outcome).unwrap();
        assert_eq!(path, dir.join("r-1").join("lane_a____x.json"));
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["stop"], "EndTurn");
        assert_eq!(value["turns"], 2);
        assert_eq!(value["system"], "system prompt");
        assert_eq!(value["messages"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_names_are_sanitised() {
        assert_eq!(file_safe("lane-a"), "lane-a");
        assert_eq!(file_safe("../etc"), "___etc");
        assert_eq!(file_safe(""), "session");
    }
}
