//! Closed built-in command catalog shared by slash input and the command palette.
//!
//! The catalog contains identifiers and metadata only. Execution remains in
//! the TUI reducer or is emitted as a typed [`crate::thread::ThreadUiAction`].

/// Stable identifiers for the first built-in command slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinCommand {
    New,
    Sessions,
    Rename,
    Fork,
    Model,
    Help,
    Navigation,
    Refresh,
    Quit,
}

/// Execution boundary selected by the catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    LocalUi,
    TypedAction,
    PromptTemplate,
}

/// Argument contract for exact slash-command dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentPolicy {
    None,
    Optional,
    Required,
}

/// Secret-free built-in descriptor. No callback or runtime capability can be
/// smuggled through this presentation model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandDescriptor {
    pub id: BuiltinCommand,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
    pub kind: CommandKind,
    pub arguments: ArgumentPolicy,
}

const NO_ALIASES: &[&str] = &[];
const SESSION_ALIASES: &[&str] = &["resume"];

/// One authoritative built-in catalog for palette discovery and slash lookup.
pub const BUILTINS: &[CommandDescriptor] = &[
    CommandDescriptor {
        id: BuiltinCommand::New,
        name: "new",
        aliases: NO_ALIASES,
        description: "Start a new conversation draft",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
    },
    CommandDescriptor {
        id: BuiltinCommand::Sessions,
        name: "sessions",
        aliases: SESSION_ALIASES,
        description: "Find and resume a saved session",
        argument_hint: Some("[query]"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Optional,
    },
    CommandDescriptor {
        id: BuiltinCommand::Model,
        name: "model",
        aliases: NO_ALIASES,
        description: "Select a provider and model",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
    },
    CommandDescriptor {
        id: BuiltinCommand::Help,
        name: "help",
        aliases: NO_ALIASES,
        description: "Show keyboard shortcuts",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
    },
    CommandDescriptor {
        id: BuiltinCommand::Navigation,
        name: "navigation",
        aliases: &["nav"],
        description: "Enter transcript navigation",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
    },
    CommandDescriptor {
        id: BuiltinCommand::Refresh,
        name: "refresh",
        aliases: NO_ALIASES,
        description: "Refresh authoritative session state",
        argument_hint: None,
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::None,
    },
    CommandDescriptor {
        id: BuiltinCommand::Quit,
        name: "quit",
        aliases: &["exit", "q"],
        description: "Quit Latte Code",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
    },
    CommandDescriptor {
        id: BuiltinCommand::Rename,
        name: "rename",
        aliases: NO_ALIASES,
        description: "Rename the current session",
        argument_hint: Some("<title>"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Required,
    },
    CommandDescriptor {
        id: BuiltinCommand::Fork,
        name: "fork",
        aliases: &["branch"],
        description: "Fork committed history into a new session",
        argument_hint: Some("[title]"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Optional,
    },
];

/// Result of exact slash recognition. Unknown and syntactically invalid slash
/// text deliberately falls through to the ordinary prompt path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashResolution {
    Prompt,
    Command {
        descriptor: &'static CommandDescriptor,
        argument: String,
    },
    ValidationError(String),
}

/// Resolves an exact built-in invocation without fuzzy execution or shell
/// parsing. Only a slash in the first byte is eligible.
#[must_use]
pub fn resolve_slash(value: &str) -> SlashResolution {
    let Some(candidate) = value.strip_prefix('/') else {
        return SlashResolution::Prompt;
    };
    let token_end = candidate
        .char_indices()
        .find_map(|(index, value)| value.is_whitespace().then_some(index))
        .unwrap_or(candidate.len());
    let name = &candidate[..token_end];
    if !valid_name(name) {
        return SlashResolution::Prompt;
    }
    let Some(descriptor) = BUILTINS
        .iter()
        .find(|descriptor| descriptor.name == name || descriptor.aliases.contains(&name))
    else {
        return SlashResolution::Prompt;
    };
    let argument = candidate[token_end..].trim().to_owned();
    if descriptor.arguments == ArgumentPolicy::None && !argument.is_empty() {
        return SlashResolution::ValidationError(format!(
            "/{} does not accept arguments",
            descriptor.name
        ));
    }
    if descriptor.arguments == ArgumentPolicy::Required && argument.is_empty() {
        return SlashResolution::ValidationError(format!(
            "/{} requires an argument",
            descriptor.name
        ));
    }
    SlashResolution::Command {
        descriptor,
        argument,
    }
}

/// Returns deterministic built-in suggestions for a single partial slash
/// token. Matching is prefix-only; whitespace, invalid syntax, and ordinary
/// prompt text deliberately disable discovery.
#[must_use]
pub fn slash_suggestions(value: &str) -> Vec<&'static CommandDescriptor> {
    let Some(prefix) = value.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.chars().any(char::is_whitespace)
        || (!prefix.is_empty() && !valid_name(prefix))
        || prefix.len() > 64
    {
        return Vec::new();
    }

    let mut matches = BUILTINS
        .iter()
        .enumerate()
        .filter_map(|(catalog_index, descriptor)| {
            let rank = if descriptor.name == prefix || descriptor.aliases.contains(&prefix) {
                0
            } else if descriptor.name.starts_with(prefix) {
                1
            } else if descriptor
                .aliases
                .iter()
                .any(|alias| alias.starts_with(prefix))
            {
                2
            } else {
                return None;
            };
            Some((rank, catalog_index, descriptor))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(rank, catalog_index, _)| (*rank, *catalog_index));
    matches
        .into_iter()
        .map(|(_, _, descriptor)| descriptor)
        .collect()
}

fn valid_name(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || (index > 0 && matches!(byte, b':' | b'_' | b'-'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_commands_aliases_and_multiline_arguments_share_one_catalog() {
        assert_eq!(
            resolve_slash("/new"),
            SlashResolution::Command {
                descriptor: &BUILTINS[0],
                argument: String::new(),
            }
        );
        assert_eq!(
            resolve_slash("/resume  session-title\nsecond line  "),
            SlashResolution::Command {
                descriptor: &BUILTINS[1],
                argument: "session-title\nsecond line".into(),
            }
        );
    }

    #[test]
    fn unknown_paths_case_mismatches_and_nonleading_slashes_remain_prompts() {
        for value in ["/tmp/file", "/unknown", "/New", " /new", "/", "/bad$name"] {
            assert_eq!(resolve_slash(value), SlashResolution::Prompt, "{value}");
        }
    }

    #[test]
    fn invalid_known_arguments_are_local_validation_errors() {
        assert_eq!(
            resolve_slash("/new unexpected"),
            SlashResolution::ValidationError("/new does not accept arguments".into())
        );
        assert!(matches!(
            resolve_slash("/sessions title"),
            SlashResolution::Command { argument, .. } if argument == "title"
        ));
        assert_eq!(
            resolve_slash("/rename"),
            SlashResolution::ValidationError("/rename requires an argument".into())
        );
    }

    #[test]
    fn catalog_execution_kinds_cover_local_typed_and_future_prompt_paths() {
        assert!(
            BUILTINS
                .iter()
                .any(|item| item.kind == CommandKind::LocalUi)
        );
        assert!(
            BUILTINS
                .iter()
                .any(|item| item.kind == CommandKind::TypedAction)
        );
        assert!(
            !BUILTINS
                .iter()
                .any(|item| item.kind == CommandKind::PromptTemplate)
        );
    }

    #[test]
    fn suggestions_use_prefix_matching_aliases_and_catalog_stability() {
        assert_eq!(slash_suggestions("/"), BUILTINS.iter().collect::<Vec<_>>());
        assert_eq!(
            slash_suggestions("/r")
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![
                BuiltinCommand::Refresh,
                BuiltinCommand::Rename,
                BuiltinCommand::Sessions,
            ]
        );
        assert_eq!(slash_suggestions("/resu"), vec![&BUILTINS[1]]);
        assert_eq!(
            slash_suggestions("/help"),
            vec![
                BUILTINS
                    .iter()
                    .find(|item| item.id == BuiltinCommand::Help)
                    .expect("help command")
            ]
        );
    }

    #[test]
    fn suggestions_ignore_prompt_text_invalid_tokens_and_arguments() {
        for value in [
            "help",
            " /help",
            "/Help",
            "/tmp/file",
            "/help now",
            "/bad$name",
            "/abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklm",
        ] {
            assert!(slash_suggestions(value).is_empty(), "{value}");
        }
    }
}
