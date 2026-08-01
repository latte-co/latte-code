#![allow(clippy::semicolon_if_nothing_returned)]
use latte_core::{
    FailureCode, IdSource, RunState, RunStatus, ThreadId, ThreadProviderBindingV2, wall_time_ms,
};
use latte_engine::{EngineBuilder, StorageError};
use latte_headless::{
    HeadlessCommand,
    registry::ProviderRegistry,
    runtime::{AgentRuntime, RuntimeError, VerificationPlan},
    thread::{ThreadHistoryPolicy, ThreadRuntimeService},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

const JSON_VERSION: u8 = 1;
const EXIT_COMPLETED: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_FOUND: i32 = 4;
const EXIT_WAITING: i32 = 10;
const EXIT_DENIED: i32 = 11;
const EXIT_INTERNAL: i32 = 70;
const EXIT_INTERRUPTED: i32 = 130;
const CONFIG_RELATIVE_PATH: &str = ".latte/latte-code.jsonc";
const DEFAULT_CONFIG: &str = r#"{
  version: 1,
  thread: {
    max_request_bytes: 524288,
    max_input_bytes: 393216,
    reserved_output_bytes: 131072,
    context_cap_bytes: 65536,
  },
  database: { path: ".latte/latte-code.db" },
  verification: {
    argv: ["cargo", "test", "--workspace"],
    cwd: ".",
    timeout_ms: 120000,
  },
  providers: {},
}"#;
const HELP: &str = "Latte Code agent\n\nUsage:\n  latte-code tui\n  latte-code [--json] run [--focus <path>] <prompt>\n  latte-code [--json] resume <run-id> (--allow|--deny)\n  latte-code [--json] show <run-id>\n  latte-code [--json] list\n  latte-code [--json] --help\n\nLatte Code merges built-in application defaults, $HOME/.latte/latte-code.jsonc, then workspace .latte/latte-code.jsonc; later values win. Configure the global default_model and at least one Provider model explicitly. Relative database.path values are resolved from the workspace root; absolute paths are supported. Provider credentials are environment references in those files.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub version: u32,
    #[serde(default)]
    pub default_model: String,
    pub providers: serde_json::Value,
    #[serde(default)]
    pub database: DatabaseConfig,
    pub verification: VerificationConfig,
    #[serde(default)]
    pub thread: ThreadConfig,
}

/// Limits for v2 transcript-history construction. Values are bytes and the
/// reserved output is subtracted before provider history can be sent.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadConfig {
    pub max_request_bytes: usize,
    pub max_input_bytes: usize,
    pub reserved_output_bytes: usize,
    pub context_cap_bytes: usize,
}
impl Default for ThreadConfig {
    fn default() -> Self {
        let defaults = ThreadHistoryPolicy::default();
        Self {
            max_request_bytes: defaults.max_request_bytes,
            max_input_bytes: defaults.max_input_bytes,
            reserved_output_bytes: defaults.reserved_output_bytes,
            context_cap_bytes: defaults.context_cap_bytes,
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub path: String,
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: ".latte/latte-code.db".into(),
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationConfig {
    pub argv: Vec<String>,
    #[serde(default = "dot")]
    pub cwd: String,
    #[serde(default = "verify_timeout")]
    pub timeout_ms: u64,
}
fn dot() -> String {
    ".".into()
}
fn verify_timeout() -> u64 {
    120_000
}
impl AppConfig {
    /// Loads and validates the layered application and provider configuration.
    ///
    /// Built-in defaults are overlaid by the optional user configuration and
    /// then the optional workspace configuration. Object keys merge
    /// recursively; arrays and scalar values in the later layer replace the
    /// earlier value.
    ///
    /// # Errors
    /// Returns an actionable, secret-safe message when a present layer cannot
    /// be read or when the merged result is invalid. Missing optional files are
    /// ignored.
    pub fn load(root: &Path) -> Result<(Self, ProviderRegistry), String> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::load_with_home(root, home.as_deref())
    }

    fn load_with_home(
        root: &Path,
        home: Option<&Path>,
    ) -> Result<(Self, ProviderRegistry), String> {
        let mut merged: Value = json5::from_str(DEFAULT_CONFIG)
            .map_err(|error| format!("invalid built-in configuration: {error}"))?;
        if let Some(home) = home {
            merge_optional_config(&mut merged, &home.join(CONFIG_RELATIVE_PATH))?;
        }
        merge_optional_config(&mut merged, &root.join(CONFIG_RELATIVE_PATH))?;

        let config: Self = serde_json::from_value(merged.clone())
            .map_err(|error| format!("invalid merged configuration: {error}"))?;
        if config.verification.argv.is_empty() {
            return Err("verification.argv must not be empty".into());
        }
        if config.database.path.trim().is_empty() {
            return Err("database.path must not be empty".into());
        }
        ThreadHistoryPolicy {
            max_request_bytes: config.thread.max_request_bytes,
            max_input_bytes: config.thread.max_input_bytes,
            reserved_output_bytes: config.thread.reserved_output_bytes,
            context_cap_bytes: config.thread.context_cap_bytes,
        }
        .validate()
        .map_err(|error| format!("invalid thread configuration: {error}"))?;
        let merged_text = serde_json::to_string(&merged)
            .map_err(|error| format!("cannot serialize merged configuration: {error}"))?;
        let registry = ProviderRegistry::parse_jsonc(&merged_text).map_err(|e| e.to_string())?;
        Ok((config, registry))
    }
    fn database_path(&self, root: &Path) -> PathBuf {
        let path = Path::new(&self.database.path);
        if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        }
    }
    fn plan(&self) -> VerificationPlan {
        VerificationPlan {
            argv: self.verification.argv.clone(),
            cwd: self.verification.cwd.clone(),
            timeout_ms: self.verification.timeout_ms,
            grace_ms: 250,
            stdout_cap: 16 * 1024,
            stderr_cap: 16 * 1024,
        }
    }
    fn thread_policy(&self) -> ThreadHistoryPolicy {
        ThreadHistoryPolicy {
            max_request_bytes: self.thread.max_request_bytes,
            max_input_bytes: self.thread.max_input_bytes,
            reserved_output_bytes: self.thread.reserved_output_bytes,
            context_cap_bytes: self.thread.context_cap_bytes,
        }
    }
}

fn merge_optional_config(base: &mut Value, path: &Path) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let overlay: Value = json5::from_str(&text)
        .map_err(|error| format!("invalid JSONC {}: {error}", path.display()))?;
    if !overlay.is_object() {
        return Err(format!(
            "invalid JSONC {}: top-level configuration must be an object",
            path.display()
        ));
    }
    merge_value(base, overlay);
    Ok(())
}

fn merge_value(base: &mut Value, overlay: Value) {
    merge_value_at(base, overlay, &mut Vec::new());
}

fn merge_value_at(base: &mut Value, overlay: Value, path: &mut Vec<String>) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                // A model object is one Provider's complete picker catalog.
                // Treat it as an atomic value so a later configuration layer
                // cannot inherit stale models from an earlier layer.
                if key == "models" && path.len() == 2 && path[0] == "providers" {
                    base.insert(key, value);
                    continue;
                }
                if let Some(existing) = base.get_mut(&key) {
                    path.push(key);
                    merge_value_at(existing, value, path);
                    path.pop();
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn discover_workspace_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .unwrap_or(start)
        .to_owned()
}

fn workspace_display_path(root: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    workspace_display_path_with_home(root, home.as_deref())
}

fn workspace_display_path_with_home(root: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return root.display().to_string();
    };
    let Ok(relative) = root.strip_prefix(home) else {
        return root.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".into()
    } else {
        format!("~/{}", relative.display())
    }
}

fn workspace_identity(root: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "workspace root is not valid UTF-8".into())
}

struct ThreadEngineProjection {
    engine: latte_engine::EngineHandle,
    subscription: latte_engine::ThreadSubscription,
    workspace_root: String,
}
impl latte_tui::thread::ThreadProjectionClient for ThreadEngineProjection {
    fn snapshots(&mut self) -> Result<Vec<latte_core::ThreadSnapshot>, String> {
        self.engine
            .list_threads_v2_for_workspace(&self.workspace_root)
            .map_err(|error| error.to_string())
    }
    fn session_catalog(&mut self) -> Result<Vec<latte_core::ThreadSessionSummary>, String> {
        self.engine
            .list_thread_sessions_v2_for_workspace(&self.workspace_root, 200)
            .map_err(|error| error.to_string())
    }
    fn exact_session_catalog(
        &mut self,
        query: &str,
    ) -> Result<Vec<latte_core::ThreadSessionSummary>, String> {
        if let Ok(thread_id) =
            serde_json::from_value::<ThreadId>(serde_json::Value::String(query.into()))
        {
            let Some(metadata) = self
                .engine
                .thread_session_v2(thread_id)
                .map_err(|error| error.to_string())?
            else {
                return Ok(Vec::new());
            };
            if metadata.workspace_root != self.workspace_root {
                return Err(format!(
                    "session {thread_id} belongs to another workspace; explicit rebinding is required"
                ));
            }
            return Ok(vec![metadata]);
        }
        self.engine
            .find_thread_sessions_v2_by_exact_title_for_workspace(&self.workspace_root, query, 200)
            .map_err(|error| error.to_string())
    }
    fn session(&mut self, thread_id: ThreadId) -> Result<latte_core::ThreadSnapshot, String> {
        let metadata = self
            .engine
            .thread_session_v2(thread_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("session {thread_id} was not found"))?;
        if metadata.workspace_root != self.workspace_root {
            return Err(format!(
                "session {thread_id} belongs to another workspace; explicit rebinding is required"
            ));
        }
        self.engine
            .thread_snapshot_tail_v2(thread_id, 500)
            .map_err(|error| error.to_string())
    }
    fn exact_session(&mut self, query: &str) -> Result<Option<latte_core::ThreadSnapshot>, String> {
        let matches = self.exact_session_catalog(query)?;
        let [metadata] = matches.as_slice() else {
            return Ok(None);
        };
        self.engine
            .thread_snapshot_tail_v2(metadata.thread_id, 500)
            .map(Some)
            .map_err(|error| error.to_string())
    }
    fn poll(&mut self) -> latte_tui::thread::ThreadProjectionPoll {
        match self.subscription.try_recv() {
            Ok(Some(_)) => latte_tui::thread::ThreadProjectionPoll::Event,
            Ok(None) => latte_tui::thread::ThreadProjectionPoll::Empty,
            Err(latte_engine::SubscriptionError::Lagged(count)) => {
                latte_tui::thread::ThreadProjectionPoll::Lagged(count)
            }
            Err(latte_engine::SubscriptionError::Closed) => {
                latte_tui::thread::ThreadProjectionPoll::Closed
            }
        }
    }
}

/// Executes the process-level CLI and returns its documented exit code.
///
/// Keeping orchestration in the library lets process-level integration tests
/// exercise the exact controller used by the binary.
pub async fn run_cli() -> i32 {
    execute().await
}

#[allow(clippy::too_many_lines)]
async fn execute() -> i32 {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.first().is_some_and(|value| value == "--json");
    if json {
        args.remove(0);
    }
    let implicit_tui = args.is_empty()
        && !json
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    if implicit_tui || matches!(args.as_slice(), [command] if command == "tui") {
        return execute_tui();
    }
    if args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("--help" | "-h" | "help")
        )
    {
        if json {
            emit_data("completed", &json!({ "help": HELP }));
        } else {
            println!("{HELP}");
        }
        return EXIT_COMPLETED;
    }

    let command = match latte_headless::parse(&args) {
        Ok(command) => command,
        Err(error) => return emit_error(json, "usage", "usage", &error, EXIT_USAGE, true),
    };
    let root = match std::env::current_dir() {
        Ok(root) => discover_workspace_root(&root),
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "current_directory",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };
    let (config, registry) = match AppConfig::load(&root) {
        Ok(value) => value,
        Err(error) => {
            return emit_error(json, "usage", "configuration", &error, EXIT_USAGE, false);
        }
    };
    let database_path = config.database_path(&root);
    let Some(database_parent) = database_path.parent() else {
        return emit_error(
            json,
            "usage",
            "configuration",
            "database.path must have a parent directory",
            EXIT_USAGE,
            false,
        );
    };
    if let Err(error) = std::fs::create_dir_all(database_parent) {
        return emit_error(
            json,
            "internal",
            "database_directory",
            &format!("cannot create {}: {error}", database_parent.display()),
            EXIT_INTERNAL,
            false,
        );
    }
    let engine = match EngineBuilder::new()
        .workspace_root(&root)
        .database_path(database_path)
        .build()
    {
        Ok(engine) => engine,
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "engine_initialization",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };

    match command {
        HeadlessCommand::Show { run_id } => match engine.show(run_id) {
            Ok(run) => render_run(&run, json),
            Err(StorageError::RunNotFound(_)) => emit_error(
                json,
                "not_found",
                "run_not_found",
                &format!("run {run_id} was not found"),
                EXIT_NOT_FOUND,
                false,
            ),
            Err(error) => emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            ),
        },
        HeadlessCommand::List => match engine.list() {
            Ok(runs) => {
                if json {
                    emit_data("completed", &json!({ "runs": runs }));
                } else {
                    for run in runs {
                        println!("{}\t{:?}\trev {}", run.run_id, run.status, run.revision)
                    }
                }
                EXIT_COMPLETED
            }
            Err(error) => emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            ),
        },
        HeadlessCommand::Resume {
            run_id,
            allow: false,
        } => deny_headless(&engine, run_id, json),
        command @ (HeadlessCommand::Run { .. } | HeadlessCommand::Resume { .. }) => {
            let provider = match registry.resolve_default(&engine.tool_descriptors()) {
                Ok(value) => value,
                Err(error) => {
                    return emit_error(
                        json,
                        "usage",
                        "configuration",
                        &error.to_string(),
                        EXIT_USAGE,
                        false,
                    );
                }
            };
            let runtime = AgentRuntime::from_bound_provider(
                engine,
                provider.provider,
                provider.binding,
                &root,
                config.plan(),
            );
            let result = match command {
                HeadlessCommand::Run { prompt, focus } => {
                    runtime.run_with_focus(&prompt, focus.as_deref()).await
                }
                HeadlessCommand::Resume { run_id, allow } => runtime.resume(run_id, allow).await,
                _ => unreachable!(),
            };
            match result {
                Ok(run) => render_run(&run, json),
                Err(RuntimeError::PermissionRequired { run_id }) => emit_error(
                    json,
                    "waiting",
                    "permission_required",
                    &format!("permission required for run {run_id}"),
                    EXIT_WAITING,
                    false,
                ),
                Err(error) => emit_error(
                    json,
                    "failed",
                    "runtime",
                    &error.to_string(),
                    EXIT_FAILED,
                    false,
                ),
            }
        }
    }
}

fn deny_headless(
    engine: &latte_engine::EngineHandle,
    run_id: latte_core::RunId,
    json: bool,
) -> i32 {
    let state = match engine.show(run_id) {
        Ok(state) => state,
        Err(StorageError::RunNotFound(_)) => {
            return emit_error(
                json,
                "not_found",
                "run_not_found",
                &format!("run {run_id} was not found"),
                EXIT_NOT_FOUND,
                false,
            );
        }
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };
    if state.status != RunStatus::WaitingPermission || state.pending_permission.is_none() {
        return emit_error(
            json,
            "failed",
            "invalid_state",
            "run is not waiting for permission",
            EXIT_FAILED,
            false,
        );
    }
    let now = wall_time_ms();
    let lease = match engine.acquire_run_lease(run_id, &format!("agent-{run_id}"), now, 60_000) {
        Ok(lease) => lease,
        Err(error) => {
            return emit_error(
                json,
                "internal",
                "storage",
                &error.to_string(),
                EXIT_INTERNAL,
                false,
            );
        }
    };
    let denied = engine.deny_waiting_permission(run_id, state.revision, &lease, now);
    let _ = engine.release_lease(&lease);
    match denied {
        Ok(run) => render_run(&run, json),
        Err(error) => emit_error(
            json,
            "failed",
            "storage",
            &error.to_string(),
            EXIT_FAILED,
            false,
        ),
    }
}

/// Executes the single v2 reconciliation command exposed by the transcript
/// TUI. Keeping this adapter small and synchronous makes the CLI boundary
/// testable while the actual mutation remains in `ThreadRuntimeService` and
/// its exact engine-owned fenced commit path.
fn reconcile_thread_action(
    service: &ThreadRuntimeService,
    thread_id: ThreadId,
    effect_id: &str,
) -> Result<String, String> {
    service
        .reconcile_unknown_effect(thread_id, effect_id)
        .map(|_| "unknown effect acknowledged; child aborted".into())
        .map_err(|error| error.to_string())
}

/// Transcript-first interactive entrypoint. The legacy v1 CLI remains intact;
/// the TUI reads only v2 snapshots and submits v2 conversation requests.
#[allow(clippy::too_many_lines)]
fn execute_tui() -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "interactive TUI requires a TTY; use --json list/show/run/resume for non-interactive use"
        );
        return EXIT_USAGE;
    }
    let root = match std::env::current_dir() {
        Ok(root) => discover_workspace_root(&root),
        Err(error) => {
            eprintln!("{error}");
            return EXIT_INTERNAL;
        }
    };
    let (config, registry) = match AppConfig::load(&root) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("configuration: {error}");
            return EXIT_USAGE;
        }
    };
    let database_path = config.database_path(&root);
    let Some(parent) = database_path.parent() else {
        eprintln!("configuration: database.path must have a parent directory");
        return EXIT_USAGE;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        eprintln!("cannot create {}: {error}", parent.display());
        return EXIT_INTERNAL;
    }
    let engine = match EngineBuilder::new()
        .workspace_root(&root)
        .database_path(database_path)
        .build()
    {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_INTERNAL;
        }
    };
    let startup_binding = match registry.thread_binding_for_default(&engine.tool_descriptors()) {
        Ok(binding) => binding,
        Err(error) => {
            eprintln!("configuration: {error}");
            return EXIT_USAGE;
        }
    };
    let startup = latte_tui::thread::ThreadStartupPresentation {
        default_provider: startup_binding.provider_name.clone(),
        default_model: startup_binding.model.clone(),
        model_catalog: registry
            .model_catalog()
            .into_iter()
            .map(|entry| latte_tui::thread::ThreadModelOption {
                provider_name: entry.provider_name,
                model: entry.model,
                name: entry.name,
                is_default: entry.is_default,
            })
            .collect(),
        workspace_display: workspace_display_path(&root),
        permission_mode: latte_tui::thread::ThreadPermissionMode::Ask,
    };
    let resolver_registry = registry.clone();
    let resolver_engine = engine.clone();
    let factory: latte_headless::thread::ThreadProviderFactory =
        std::sync::Arc::new(move |binding: &ThreadProviderBindingV2| {
            resolver_registry
                .resolve_thread_bound(binding, &resolver_engine.tool_descriptors())
                .map_err(|error| error.to_string())
        });
    let (progress_tx, progress_rx) =
        std::sync::mpsc::channel::<latte_core::ThreadTransientProgress>();
    let progress_sink: std::sync::Arc<dyn latte_headless::thread::ThreadProgressSink> =
        std::sync::Arc::new(move |progress| {
            let _ = progress_tx.send(progress);
        });
    let service = ThreadRuntimeService::new(engine.clone(), &root, config.thread_policy(), factory)
        .with_progress_sink(progress_sink)
        .with_verification(config.plan());
    let mut projection = ThreadEngineProjection {
        engine: engine.clone(),
        subscription: engine.subscribe_threads(),
        workspace_root: match workspace_identity(&root) {
            Ok(identity) => identity,
            Err(error) => {
                eprintln!("configuration: {error}");
                return EXIT_USAGE;
            }
        },
    };
    let (feedback_tx, feedback_rx) =
        std::sync::mpsc::channel::<latte_tui::thread::ThreadUiFeedback>();
    let action_registry = registry.clone();
    match latte_tui::thread::run_with_feedback_and_progress(
        &mut projection,
        startup,
        move |action| {
            match action {
                latte_tui::thread::ThreadUiAction::Start {
                    submission_id,
                    prompt,
                } => {
                    let binding = startup_binding.clone();
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    let thread_id =
                        ThreadId::from_uuid(latte_core::SystemIdSource::default().next_uuid_v7());
                    let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::assigned(
                        submission_id,
                        thread_id,
                    ));
                    tokio::spawn(async move {
                        let result = service
                            .start(thread_id, prompt, binding)
                            .await
                            .map(|_| "conversation completed".into())
                            .map_err(|error| error.to_string());
                        let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::submission(
                            submission_id,
                            result,
                        ));
                    });
                }
                latte_tui::thread::ThreadUiAction::StartWithModel {
                    submission_id,
                    prompt,
                    provider_name,
                    model,
                } => {
                    let binding = match action_registry.thread_binding_for_model(
                        &provider_name,
                        &model,
                        &engine.tool_descriptors(),
                    ) {
                        Ok(binding) => binding,
                        Err(error) => {
                            let _ =
                                feedback_tx.send(latte_tui::thread::ThreadUiFeedback::submission(
                                    submission_id,
                                    Err(error.to_string()),
                                ));
                            return Ok(());
                        }
                    };
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    let thread_id =
                        ThreadId::from_uuid(latte_core::SystemIdSource::default().next_uuid_v7());
                    let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::assigned(
                        submission_id,
                        thread_id,
                    ));
                    tokio::spawn(async move {
                        let result = service
                            .start(thread_id, prompt, binding)
                            .await
                            .map(|_| "conversation completed".into())
                            .map_err(|error| error.to_string());
                        let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::submission(
                            submission_id,
                            result,
                        ));
                    });
                }
                latte_tui::thread::ThreadUiAction::FollowUp {
                    submission_id,
                    thread_id,
                    expected_thread_revision,
                    prompt,
                } => {
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    tokio::spawn(async move {
                        let result = service
                            .follow_up(thread_id, expected_thread_revision, prompt)
                            .await
                            .map(|_| "follow-up completed".into())
                            .map_err(|error| error.to_string());
                        let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::submission(
                            submission_id,
                            result,
                        ));
                    });
                }
                latte_tui::thread::ThreadUiAction::Cancel { thread_id } => {
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = service
                            .cancel_durable(thread_id)
                            .map(|_| "interruption requested".into())
                            .map_err(|error| error.to_string());
                        let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::command(result));
                    });
                }
                latte_tui::thread::ThreadUiAction::ProvideInput {
                    submission_id,
                    thread_id,
                    request_id,
                    value,
                } => {
                    let snapshot = engine
                        .thread_snapshot_v2(thread_id, None, 1)
                        .map_err(|error| error.to_string());
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    match snapshot {
                        Ok(snapshot) => {
                            tokio::spawn(async move {
                                let result = service
                                    .provide_input(thread_id, snapshot.revision, request_id, value)
                                    .await
                                    .map(|_| "input accepted".into())
                                    .map_err(|error| error.to_string());
                                let _ = feedback.send(
                                    latte_tui::thread::ThreadUiFeedback::input_submission(
                                        submission_id,
                                        result,
                                    ),
                                );
                            });
                        }
                        Err(error) => {
                            let _ = feedback.send(
                                latte_tui::thread::ThreadUiFeedback::input_submission(
                                    submission_id,
                                    Err(error),
                                ),
                            );
                        }
                    }
                }
                latte_tui::thread::ThreadUiAction::ResolvePermission {
                    thread_id,
                    request_id,
                    allow,
                } => {
                    let snapshot = engine
                        .thread_snapshot_v2(thread_id, None, 1)
                        .map_err(|error| error.to_string());
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    match snapshot {
                        Ok(snapshot) => {
                            tokio::spawn(async move {
                                let result = service
                                    .resolve_permission(
                                        thread_id,
                                        snapshot.revision,
                                        request_id,
                                        allow,
                                    )
                                    .await
                                    .map(|_| {
                                        if allow {
                                            "permission allowed".into()
                                        } else {
                                            "permission denied".into()
                                        }
                                    })
                                    .map_err(|error| error.to_string());
                                let _ = feedback
                                    .send(latte_tui::thread::ThreadUiFeedback::command(result));
                            });
                        }
                        Err(error) => {
                            let _ = feedback_tx
                                .send(latte_tui::thread::ThreadUiFeedback::command(Err(error)));
                        }
                    }
                }
                latte_tui::thread::ThreadUiAction::ReconcileUnknown {
                    thread_id,
                    effect_id,
                } => {
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        // The reducer has already required Ctrl+R followed by
                        // Ctrl+A. The service re-loads the authoritative
                        // snapshot and uses the exact v2 fenced reconciliation
                        // path; its emitted thread event refreshes the TUI
                        // projection after the terminal transition.
                        let result = reconcile_thread_action(&service, thread_id, &effect_id);
                        let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::command(result));
                    });
                }
                latte_tui::thread::ThreadUiAction::SwitchModel {
                    switch_id,
                    thread_id,
                    expected_thread_revision,
                    provider_name,
                    model,
                } => {
                    let binding = action_registry
                        .thread_binding_for_model(
                            &provider_name,
                            &model,
                            &engine.tool_descriptors(),
                        )
                        .map_err(|error| error.to_string());
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = binding.and_then(|binding| {
                            service
                                .switch_model(thread_id, expected_thread_revision, &binding)
                                .map(|snapshot| {
                                    format!(
                                        "Model switched to {}/{}",
                                        snapshot.binding.provider_name, snapshot.binding.model
                                    )
                                })
                                .map_err(|error| error.to_string())
                        });
                        let _ = feedback.send(latte_tui::thread::ThreadUiFeedback::model_switch(
                            switch_id, result,
                        ));
                    });
                }
                latte_tui::thread::ThreadUiAction::RefreshSnapshots
                | latte_tui::thread::ThreadUiAction::ShowSessions { .. }
                | latte_tui::thread::ThreadUiAction::OpenSession { .. }
                | latte_tui::thread::ThreadUiAction::Quit => {}
            }
            Ok(())
        },
        &feedback_rx,
        &progress_rx,
    ) {
        Ok(()) => EXIT_COMPLETED,
        Err(error) => {
            eprintln!("{error}");
            if matches!(error, latte_tui::TuiError::Interrupted) {
                EXIT_INTERRUPTED
            } else if matches!(error, latte_tui::TuiError::NonTty) {
                EXIT_USAGE
            } else {
                EXIT_INTERNAL
            }
        }
    }
}

fn render_run(run: &RunState, json_output: bool) -> i32 {
    let (status, code) = outcome(run);
    if json_output {
        emit_data(status, &json!({ "run": run }));
    } else {
        println!(
            "run {}: {:?} (revision {})",
            run.run_id, run.status, run.revision
        );
        if let Some(handoff) = &run.handoff {
            println!("{}", handoff.summary)
        }
    }
    code
}

fn outcome(run: &RunState) -> (&'static str, i32) {
    match run.status {
        RunStatus::Completed => ("completed", EXIT_COMPLETED),
        RunStatus::WaitingPermission | RunStatus::WaitingInput => ("waiting", EXIT_WAITING),
        RunStatus::Interrupted | RunStatus::Cancelling => ("interrupted", EXIT_INTERRUPTED),
        RunStatus::Failed
            if run
                .failure
                .as_ref()
                .is_some_and(|failure| failure.code == FailureCode::PermissionDenied) =>
        {
            ("denied", EXIT_DENIED)
        }
        RunStatus::Failed => ("failed", EXIT_FAILED),
        RunStatus::Queued | RunStatus::Running => ("internal", EXIT_INTERNAL),
    }
}

fn emit_data(status: &str, data: &Value) {
    println!(
        "{}",
        json!({ "version": JSON_VERSION, "status": status, "data": data })
    );
}

fn emit_error(
    json_output: bool,
    status: &str,
    code: &str,
    message: &str,
    exit: i32,
    show_help: bool,
) -> i32 {
    if json_output {
        println!(
            "{}",
            json!({
                "version": JSON_VERSION,
                "status": status,
                "error": { "code": code, "message": message }
            })
        );
    } else if show_help {
        eprintln!("{message}\n\n{HELP}");
    } else {
        eprintln!("{message}");
    }
    exit
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, DatabaseConfig, EXIT_COMPLETED, EXIT_DENIED, EXIT_FAILED, EXIT_INTERNAL,
        EXIT_INTERRUPTED, EXIT_NOT_FOUND, EXIT_USAGE, EXIT_WAITING, ThreadConfig,
        ThreadEngineProjection, VerificationConfig, deny_headless, discover_workspace_root, dot,
        emit_data, emit_error, execute_tui, merge_optional_config, merge_value, outcome,
        reconcile_thread_action, render_run, verify_timeout, workspace_display_path,
        workspace_display_path_with_home, workspace_identity,
    };
    use latte_core::{
        Evidence, FailureCode, Handoff, IdSource, Retryability, RunFailure, RunId, RunState,
        RunStatus, SystemIdSource, ThreadCommandId, ThreadId, ThreadLifecycle,
        ThreadProviderBindingV2, VerificationStatus,
    };
    use latte_engine::{
        CommitThreadRunUpdate, EngineBuilder, ThreadCommitRequest, ThreadEffectDescriptor,
        ThreadEffectRequest, ThreadEffectStartRequest,
    };
    use latte_headless::{
        provider::{FakeProvider, ProviderResponse, ProviderUsage, ToolCall},
        runtime::{AgentRuntime, RuntimeError},
        thread::{ThreadHistoryPolicy, ThreadRuntimeService},
    };
    use latte_tui::thread::{ThreadProjectionClient, ThreadProjectionPoll};
    use serde_json::json;
    use std::{path::Path, sync::Arc};

    fn state(status: RunStatus) -> RunState {
        let mut state =
            RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        state.status = status;
        state
    }

    #[test]
    fn run_statuses_have_stable_exit_codes() {
        assert_eq!(outcome(&state(RunStatus::Completed)).1, EXIT_COMPLETED);
        assert_eq!(
            outcome(&state(RunStatus::WaitingPermission)).1,
            EXIT_WAITING
        );
        assert_eq!(outcome(&state(RunStatus::Failed)).1, EXIT_FAILED);
        assert_eq!(outcome(&state(RunStatus::Interrupted)).1, EXIT_INTERRUPTED);
    }

    #[test]
    fn permission_denial_has_distinct_exit_code() {
        let mut run = state(RunStatus::Failed);
        run.failure = Some(RunFailure {
            code: FailureCode::PermissionDenied,
            message: "denied".into(),
            retryability: Retryability::Terminal,
        });
        assert_eq!(outcome(&run).1, EXIT_DENIED);
    }

    #[test]
    fn remaining_run_statuses_and_config_value_objects_are_exact() {
        assert_eq!(outcome(&state(RunStatus::WaitingInput)).1, EXIT_WAITING);
        assert_eq!(outcome(&state(RunStatus::Cancelling)).1, EXIT_INTERRUPTED);
        assert_eq!(outcome(&state(RunStatus::Queued)).1, EXIT_INTERNAL);
        assert_eq!(outcome(&state(RunStatus::Running)).1, EXIT_INTERNAL);

        let threads = ThreadConfig::default();
        let policy = ThreadHistoryPolicy::default();
        assert_eq!(threads.max_request_bytes, policy.max_request_bytes);
        assert_eq!(threads.max_input_bytes, policy.max_input_bytes);
        assert_eq!(threads.reserved_output_bytes, policy.reserved_output_bytes);
        assert_eq!(threads.context_cap_bytes, policy.context_cap_bytes);
        assert_eq!(DatabaseConfig::default().path, ".latte/latte-code.db");

        let config = AppConfig {
            version: 1,
            default_model: "primary/test".into(),
            providers: json!({}),
            database: DatabaseConfig::default(),
            verification: VerificationConfig {
                argv: vec!["cargo".into(), "test".into()],
                cwd: "checks".into(),
                timeout_ms: 42,
            },
            thread: threads,
        };
        let plan = config.plan();
        assert_eq!(plan.argv, ["cargo", "test"]);
        assert_eq!(plan.cwd, "checks");
        assert_eq!(plan.timeout_ms, 42);
        assert_eq!(plan.grace_ms, 250);
        assert_eq!(plan.stdout_cap, 16 * 1024);
        assert_eq!(plan.stderr_cap, 16 * 1024);
        let derived = config.thread_policy();
        assert_eq!(derived.max_request_bytes, policy.max_request_bytes);
        assert_eq!(derived.context_cap_bytes, policy.context_cap_bytes);
        assert_eq!(
            config.database_path(Path::new("/workspace")),
            Path::new("/workspace/.latte/latte-code.db")
        );
    }

    #[test]
    fn workspace_identity_is_canonical_and_database_path_honors_absolute_configuration() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            workspace_identity(root.path()).unwrap(),
            std::fs::canonicalize(root.path())
                .unwrap()
                .to_str()
                .unwrap()
        );
        let absolute = root.path().join("state.db");
        let config = AppConfig {
            version: 1,
            default_model: "primary/test".into(),
            providers: json!({}),
            database: DatabaseConfig {
                path: absolute.display().to_string(),
            },
            verification: VerificationConfig {
                argv: vec!["true".into()],
                cwd: ".".into(),
                timeout_ms: 1,
            },
            thread: ThreadConfig::default(),
        };
        assert_eq!(config.database_path(Path::new("/ignored")), absolute);
    }

    #[test]
    fn config_merge_and_validation_cover_scalar_array_and_typed_failure_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".latte")).unwrap();
        std::fs::create_dir_all(home.path().join(".latte")).unwrap();
        std::fs::write(
            home.path().join(".latte/latte-code.jsonc"),
            r#"{
                default_model: "primary/model",
                providers: { primary: {
                    type: "openai-chat",
                    models: ["model"],
                    endpoint: "https://provider.example/chat/completions",
                    api_key: { source: "env", name: "TEST_PROVIDER_KEY" }
                } }
            }"#,
        )
        .unwrap();
        let config_path = root.path().join(".latte/latte-code.jsonc");

        std::fs::write(&config_path, "[]").unwrap();
        assert!(
            AppConfig::load_with_home(root.path(), Some(home.path()))
                .unwrap_err()
                .contains("top-level configuration must be an object")
        );
        std::fs::write(&config_path, r"{ verification: { argv: [] } }").unwrap();
        assert_eq!(
            AppConfig::load_with_home(root.path(), Some(home.path())).unwrap_err(),
            "verification.argv must not be empty"
        );
        std::fs::write(&config_path, r#"{ database: { path: " " } }"#).unwrap();
        assert_eq!(
            AppConfig::load_with_home(root.path(), Some(home.path())).unwrap_err(),
            "database.path must not be empty"
        );
        std::fs::write(
            &config_path,
            r"{ thread: { max_input_bytes: 8, reserved_output_bytes: 8 } }",
        )
        .unwrap();
        assert!(
            AppConfig::load_with_home(root.path(), Some(home.path()))
                .unwrap_err()
                .contains("invalid thread configuration")
        );
        std::fs::write(&config_path, r"{ unexpected: true }").unwrap();
        assert!(
            AppConfig::load_with_home(root.path(), Some(home.path()))
                .unwrap_err()
                .contains("unknown field")
        );

        let directory = root.path().join("not-a-file");
        std::fs::create_dir(&directory).unwrap();
        assert!(
            merge_optional_config(&mut json!({}), &directory)
                .unwrap_err()
                .contains("cannot read")
        );
        assert!(merge_optional_config(&mut json!({}), &root.path().join("missing")).is_ok());

        let mut merged = json!({"nested":{"kept":1,"array":[1]},"scalar":1});
        merge_value(
            &mut merged,
            json!({"nested":{"added":2,"array":[2,3]},"scalar":{"now":true}}),
        );
        assert_eq!(
            merged,
            json!({
                "nested":{"kept":1,"added":2,"array":[2,3]},
                "scalar":{"now":true}
            })
        );
    }

    #[test]
    fn verification_defaults_and_output_contracts_are_stable_for_humans_and_json() {
        let config: AppConfig = serde_json::from_value(json!({
            "version": 1,
            "default_model": "primary/test",
            "providers": {},
            "verification": { "argv": ["true"] }
        }))
        .unwrap();
        assert_eq!(config.verification.cwd, dot());
        assert_eq!(config.verification.timeout_ms, verify_timeout());
        assert_eq!(config.database.path, ".latte/latte-code.db");

        let mut completed = state(RunStatus::Completed);
        completed.revision = 4;
        completed.handoff = Some(Handoff {
            summary: "verified handoff".into(),
            files_changed: vec!["src/lib.rs".into()],
            evidence: vec![Evidence {
                name: "unit".into(),
                status: VerificationStatus::Passed,
                summary: "all green".into(),
            }],
        });
        assert_eq!(render_run(&completed, false), EXIT_COMPLETED);
        assert_eq!(render_run(&completed, true), EXIT_COMPLETED);

        let waiting = state(RunStatus::WaitingInput);
        assert_eq!(render_run(&waiting, false), EXIT_WAITING);
        assert_eq!(render_run(&waiting, true), EXIT_WAITING);
        emit_data("completed", &json!({"kind":"contract-probe"}));
        assert_eq!(
            emit_error(true, "usage", "bad_argument", "bad", EXIT_USAGE, true),
            EXIT_USAGE
        );
        assert_eq!(
            emit_error(false, "usage", "bad_argument", "bad", EXIT_USAGE, true),
            EXIT_USAGE
        );
        assert_eq!(
            emit_error(
                false,
                "internal",
                "storage",
                "storage failed",
                EXIT_INTERNAL,
                false,
            ),
            EXIT_INTERNAL
        );
    }

    #[tokio::test]
    async fn deny_headless_distinguishes_missing_invalid_and_prepared_permission_states() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "original").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(root.path().join("state.db"))
            .build()
            .unwrap();
        let ids = SystemIdSource::default();
        let missing = RunId::from_uuid(ids.next_uuid_v7());
        assert_eq!(deny_headless(&engine, missing, true), EXIT_NOT_FOUND);

        let queued_id = RunId::from_uuid(ids.next_uuid_v7());
        engine.create_run(queued_id, 1).unwrap();
        assert_eq!(deny_headless(&engine, queued_id, false), EXIT_FAILED);

        let runtime = AgentRuntime::new(
            engine.clone(),
            FakeProvider::scripted([
                ProviderResponse {
                    message: Some("I will inspect the current file first".into()),
                    tool_calls: vec![ToolCall {
                        id: "call_read_1".into(),
                        name: "read_file".into(),
                        input: json!({"path":"note.txt"}),
                    }],
                    input_request: None,
                    usage: ProviderUsage::default(),
                    finish_reason: None,
                    provider_state: None,
                },
                ProviderResponse {
                    message: Some("I need permission to update the file".into()),
                    tool_calls: vec![ToolCall {
                        id: "call_write_1".into(),
                        name: "write_file".into(),
                        input: json!({
                            "path":"note.txt",
                            "content":"updated",
                            "create_intent":false,
                            "precondition":"0682c5f2076f099c34cfdd15a9e063849ed437a49677e6fcc5b4198c76575be5"
                        }),
                    }],
                    input_request: None,
                    usage: ProviderUsage::default(),
                    finish_reason: None,
                    provider_state: None,
                },
            ]),
            root.path(),
            latte_headless::runtime::VerificationPlan {
                argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 100,
                stdout_cap: 1_024,
                stderr_cap: 1_024,
            },
        );
        let run_id = match runtime.run("update note.txt").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("expected permission boundary, got {error}"),
        };
        let waiting = engine.show(run_id).unwrap();
        assert_eq!(waiting.status, RunStatus::WaitingPermission);
        assert!(waiting.pending_permission.is_some());
        assert_eq!(
            std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
            "original"
        );

        assert_eq!(deny_headless(&engine, run_id, true), EXIT_DENIED);
        let denied = engine.show(run_id).unwrap();
        assert_eq!(denied.status, RunStatus::Failed);
        assert_eq!(
            denied.failure.as_ref().map(|failure| failure.code),
            Some(FailureCode::PermissionDenied)
        );
        assert!(denied.pending_permission.is_none());
        assert_eq!(
            std::fs::read_to_string(root.path().join("note.txt")).unwrap(),
            "original"
        );
        assert_eq!(deny_headless(&engine, run_id, false), EXIT_FAILED);
    }

    #[test]
    fn tui_entrypoint_rejects_non_terminal_processes_before_loading_authority() {
        assert_eq!(execute_tui(), EXIT_USAGE);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn workspace_and_projection_adapters_cover_empty_event_and_lagged_states() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("no/git/here");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(discover_workspace_root(&nested), nested);
        assert_eq!(
            workspace_display_path_with_home(root.path(), None),
            root.path().display().to_string()
        );
        assert!(!workspace_display_path(root.path()).is_empty());

        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let canonical_root = workspace_identity(root.path()).unwrap();
        let mut projection = ThreadEngineProjection {
            subscription: engine.subscribe_threads(),
            engine: engine.clone(),
            workspace_root: canonical_root.clone(),
        };
        assert!(projection.snapshots().unwrap().is_empty());
        assert_eq!(projection.poll(), ThreadProjectionPoll::Empty);

        let ids = SystemIdSource::default();
        let binding = ThreadProviderBindingV2 {
            version: 1,
            provider_name: "test".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "test".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:TEST".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        let mut created = Vec::new();
        for index in 0..510 {
            let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
            let run_id = RunId::from_uuid(ids.next_uuid_v7());
            engine
                .create_thread_v2(
                    thread_id,
                    run_id,
                    binding.clone(),
                    &format!("thread-{index}"),
                    u64::try_from(index + 1).unwrap(),
                )
                .unwrap();
            created.push((thread_id, run_id));
        }
        let first_thread_id = created[0].0;
        let catalog = projection.session_catalog().unwrap();
        assert_eq!(catalog.len(), 200);
        assert!(
            catalog
                .iter()
                .all(|session| session.workspace_root == canonical_root)
        );
        assert_eq!(
            projection.session(first_thread_id).unwrap().thread_id,
            first_thread_id
        );
        assert_eq!(
            projection
                .exact_session(&first_thread_id.to_string())
                .unwrap()
                .unwrap()
                .thread_id,
            first_thread_id,
            "exact resume must not inherit the recent catalog cap"
        );
        assert_eq!(
            projection
                .exact_session("thread-0")
                .unwrap()
                .unwrap()
                .thread_id,
            first_thread_id,
            "exact title resume must search beyond the recent catalog cap"
        );

        let mut foreign_projection = ThreadEngineProjection {
            subscription: engine.subscribe_threads(),
            engine: engine.clone(),
            workspace_root: "/another/workspace".into(),
        };
        assert!(foreign_projection.session_catalog().unwrap().is_empty());
        assert!(
            foreign_projection
                .session(first_thread_id)
                .unwrap_err()
                .contains("belongs to another workspace")
        );
        assert!(
            foreign_projection
                .exact_session(&first_thread_id.to_string())
                .unwrap_err()
                .contains("belongs to another workspace")
        );
        assert!(
            projection
                .exact_session("not-a-session-id")
                .unwrap()
                .is_none()
        );
        let missing_thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        assert!(
            projection
                .session(missing_thread_id)
                .unwrap_err()
                .contains("was not found")
        );
        assert!(
            projection
                .exact_session(&missing_thread_id.to_string())
                .unwrap()
                .is_none()
        );
        for (index, (thread_id, run_id)) in created.iter().copied().take(70).enumerate() {
            let lease = engine.acquire_thread_lease(thread_id, 100, 10_000).unwrap();
            engine
                .commit_thread_run_update(
                    ThreadCommitRequest {
                        thread_id,
                        run_id,
                        expected_thread_revision: 0,
                        expected_run_revision: 0,
                        command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: None,
                        update: CommitThreadRunUpdate::Start {
                            source_key: format!("projection:start:{index}"),
                        },
                    },
                    &lease,
                    u64::try_from(index + 200).unwrap(),
                )
                .unwrap();
        }
        assert!(matches!(projection.poll(), ThreadProjectionPoll::Lagged(count) if count > 0));
        assert_eq!(projection.poll(), ThreadProjectionPoll::Event);
        let refreshed = projection.snapshots().unwrap();
        assert_eq!(refreshed.len(), 510);
    }

    #[test]
    fn complete_example_config_loads_application_and_provider_sections() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let config_dir = root.path().join(".latte");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("latte-code.jsonc"),
            include_str!("../../../latte-code.config.example.jsonc"),
        )
        .unwrap();

        let (config, registry) = AppConfig::load_with_home(root.path(), Some(home.path())).unwrap();
        assert_eq!(config.thread.max_request_bytes, 524_288);
        assert_eq!(config.thread.reserved_output_bytes, 131_072);
        assert_eq!(registry.default_name(), Some("primary"));
        assert_eq!(registry.default_model(), Some("model-id"));
    }

    #[test]
    fn missing_provider_configuration_keeps_read_only_state_commands_available() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        let (config, registry) = AppConfig::load_with_home(root.path(), Some(home.path())).unwrap();

        assert!(config.default_model.is_empty());
        assert_eq!(config.database.path, ".latte/latte-code.db");
        assert!(registry.model_catalog().is_empty());
        assert!(registry.thread_binding_for_default(&[]).is_err());
    }

    #[test]
    fn workspace_configuration_recursively_overrides_user_configuration() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".latte")).unwrap();
        std::fs::create_dir_all(root.path().join(".latte")).unwrap();
        std::fs::write(
            home.path().join(".latte/latte-code.jsonc"),
            r#"{
                database: { path: "user.db" },
                verification: { timeout_ms: 9000 },
                default_model: "primary/user-model",
                providers: { primary: {
                    type: "openai-chat",
                    models: { "user-model": {} },
                    endpoint: "https://provider.example/chat/completions",
                    api_key: { source: "env", name: "TEST_PROVIDER_KEY" },
                    credential_ref_id: "env:TEST_PROVIDER_KEY",
                    data_scope_id: "workspace",
                    credential_generation: 1
                } }
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join(".latte/latte-code.jsonc"),
            r#"{
                database: { path: "workspace.db" },
                default_model: "primary/workspace-model",
                providers: { primary: { models: { "workspace-model": {} } } }
            }"#,
        )
        .unwrap();

        let (config, registry) = AppConfig::load_with_home(root.path(), Some(home.path())).unwrap();
        let binding = registry.thread_binding_for_default(&[]).unwrap();

        assert_eq!(config.database.path, "workspace.db");
        assert_eq!(config.verification.timeout_ms, 9000);
        assert_eq!(binding.model, "workspace-model");
        assert_eq!(registry.model_catalog().len(), 1);
    }

    #[test]
    fn present_invalid_configuration_reports_its_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".latte/latte-code.jsonc");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[").unwrap();

        let error = AppConfig::load_with_home(root.path(), None).unwrap_err();

        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("invalid JSONC"));
    }

    #[test]
    fn workspace_discovery_walks_up_from_build_output() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let nested = root.path().join("target/debug");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(discover_workspace_root(&nested), root.path());
    }

    #[test]
    fn startup_workspace_path_is_compact_without_losing_the_resolved_root() {
        let home = Path::new("/Users/example");
        assert_eq!(
            workspace_display_path_with_home(
                Path::new("/Users/example/projects/latte-code"),
                Some(home)
            ),
            "~/projects/latte-code"
        );
        assert_eq!(
            workspace_display_path_with_home(Path::new("/opt/latte-code"), Some(home)),
            "/opt/latte-code"
        );
        assert_eq!(workspace_display_path_with_home(home, Some(home)), "~");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn tui_reconciliation_adapter_uses_exact_v2_effect_and_terminalizes_child() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "fixture").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(root.path().join("state.db"))
            .build()
            .unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let binding = ThreadProviderBindingV2 {
            version: 1,
            provider_name: "test".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "test".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:TEST".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        engine
            .create_thread_v2(thread_id, run_id, binding, "recover", 1)
            .unwrap();
        let lease = engine.acquire_thread_lease(thread_id, 2, 10_000).unwrap();
        let running = engine
            .commit_thread_run_update(
                ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: 0,
                    expected_run_revision: 0,
                    command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitThreadRunUpdate::Start {
                        source_key: "test:start".into(),
                    },
                },
                &lease,
                3,
            )
            .unwrap()
            .snapshot;
        let descriptor = ThreadEffectDescriptor {
            effect_id: format!("thread-effect:{run_id}:tui-reconcile"),
            tool_call_id: "tui-reconcile".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"note.txt"}),
            attempt: 1,
        };
        let prepared = engine
            .prepare_thread_effect(
                ThreadEffectRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: running.revision,
                    expected_run_revision: running.runs[0].run_revision,
                    command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    source_key: "test:prepare".into(),
                    descriptor: descriptor.clone(),
                },
                &lease,
                4,
            )
            .unwrap();
        let started = engine
            .start_thread_effect(
                ThreadEffectStartRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: prepared.snapshot.revision,
                    expected_run_revision: prepared.snapshot.runs[0].run_revision,
                    command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    source_key: "test:start-effect".into(),
                    effect_id: descriptor.effect_id.clone(),
                },
                prepared.operation_digest,
                &lease,
                5,
            )
            .unwrap();
        let unknown = engine
            .mark_thread_effect_unknown(
                &started,
                "test:unknown".into(),
                ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                &lease,
                6,
            )
            .unwrap();
        assert_eq!(unknown.lifecycle, ThreadLifecycle::ReconciliationRequired);
        engine.release_lease(&lease).unwrap();
        let service = ThreadRuntimeService::new(
            engine.clone(),
            root.path(),
            ThreadHistoryPolicy::default(),
            Arc::new(|_| Err("provider is unused for reconciliation".into())),
        );

        assert_eq!(
            reconcile_thread_action(&service, thread_id, &descriptor.effect_id).unwrap(),
            "unknown effect acknowledged; child aborted"
        );
        assert_eq!(
            engine
                .thread_snapshot_v2(thread_id, None, 100)
                .unwrap()
                .lifecycle,
            ThreadLifecycle::Failed
        );
        assert_eq!(
            engine.effect_status(&descriptor.effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedFailed
        );
        assert_eq!(engine.show(run_id).unwrap().status, RunStatus::Failed);
    }
}
