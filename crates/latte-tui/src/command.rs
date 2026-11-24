//! Closed built-in command catalog shared by slash input and the command palette.
//!
//! The catalog contains identifiers and metadata only. Execution remains in
//! the TUI reducer or is emitted as a typed [`crate::session::SessionUiAction`].

use latte_core::SessionLifecycle;

/// Stable identifiers for the built-in command catalog.
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
    Compact,
    Cancel,
    Mode,
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

/// Lifecycle concurrency contract declared by each built-in command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConcurrencyPolicy {
    Always,
    SessionRequired,
    IdleOnly,
    RunningOnly,
}

/// Dispatch-time and popup-time availability of a built-in command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAvailability {
    Enabled,
    Disabled { reason: &'static str },
}

impl CommandAvailability {
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Minimal state snapshot needed to evaluate [`command_availability`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CommandContext {
    pub connected: bool,
    pub has_open_session: bool,
    pub lifecycle: Option<SessionLifecycle>,
    pub has_active_turn: bool,
    pub has_pending_request: bool,
    pub has_pending_submission: bool,
    pub has_pending_model_switch: bool,
    pub has_pending_compaction: bool,
    pub has_queued_follow_up: bool,
}

impl CommandContext {
    #[must_use]
    pub const fn session_switch_available(&self) -> bool {
        !self.has_pending_submission
            && !self.has_pending_model_switch
            && !self.has_pending_compaction
            && !self.has_active_turn
            && !self.has_pending_request
            && !matches!(
                self.lifecycle,
                Some(SessionLifecycle::ReconciliationRequired)
            )
    }
}

/// Evaluates whether `command` may execute in `ctx`, returning a bounded
/// user-facing reason when disabled.
#[must_use]
#[allow(clippy::too_many_lines)]
pub const fn command_availability(
    command: BuiltinCommand,
    ctx: &CommandContext,
) -> CommandAvailability {
    match command {
        BuiltinCommand::Help
        | BuiltinCommand::Navigation
        | BuiltinCommand::Refresh
        | BuiltinCommand::Quit
        | BuiltinCommand::Mode => CommandAvailability::Enabled,
        BuiltinCommand::New | BuiltinCommand::Model => {
            if ctx.session_switch_available() {
                CommandAvailability::Enabled
            } else {
                CommandAvailability::Disabled {
                    reason: "Session switching is disabled while work or a request is active",
                }
            }
        }
        BuiltinCommand::Sessions => {
            if !ctx.connected {
                CommandAvailability::Disabled {
                    reason: "Actions are disabled while disconnected",
                }
            } else if !ctx.session_switch_available() {
                CommandAvailability::Disabled {
                    reason: "Session switching is disabled while work or a request is active",
                }
            } else {
                CommandAvailability::Enabled
            }
        }
        BuiltinCommand::Rename | BuiltinCommand::Fork => {
            if !ctx.connected {
                CommandAvailability::Disabled {
                    reason: "Actions are disabled while disconnected",
                }
            } else if !ctx.session_switch_available() {
                CommandAvailability::Disabled {
                    reason: "Session switching is disabled while work or a request is active",
                }
            } else if !ctx.has_open_session {
                CommandAvailability::Disabled {
                    reason: "This command requires an open saved session",
                }
            } else {
                CommandAvailability::Enabled
            }
        }
        BuiltinCommand::Compact => {
            if !ctx.connected {
                CommandAvailability::Disabled {
                    reason: "Actions are disabled while disconnected",
                }
            } else if ctx.has_pending_compaction {
                CommandAvailability::Disabled {
                    reason: "Compaction is already in progress",
                }
            } else if !ctx.has_open_session {
                CommandAvailability::Disabled {
                    reason: "This command requires an open saved session",
                }
            } else if !matches!(ctx.lifecycle, Some(SessionLifecycle::Ready))
                || ctx.has_active_turn
                || ctx.has_pending_request
                || ctx.has_pending_submission
                || ctx.has_pending_model_switch
                || ctx.has_queued_follow_up
            {
                CommandAvailability::Disabled {
                    reason: "Compact requires an idle Ready session",
                }
            } else {
                CommandAvailability::Enabled
            }
        }
        BuiltinCommand::Cancel => {
            if !ctx.connected {
                CommandAvailability::Disabled {
                    reason: "Actions are disabled while disconnected",
                }
            } else if ctx.has_pending_request
                || matches!(
                    ctx.lifecycle,
                    Some(
                        SessionLifecycle::WaitingPermission
                            | SessionLifecycle::WaitingInput
                            | SessionLifecycle::ReconciliationRequired
                    )
                )
            {
                CommandAvailability::Disabled {
                    reason: "Resolve the pending request or press Ctrl+C to cancel",
                }
            } else if !matches!(ctx.lifecycle, Some(SessionLifecycle::Running))
                || !ctx.has_active_turn
            {
                CommandAvailability::Disabled {
                    reason: "No active turn to cancel",
                }
            } else {
                CommandAvailability::Enabled
            }
        }
    }
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
    pub concurrency: ConcurrencyPolicy,
}

const NO_ALIASES: &[&str] = &[];
const SESSION_ALIASES: &[&str] = &["resume"];

/// One authoritative built-in catalog for palette discovery and slash lookup.
pub const BUILTINS: &[CommandDescriptor] = &[
    CommandDescriptor {
        id: BuiltinCommand::New,
        name: "new",
        aliases: NO_ALIASES,
        description: "Start a new session draft",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::IdleOnly,
    },
    CommandDescriptor {
        id: BuiltinCommand::Sessions,
        name: "sessions",
        aliases: SESSION_ALIASES,
        description: "Find and resume a saved session",
        argument_hint: Some("[query]"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Optional,
        concurrency: ConcurrencyPolicy::IdleOnly,
    },
    CommandDescriptor {
        id: BuiltinCommand::Model,
        name: "model",
        aliases: NO_ALIASES,
        description: "Select a provider and model",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::IdleOnly,
    },
    CommandDescriptor {
        id: BuiltinCommand::Help,
        name: "help",
        aliases: NO_ALIASES,
        description: "Show keyboard shortcuts",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::Always,
    },
    CommandDescriptor {
        id: BuiltinCommand::Navigation,
        name: "navigation",
        aliases: &["nav"],
        description: "Enter transcript navigation",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::Always,
    },
    CommandDescriptor {
        id: BuiltinCommand::Refresh,
        name: "refresh",
        aliases: NO_ALIASES,
        description: "Refresh authoritative session state",
        argument_hint: None,
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::Always,
    },
    CommandDescriptor {
        id: BuiltinCommand::Quit,
        name: "quit",
        aliases: &["exit", "q"],
        description: "Quit Latte Code",
        argument_hint: None,
        kind: CommandKind::LocalUi,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::Always,
    },
    CommandDescriptor {
        id: BuiltinCommand::Rename,
        name: "rename",
        aliases: NO_ALIASES,
        description: "Rename the current session",
        argument_hint: Some("<title>"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Required,
        concurrency: ConcurrencyPolicy::SessionRequired,
    },
    CommandDescriptor {
        id: BuiltinCommand::Fork,
        name: "fork",
        aliases: &["branch"],
        description: "Fork committed history into a new session",
        argument_hint: Some("[title]"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Optional,
        concurrency: ConcurrencyPolicy::SessionRequired,
    },
    CommandDescriptor {
        id: BuiltinCommand::Compact,
        name: "compact",
        aliases: NO_ALIASES,
        description: "Compact conversation history",
        argument_hint: None,
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::IdleOnly,
    },
    CommandDescriptor {
        id: BuiltinCommand::Cancel,
        name: "cancel",
        aliases: NO_ALIASES,
        description: "Cancel the active turn",
        argument_hint: None,
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::None,
        concurrency: ConcurrencyPolicy::RunningOnly,
    },
    CommandDescriptor {
        id: BuiltinCommand::Mode,
        name: "mode",
        aliases: &["permissions"],
        description: "Cycle or set permission mode (ask, auto-edit, full-auto)",
        argument_hint: Some("[ask|auto-edit|full-auto]"),
        kind: CommandKind::TypedAction,
        arguments: ArgumentPolicy::Optional,
        concurrency: ConcurrencyPolicy::Always,
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

    #[test]
    fn compact_and_cancel_resolve_reject_arguments_and_rank_in_prefix_suggestions() {
        assert_eq!(
            resolve_slash("/compact"),
            SlashResolution::Command {
                descriptor: &BUILTINS[9],
                argument: String::new(),
            }
        );
        assert_eq!(
            resolve_slash("/cancel"),
            SlashResolution::Command {
                descriptor: &BUILTINS[10],
                argument: String::new(),
            }
        );
        assert_eq!(
            resolve_slash("/compact extra"),
            SlashResolution::ValidationError("/compact does not accept arguments".into())
        );
        assert_eq!(
            resolve_slash("/cancel now"),
            SlashResolution::ValidationError("/cancel does not accept arguments".into())
        );
        assert_eq!(
            slash_suggestions("/c")
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![BuiltinCommand::Compact, BuiltinCommand::Cancel]
        );
        assert_eq!(
            slash_suggestions("/co")
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![BuiltinCommand::Compact]
        );
        assert_eq!(
            slash_suggestions("/ca")
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![BuiltinCommand::Cancel]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn command_availability_enforces_lifecycle_connection_and_busy_gates() {
        let draft = CommandContext {
            connected: true,
            ..CommandContext::default()
        };
        assert!(command_availability(BuiltinCommand::New, &draft).is_enabled());
        assert!(command_availability(BuiltinCommand::Sessions, &draft).is_enabled());
        assert!(command_availability(BuiltinCommand::Model, &draft).is_enabled());
        assert!(command_availability(BuiltinCommand::Help, &draft).is_enabled());
        assert!(command_availability(BuiltinCommand::Navigation, &draft).is_enabled());
        assert!(command_availability(BuiltinCommand::Refresh, &draft).is_enabled());
        assert!(command_availability(BuiltinCommand::Quit, &draft).is_enabled());
        assert_eq!(
            command_availability(BuiltinCommand::Rename, &draft),
            CommandAvailability::Disabled {
                reason: "This command requires an open saved session"
            }
        );
        assert_eq!(
            command_availability(BuiltinCommand::Compact, &draft),
            CommandAvailability::Disabled {
                reason: "This command requires an open saved session"
            }
        );
        assert_eq!(
            command_availability(BuiltinCommand::Cancel, &draft),
            CommandAvailability::Disabled {
                reason: "No active turn to cancel"
            }
        );

        let ready = CommandContext {
            connected: true,
            has_open_session: true,
            lifecycle: Some(SessionLifecycle::Ready),
            ..CommandContext::default()
        };
        assert!(command_availability(BuiltinCommand::Compact, &ready).is_enabled());
        assert!(command_availability(BuiltinCommand::Rename, &ready).is_enabled());
        assert!(command_availability(BuiltinCommand::Fork, &ready).is_enabled());
        assert!(!command_availability(BuiltinCommand::Cancel, &ready).is_enabled());

        let compacting = CommandContext {
            has_pending_compaction: true,
            ..ready
        };
        assert_eq!(
            command_availability(BuiltinCommand::Compact, &compacting),
            CommandAvailability::Disabled {
                reason: "Compaction is already in progress"
            }
        );
        assert!(!command_availability(BuiltinCommand::New, &compacting).is_enabled());

        let queued = CommandContext {
            has_queued_follow_up: true,
            ..ready
        };
        assert_eq!(
            command_availability(BuiltinCommand::Compact, &queued),
            CommandAvailability::Disabled {
                reason: "Compact requires an idle Ready session"
            }
        );

        let running = CommandContext {
            connected: true,
            has_open_session: true,
            lifecycle: Some(SessionLifecycle::Running),
            has_active_turn: true,
            has_queued_follow_up: true,
            has_pending_submission: true,
            ..CommandContext::default()
        };
        assert!(command_availability(BuiltinCommand::Cancel, &running).is_enabled());
        assert_eq!(
            command_availability(BuiltinCommand::Compact, &running),
            CommandAvailability::Disabled {
                reason: "Compact requires an idle Ready session"
            }
        );
        assert_eq!(
            command_availability(BuiltinCommand::Sessions, &running),
            CommandAvailability::Disabled {
                reason: "Session switching is disabled while work or a request is active"
            }
        );
        assert_eq!(
            command_availability(BuiltinCommand::Fork, &running),
            CommandAvailability::Disabled {
                reason: "Session switching is disabled while work or a request is active"
            }
        );

        for parked in [
            SessionLifecycle::WaitingPermission,
            SessionLifecycle::WaitingInput,
            SessionLifecycle::ReconciliationRequired,
        ] {
            let ctx = CommandContext {
                connected: true,
                has_open_session: true,
                lifecycle: Some(parked),
                has_active_turn: parked != SessionLifecycle::ReconciliationRequired,
                has_pending_request: parked != SessionLifecycle::ReconciliationRequired,
                ..CommandContext::default()
            };
            assert_eq!(
                command_availability(BuiltinCommand::Cancel, &ctx),
                CommandAvailability::Disabled {
                    reason: "Resolve the pending request or press Ctrl+C to cancel"
                }
            );
            assert!(!command_availability(BuiltinCommand::Compact, &ctx).is_enabled());
        }

        let disconnected = CommandContext {
            connected: false,
            ..ready
        };
        for cmd in [
            BuiltinCommand::Sessions,
            BuiltinCommand::Rename,
            BuiltinCommand::Fork,
            BuiltinCommand::Compact,
            BuiltinCommand::Cancel,
        ] {
            assert_eq!(
                command_availability(cmd, &disconnected),
                CommandAvailability::Disabled {
                    reason: "Actions are disabled while disconnected"
                }
            );
        }
        assert!(command_availability(BuiltinCommand::Help, &disconnected).is_enabled());
    }
}
