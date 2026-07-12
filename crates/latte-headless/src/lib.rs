//! Non-interactive command shapes and truthful placeholder rendering.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::semicolon_if_nothing_returned
)]
use latte_core::RunId;
use latte_engine::EngineHandle;
use std::path::PathBuf;
pub mod context;
pub mod provider;
pub mod runtime;
pub mod service;
/// Parsed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadlessCommand {
    /// New run.
    Run {
        /// User request.
        prompt: String,
        /// Optional workspace-relative file or directory used to focus context.
        focus: Option<PathBuf>,
    },
    /// Resume.
    Resume { run_id: RunId, allow: bool },
    /// Show.
    Show { run_id: RunId },
    /// List.
    List,
}
/// Parses run/resume/show/list without executing.
///
/// # Errors
/// Returns a usage description for an unknown shape or invalid identifier.
pub fn parse(args: &[String]) -> Result<HeadlessCommand, String> {
    match args {
        [c, p @ ..] if c == "run" => parse_run(p),
        [c, id, decision] if c == "resume" && matches!(decision.as_str(), "--allow" | "--deny") => {
            parse_id(id).map(|run_id| HeadlessCommand::Resume {
                run_id,
                allow: decision == "--allow",
            })
        }
        [c, id] if c == "show" => parse_id(id).map(|run_id| HeadlessCommand::Show { run_id }),
        [c] if c == "list" => Ok(HeadlessCommand::List),
        _ => Err(
            "expected: run [--focus <path>] <prompt> | resume <run-id> (--allow|--deny) | show <run-id> | list"
                .into(),
        ),
    }
}

fn parse_run(args: &[String]) -> Result<HeadlessCommand, String> {
    let (focus, prompt) = match args {
        [flag, path, prompt @ ..] if flag == "--focus" && !path.is_empty() => {
            (Some(PathBuf::from(path)), prompt)
        }
        [flag, ..] if flag == "--focus" => return Err("--focus requires a path".into()),
        prompt => (None, prompt),
    };
    if prompt.is_empty() {
        return Err("run requires a prompt".into());
    }
    if prompt.iter().any(|value| value == "--focus") {
        return Err("--focus must appear immediately after run".into());
    }
    Ok(HeadlessCommand::Run {
        prompt: prompt.join(" "),
        focus,
    })
}
fn parse_id(v: &str) -> Result<RunId, String> {
    uuid::Uuid::parse_str(v)
        .map(RunId::from_uuid)
        .map_err(|_| "invalid run id".into())
}

/// Renders an honest non-operational placeholder.
#[must_use]
pub fn render_placeholder(c: &HeadlessCommand, _engine: &EngineHandle) -> String {
    match c {
        HeadlessCommand::List => "No runs: persistence is not implemented in this core slice.",
        HeadlessCommand::Run { .. } => "Run execution is not implemented in this core slice.",
        HeadlessCommand::Resume { .. } => "Use the runtime executor to resume this run.",
        HeadlessCommand::Show { .. } => "Run lookup is not implemented in this core slice.",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::{HeadlessCommand, parse};
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_run_focus_as_structured_path() {
        assert_eq!(
            parse(&args(&["run", "--focus", "src/lib.rs", "fix", "it"])),
            Ok(HeadlessCommand::Run {
                prompt: "fix it".into(),
                focus: Some(PathBuf::from("src/lib.rs")),
            })
        );
    }

    #[test]
    fn rejects_missing_or_misplaced_focus() {
        assert_eq!(
            parse(&args(&["run", "--focus"])),
            Err("--focus requires a path".into())
        );
        assert_eq!(
            parse(&args(&["run", "fix", "--focus", "src"])),
            Err("--focus must appear immediately after run".into())
        );
    }
}
