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
pub mod registry;
pub mod runtime;
pub mod service;
pub mod thread;
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
    use super::{HeadlessCommand, parse, render_placeholder};
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

    #[test]
    fn parses_every_public_shape_and_rejects_invalid_ids_and_decisions() {
        let id = "01900000-0000-7000-8000-000000000001";
        assert_eq!(
            parse(&args(&["run", "inspect", "the", "workspace"])),
            Ok(HeadlessCommand::Run {
                prompt: "inspect the workspace".into(),
                focus: None,
            })
        );
        assert!(matches!(
            parse(&args(&["resume", id, "--allow"])),
            Ok(HeadlessCommand::Resume { allow: true, .. })
        ));
        assert!(matches!(
            parse(&args(&["resume", id, "--deny"])),
            Ok(HeadlessCommand::Resume { allow: false, .. })
        ));
        assert!(matches!(
            parse(&args(&["show", id])),
            Ok(HeadlessCommand::Show { .. })
        ));
        assert_eq!(parse(&args(&["list"])), Ok(HeadlessCommand::List));
        assert_eq!(parse(&args(&["run"])), Err("run requires a prompt".into()));
        assert_eq!(
            parse(&args(&["show", "not-a-run"])),
            Err("invalid run id".into())
        );
        assert!(parse(&args(&["resume", id, "maybe"])).is_err());
        assert!(parse(&args(&["unknown"])).is_err());
    }

    #[test]
    fn placeholder_text_is_truthful_for_every_command_variant() {
        let root = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let run_id = match parse(&args(&["show", "01900000-0000-7000-8000-000000000001"])).unwrap()
        {
            HeadlessCommand::Show { run_id } => run_id,
            command => panic!("unexpected command: {command:?}"),
        };
        let commands = [
            HeadlessCommand::List,
            HeadlessCommand::Run {
                prompt: "work".into(),
                focus: None,
            },
            HeadlessCommand::Resume {
                run_id,
                allow: true,
            },
            HeadlessCommand::Show { run_id },
        ];
        let rendered = commands
            .iter()
            .map(|command| render_placeholder(command, &engine))
            .collect::<Vec<_>>();
        assert_eq!(rendered.len(), 4);
        assert!(rendered.iter().all(|text| !text.is_empty()));
        assert!(rendered[0].contains("No runs"));
        assert!(rendered[1].contains("not implemented"));
        assert!(rendered[2].contains("resume"));
        assert!(rendered[3].contains("lookup"));
    }
}
