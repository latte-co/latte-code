#![allow(clippy::semicolon_if_nothing_returned)]
use latte_core::{IdSource, SystemIdSource, ThreadId, ThreadProviderBindingV2, wall_time_ms};
use latte_engine::EngineBuilder;
use latte_headless::{
    registry::ProviderRegistry,
    runtime::VerificationPlan,
    thread::{ThreadHistoryPolicy, ThreadRuntimeService},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

mod server_client;

const JSON_VERSION: u8 = 2;
const EXIT_COMPLETED: i32 = 0;
const EXIT_USAGE: i32 = 2;
const EXIT_INTERNAL: i32 = 70;
const EXIT_INTERRUPTED: i32 = 130;
const CONFIG_RELATIVE_PATH: &str = ".latte/latte-code.jsonc";
const STORAGE_HOME_ENV: &str = "LATTE_CODE_HOME";
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
const HELP: &str = "Latte Code agent\n\nUsage:\n  latte-code tui\n  latte-code [--json] run [--focus <path>] [--server url] [--token token] <prompt>\n  latte-code [--json] resume <session-id> <prompt> [--server url] [--token token]\n  latte-code [--json] show <session-id> [--server url] [--token token]\n  latte-code [--json] list [--server url] [--token token]\n  latte-code [--json] serve [--port <port>]\n  latte-code [--json] --help\n\nLatte Code merges built-in application defaults, $HOME/.latte/latte-code.jsonc, then workspace .latte/latte-code.jsonc; later values win. Configure the global default_model and at least one Provider model explicitly. Durable state lives in $LATTE_CODE_HOME/state.db, defaulting to $HOME/.latte/latte-code/state.db. database.path remains parseable for migration compatibility but does not redirect user history. Provider credentials may be literal strings or environment references in those files. run/list/show/resume are session commands served over HTTP+SSE: by default the server is embedded in this process (random loopback port, token kept in memory); --server connects to a standalone server, reading its token from $LATTE_CODE_HOME/server.token or --token. serve starts the local HTTP server on 127.0.0.1 (default port 4096, or an ephemeral port with --port 0); its Bearer token is written to $LATTE_CODE_HOME/server.token with owner-only permissions.";

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
    fn legacy_database_path(&self, root: &Path) -> PathBuf {
        let path = Path::new(&self.database.path);
        if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
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

fn storage_home() -> Result<PathBuf, String> {
    storage_home_with(
        std::env::var_os(STORAGE_HOME_ENV).as_deref(),
        std::env::var_os("HOME").as_deref().map(Path::new),
    )
}

fn storage_home_with(
    override_home: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = if let Some(value) = override_home {
        if value.is_empty() {
            return Err(format!("{STORAGE_HOME_ENV} must not be empty"));
        }
        PathBuf::from(value)
    } else {
        home.ok_or_else(|| format!("{STORAGE_HOME_ENV} or HOME must be set"))?
            .join(".latte/latte-code")
    };
    if !path.is_absolute() {
        return Err(format!("{STORAGE_HOME_ENV} must be an absolute path"));
    }
    Ok(path)
}

fn storage_database_path() -> Result<PathBuf, String> {
    Ok(storage_home()?.join("state.db"))
}

fn storage_conversation_root() -> Result<PathBuf, String> {
    Ok(storage_home()?.join("sessions"))
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
    fn search_session_catalog(
        &mut self,
        query: &str,
    ) -> Result<Vec<latte_core::ThreadSessionSummary>, String> {
        self.engine
            .search_thread_sessions_v2(query, 200)
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
    if matches!(args.first().map(String::as_str), Some("serve")) {
        return execute_serve(&args[1..], json).await;
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

    // run/list/show/resume are session commands served over HTTP+SSE: the
    // server is the single engine host (embedded by default, or standalone
    // via --server).
    execute_session_command(json, &args).await
}

/// Executes the v2 session commands (`run`/`list`/`show`/`resume`) over
/// HTTP+SSE. By default the server is embedded in this process on a random
/// loopback port; `--server` connects to a standalone server instead.
async fn execute_session_command(json: bool, args: &[String]) -> i32 {
    let parsed = match server_client::parse_session_command(args) {
        Ok(parsed) => parsed,
        Err(message) => return emit_error(json, "usage", "usage", &message, EXIT_USAGE, true),
    };
    // `--json` is accepted both as a global prefix and after the subcommand.
    let json = json || parsed.json;
    let root = match std::env::current_dir() {
        Ok(cwd) => discover_workspace_root(&cwd),
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
    let storage_home = match storage_home() {
        Ok(path) => path,
        Err(message) => {
            return emit_error(json, "usage", "configuration", &message, EXIT_USAGE, false);
        }
    };
    let (mut client, embedded) =
        match server_client::connect(parsed.server, parsed.token, &root, &storage_home).await {
            Ok(connected) => connected,
            Err(error) => return emit_client_error(json, &error),
        };
    let outcome = execute_session_command_inner(&mut client, parsed.command, &root, json, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
    // Close the client (dropping any open SSE stream) before stopping the
    // embedded server so graceful shutdown does not wait on it.
    drop(client);
    if let Some(embedded) = embedded {
        embedded.shutdown().await;
    }
    match outcome {
        Ok(code) => code,
        Err(error) => emit_client_error(json, &error),
    }
}

async fn execute_session_command_inner(
    client: &mut server_client::ServerClient,
    command: server_client::SessionCommand,
    root: &Path,
    json: bool,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<i32, server_client::ClientError> {
    use server_client::SessionServer;
    let mut progress = |text: &str| {
        eprint!("{text}");
        let _ = std::io::stderr().flush();
    };
    match command {
        server_client::SessionCommand::Run { prompt, focus } => {
            let result = server_client::run_session(
                client,
                root,
                &prompt,
                focus.as_deref(),
                &mut progress,
                cancel,
            )
            .await?;
            if json {
                println!("{}", server_client::run_envelope(&result));
            } else if let Some(snapshot) = result.snapshot() {
                println!("{}", server_client::render_session_text(snapshot));
            }
            Ok(result.exit_code())
        }
        server_client::SessionCommand::Resume { session_id, prompt } => {
            let result = server_client::resume_session(
                client,
                root,
                &session_id,
                &prompt,
                &mut progress,
                cancel,
            )
            .await?;
            if json {
                println!("{}", server_client::run_envelope(&result));
            } else if let Some(snapshot) = result.snapshot() {
                println!("{}", server_client::render_session_text(snapshot));
            }
            Ok(result.exit_code())
        }
        server_client::SessionCommand::List => {
            let workspace_id = client.resolve_workspace(root).await?;
            let sessions = client.list_sessions(&workspace_id).await?;
            if json {
                println!("{}", server_client::list_envelope(&sessions));
            } else {
                for session in &sessions {
                    println!("{}", server_client::render_session_row(session));
                }
            }
            Ok(EXIT_COMPLETED)
        }
        server_client::SessionCommand::Show { session_id } => {
            let thread_id = server_client::parse_session_id(&session_id)?;
            let snapshot = client.snapshot(&thread_id).await?;
            if json {
                println!("{}", server_client::session_envelope(&snapshot));
            } else {
                println!("{}", server_client::render_session_text(&snapshot));
            }
            Ok(EXIT_COMPLETED)
        }
    }
}

/// Emits a v2 error envelope (JSON) or plain stderr line and returns the
/// classified exit code.
fn emit_client_error(json: bool, error: &server_client::ClientError) -> i32 {
    if json {
        println!("{}", server_client::error_envelope(error));
    } else {
        eprintln!("{}", error.message());
    }
    error.exit_code()
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

#[allow(clippy::too_many_lines)]
fn dispatch_session_management_action(
    engine: &latte_engine::EngineHandle,
    feedback_tx: &std::sync::mpsc::Sender<latte_tui::thread::ThreadUiFeedback>,
    action: latte_tui::thread::ThreadUiAction,
) -> Option<latte_tui::thread::ThreadUiAction> {
    use latte_tui::thread::{SessionManagementOutcome, ThreadUiAction, ThreadUiFeedback};

    match action {
        ThreadUiAction::RenameSession { thread_id, title } => {
            let engine = engine.clone();
            let feedback = feedback_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = engine
                    .rename_thread_session_v2(thread_id, &title)
                    .map(|session| {
                        SessionManagementOutcome::Updated(format!(
                            "Session renamed to {}",
                            session.title
                        ))
                    })
                    .map_err(|error| error.to_string());
                let _ = feedback.send(ThreadUiFeedback::session_management(result));
            });
            None
        }
        ThreadUiAction::ForkSession { thread_id, title } => {
            let engine = engine.clone();
            let feedback = feedback_tx.clone();
            tokio::task::spawn_blocking(move || {
                let fork_id =
                    ThreadId::from_uuid(latte_core::SystemIdSource::default().next_uuid_v7());
                let result = engine
                    .fork_thread_session_v2(thread_id, fork_id, title.as_deref(), wall_time_ms())
                    .map(|snapshot| SessionManagementOutcome::Forked(snapshot.thread_id))
                    .map_err(|error| error.to_string());
                let _ = feedback.send(ThreadUiFeedback::session_management(result));
            });
            None
        }
        action => Some(action),
    }
}

fn open_tui_engine(
    root: &Path,
    config: &AppConfig,
    database_path: &Path,
    conversation_root: &Path,
    now_ms: u64,
) -> Result<latte_engine::EngineHandle, String> {
    let parent = database_path
        .parent()
        .ok_or_else(|| "global storage database must have a parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let engine = EngineBuilder::new()
        .workspace_root(root)
        .database_path(database_path)
        .conversation_root(conversation_root)
        .build()
        .map_err(|error| error.to_string())?;
    engine
        .import_legacy_workspace_database(config.legacy_database_path(root), now_ms)
        .map_err(|error| format!("legacy import: {error}"))?;
    Ok(engine)
}

fn tui_startup_presentation(
    root: &Path,
    registry: &latte_headless::registry::ProviderRegistry,
    engine: &latte_engine::EngineHandle,
) -> Result<
    (
        Option<ThreadProviderBindingV2>,
        latte_tui::thread::ThreadStartupPresentation,
    ),
    String,
> {
    let startup_binding = match registry.thread_binding_for_default(&engine.tool_descriptors()) {
        Ok(binding) => Some(binding),
        Err(_) if registry.model_catalog().is_empty() => None,
        Err(error) => return Err(error.to_string()),
    };
    let startup = latte_tui::thread::ThreadStartupPresentation {
        default_provider: startup_binding
            .as_ref()
            .map_or_else(String::new, |binding| binding.provider_name.clone()),
        default_model: startup_binding
            .as_ref()
            .map_or_else(String::new, |binding| binding.model.clone()),
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
        workspace_display: workspace_display_path(root),
        permission_mode: latte_tui::thread::ThreadPermissionMode::Ask,
    };
    Ok((startup_binding, startup))
}

/// Holds the fully-resolved state the TUI main loop needs.
struct TuiSetup {
    engine: latte_engine::EngineHandle,
    startup_binding: Option<ThreadProviderBindingV2>,
    startup: latte_tui::thread::ThreadStartupPresentation,
    progress_rx: std::sync::mpsc::Receiver<latte_core::ThreadTransientProgress>,
    service: ThreadRuntimeService,
    projection: ThreadEngineProjection,
    feedback_tx: std::sync::mpsc::Sender<latte_tui::thread::ThreadUiFeedback>,
    feedback_rx: std::sync::mpsc::Receiver<latte_tui::thread::ThreadUiFeedback>,
    action_registry: latte_headless::registry::ProviderRegistry,
}

/// Resolves config, engine, and runtime state for the TUI. Separated from
/// [`execute_tui`] so the setup paths are testable without a TTY.
fn tui_setup() -> Result<TuiSetup, i32> {
    let root = match std::env::current_dir() {
        Ok(root) => discover_workspace_root(&root),
        Err(error) => {
            eprintln!("{error}");
            return Err(EXIT_INTERNAL);
        }
    };
    let (config, registry) = match AppConfig::load(&root) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("configuration: {error}");
            return Err(EXIT_USAGE);
        }
    };
    let database_path = match storage_database_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("configuration: {error}");
            return Err(EXIT_USAGE);
        }
    };
    let conversation_root = match storage_conversation_root() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("configuration: {error}");
            return Err(EXIT_USAGE);
        }
    };
    let engine = match open_tui_engine(
        &root,
        &config,
        &database_path,
        &conversation_root,
        wall_time_ms(),
    ) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            return Err(EXIT_INTERNAL);
        }
    };
    let (startup_binding, startup) = match tui_startup_presentation(&root, &registry, &engine) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("configuration: {error}");
            return Err(EXIT_USAGE);
        }
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
        std::sync::Arc::new(move |_thread_id, progress| {
            let _ = progress_tx.send(progress);
        });
    let service = ThreadRuntimeService::new(engine.clone(), &root, config.thread_policy(), factory)
        .with_progress_sink(progress_sink)
        .with_verification(config.plan());
    let projection = ThreadEngineProjection {
        engine: engine.clone(),
        subscription: engine.subscribe_threads(),
        workspace_root: match workspace_identity(&root) {
            Ok(identity) => identity,
            Err(error) => {
                eprintln!("configuration: {error}");
                return Err(EXIT_USAGE);
            }
        },
    };
    let (feedback_tx, feedback_rx) =
        std::sync::mpsc::channel::<latte_tui::thread::ThreadUiFeedback>();
    let action_registry = registry.clone();
    Ok(TuiSetup {
        engine,
        startup_binding,
        startup,
        progress_rx,
        service,
        projection,
        feedback_tx,
        feedback_rx,
        action_registry,
    })
}

/// Transcript-first interactive entrypoint. The legacy v1 CLI remains intact;
/// the TUI reads only v2 snapshots and submits v2 conversation requests.
fn execute_tui() -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "interactive TUI requires a TTY; use --json list/show/run/resume for non-interactive use"
        );
        return EXIT_USAGE;
    }
    let setup = match tui_setup() {
        Ok(setup) => setup,
        Err(code) => return code,
    };
    tui_main_loop(setup)
}

/// The TUI main loop: runs the terminal UI and dispatches actions.
#[allow(clippy::too_many_lines)]
fn tui_main_loop(setup: TuiSetup) -> i32 {
    let TuiSetup {
        engine,
        startup_binding,
        startup,
        progress_rx,
        service,
        mut projection,
        feedback_tx,
        feedback_rx,
        action_registry,
    } = setup;
    match latte_tui::thread::run_with_feedback_and_progress(
        &mut projection,
        startup,
        move |action| {
            let Some(action) = dispatch_session_management_action(&engine, &feedback_tx, action)
            else {
                return Ok(());
            };
            match action {
                latte_tui::thread::ThreadUiAction::Start {
                    submission_id,
                    prompt,
                } => {
                    let Some(binding) = startup_binding.clone() else {
                        let _ = feedback_tx.send(
                            latte_tui::thread::ThreadUiFeedback::submission(
                                submission_id,
                                Err("configure default_model and providers in ~/.latte/latte-code.jsonc, then restart Latte Code".into()),
                            ),
                        );
                        return Ok(());
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
                            .start(thread_id, prompt, binding, None)
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
                            .start(thread_id, prompt, binding, None)
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
                latte_tui::thread::ThreadUiAction::QueueFollowUp {
                    submission_id,
                    thread_id,
                    prompt,
                } => {
                    let result = service
                        .queue_follow_up(thread_id, prompt)
                        .map(|position| format!("follow-up queued at position {position}"))
                        .map_err(|error| error.to_string());
                    let _ = feedback_tx.send(latte_tui::thread::ThreadUiFeedback::submission(
                        submission_id,
                        result,
                    ));
                }
                latte_tui::thread::ThreadUiAction::Cancel { thread_id } => {
                    let service = service.clone();
                    let feedback = feedback_tx.clone();
                    let cancel_engine = engine.clone();
                    tokio::task::spawn_blocking(move || {
                        // The TUI cancels the session's current active run, so
                        // fence against the live snapshot's revisions.
                        let result = cancel_engine
                            .thread_snapshot_v2(thread_id, None, 1)
                            .map_err(|error| error.to_string())
                            .and_then(|snapshot| {
                                let run_revision = snapshot
                                    .active_run_id
                                    .and_then(|run_id| {
                                        snapshot.runs.iter().find(|run| run.run_id == run_id)
                                    })
                                    .map(|run| run.run_revision)
                                    .unwrap_or_default();
                                service
                                    .cancel_durable(thread_id, snapshot.revision, run_revision)
                                    .map(|_| "interruption requested".into())
                                    .map_err(|error| error.to_string())
                            });
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
                            let run_revision = snapshot
                                .active_run_id
                                .and_then(|run_id| {
                                    snapshot
                                        .runs
                                        .iter()
                                        .find(|r| r.run_id == run_id)
                                        .map(|r| r.run_revision)
                                })
                                .unwrap_or(0);
                            tokio::spawn(async move {
                                let result = service
                                    .provide_input(
                                        thread_id,
                                        snapshot.revision,
                                        run_revision,
                                        request_id,
                                        value,
                                    )
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
                            let run_revision = snapshot
                                .active_run_id
                                .and_then(|run_id| {
                                    snapshot
                                        .runs
                                        .iter()
                                        .find(|r| r.run_id == run_id)
                                        .map(|r| r.run_revision)
                                })
                                .unwrap_or(0);
                            tokio::spawn(async move {
                                let result = service
                                    .resolve_permission(
                                        thread_id,
                                        snapshot.revision,
                                        run_revision,
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
                | latte_tui::thread::ThreadUiAction::SearchSessions { .. }
                | latte_tui::thread::ThreadUiAction::OpenSession { .. }
                | latte_tui::thread::ThreadUiAction::Quit => {}
                latte_tui::thread::ThreadUiAction::RenameSession { .. }
                | latte_tui::thread::ThreadUiAction::ForkSession { .. } => {
                    unreachable!("session management actions are dispatched before this match")
                }
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

/// Default local server port when `--port` is not supplied.
const DEFAULT_SERVER_PORT: u16 = 4096;

/// Starts the local HTTP server through the final binary. This is the single
/// observable entry point users have to run `latte-server`: it loads the same
/// layered configuration and provider registry as the CLI/TUI, builds a
/// per-binding provider factory, mints a Bearer token into
/// `$LATTE_CODE_HOME/server.token` (owner-only), and serves on 127.0.0.1.
async fn execute_serve(args: &[String], json: bool) -> i32 {
    // `--port` parse errors are a usage error with help.
    let port = match parse_serve_port(args) {
        Ok(port) => port,
        Err(error) => return emit_error(json, "usage", "usage", &error, EXIT_USAGE, true),
    };
    match serve_bound(port, json).await {
        Ok(code) => code,
        Err(ServerSetupError {
            code,
            category,
            message,
        }) => emit_error(json, code, category, &message, exit_for_setup(code), false),
    }
}

/// Sets up and runs the server, returning a process exit code or a classified
/// setup error. Splitting this out lets `execute_serve` route every setup
/// failure through one uniform emitter.
async fn serve_bound(port: u16, json: bool) -> Result<i32, ServerSetupError> {
    use std::io::Write;

    let root = std::env::current_dir()
        .map(|cwd| discover_workspace_root(&cwd))
        .map_err(|error| ServerSetupError {
            code: "internal",
            category: "current_directory",
            message: error.to_string(),
        })?;

    let storage_home = storage_home().map_err(|message| ServerSetupError {
        code: "usage",
        category: "configuration",
        message,
    })?;
    let (state, token, token_path) = prepare_server(&root, &storage_home)?;

    // Bind the listener FIRST so a port conflict fails before we publish the
    // token; only the process that will actually serve writes server.token.
    let (listener, local_addr) = bind_local_listener(port).await?;

    write_server_token(&token_path, &token).map_err(|message| ServerSetupError {
        code: "internal",
        category: "server_token",
        message,
    })?;

    // Announce the observable readiness contract before blocking on serve so a
    // client (or E2E) can discover the port and token file deterministically.
    if json {
        emit_data("listening", &readiness_envelope(local_addr, &token_path));
    } else {
        println!(
            "latte-code server listening on http://{local_addr}; token at {}",
            token_path.display()
        );
    }
    let _ = std::io::stdout().flush();

    latte_server::serve_on(state, listener)
        .await
        .map_err(|error| ServerSetupError {
            code: "internal",
            category: "server_runtime",
            message: error.to_string(),
        })?;
    Ok(EXIT_COMPLETED)
}

/// The JSON readiness envelope announced when the server begins listening.
fn readiness_envelope(local_addr: std::net::SocketAddr, token_path: &Path) -> Value {
    json!({
        "address": local_addr.to_string(),
        "port": local_addr.port(),
        "token_path": token_path.display().to_string(),
    })
}

/// A classified failure from `prepare_server`, carrying the same `(code,
/// category, message)` shape the CLI emits.
#[derive(Debug)]
struct ServerSetupError {
    code: &'static str,
    category: &'static str,
    message: String,
}

/// Maps a setup failure's status code to the process exit code.
fn exit_for_setup(code: &str) -> i32 {
    if code == "usage" {
        EXIT_USAGE
    } else {
        EXIT_INTERNAL
    }
}

/// Binds the loopback listener and resolves its local address, classifying any
/// failure as an internal `server_bind` setup error.
async fn bind_local_listener(
    port: u16,
) -> Result<(tokio::net::TcpListener, std::net::SocketAddr), ServerSetupError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| ServerSetupError {
            code: "internal",
            category: "server_bind",
            message: format!("cannot bind 127.0.0.1:{port}: {error}"),
        })?;
    let local_addr = listener.local_addr().map_err(|error| ServerSetupError {
        code: "internal",
        category: "server_bind",
        message: error.to_string(),
    })?;
    Ok((listener, local_addr))
}

/// Builds the fully-configured server state for a startup workspace root
/// without binding a socket or writing the token (the caller owns bind → token
/// → serve). Each workspace resolves its OWN config/registry and uses the
/// shared global durable store, so sessions survive restarts and per-workspace
/// provider configuration is honored.
/// Reads an optional lease-TTL override (milliseconds) for server-owned
/// runtimes. Production leaves this unset and keeps the 60s runtime default;
/// end-to-end crash-recovery tests set `LATTE_LEASE_TTL_MS` low so an orphaned
/// lease expires within the test window and the recovery sweeper reclaims it.
fn server_lease_ttl_ms() -> Option<u64> {
    std::env::var("LATTE_LEASE_TTL_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
}

fn prepare_server(
    root: &Path,
    storage_home: &Path,
) -> Result<(std::sync::Arc<latte_server::ServerState>, String, PathBuf), ServerSetupError> {
    // Validate the startup workspace configuration up front for a fast, clear
    // usage error; per-workspace configs are (re)loaded lazily by the builder.
    AppConfig::load(root).map_err(|message| ServerSetupError {
        code: "usage",
        category: "configuration",
        message,
    })?;
    std::fs::create_dir_all(storage_home).map_err(|error| ServerSetupError {
        code: "internal",
        category: "storage_directory",
        message: format!("cannot create {}: {error}", storage_home.display()),
    })?;

    let database_path = storage_home.join("state.db");
    let conversation_root = storage_home.join("sessions");

    // Per-workspace runtime builder: each canonical workspace root loads its
    // own `.latte/latte-code.jsonc` (models/endpoints/credentials) and gets a
    // durable engine bound to the shared global database + conversation store.
    let builder_db = database_path.clone();
    let builder_conv = conversation_root.clone();
    let builder: latte_server::WorkspaceRuntimeBuilder =
        std::sync::Arc::new(move |workspace_root: &Path| {
            let (config, registry) = AppConfig::load(workspace_root)?;
            let engine = EngineBuilder::new()
                .workspace_root(workspace_root)
                .database_path(&builder_db)
                .conversation_root(&builder_conv)
                .build()
                .map_err(|error| error.to_string())?;
            // Import any v1 legacy workspace database once, the same way the
            // in-process TUI path does, so server-driven sessions see history.
            engine
                .import_legacy_workspace_database(
                    config.legacy_database_path(workspace_root),
                    wall_time_ms(),
                )
                .map_err(|error| format!("legacy import: {error}"))?;
            let factory_engine = engine.clone();
            let factory_registry = registry.clone();
            let factory: latte_headless::thread::ThreadProviderFactory =
                std::sync::Arc::new(move |binding: &ThreadProviderBindingV2| {
                    factory_registry
                        .resolve_thread_bound(binding, &factory_engine.tool_descriptors())
                        .map_err(|error| error.to_string())
                });
            let runtime = latte_headless::thread::ThreadRuntimeService::new(
                engine.clone(),
                workspace_root,
                config.thread_policy(),
                factory,
            )
            .with_verification(config.plan());
            let runtime = match server_lease_ttl_ms() {
                Some(ttl_ms) => runtime.with_lease_ttl_ms(ttl_ms),
                None => runtime,
            };
            Ok(latte_server::BuiltWorkspace {
                engine,
                runtime,
                registry: std::sync::Arc::new(registry),
            })
        });

    // Session locator: resolve a session's owning workspace from the durable
    // global catalog so reads work after a restart, before any in-memory index
    // is populated. Uses a catalog engine bound to the startup root (queries by
    // thread id are workspace-independent lookups against the shared store).
    let catalog_engine = EngineBuilder::new()
        .workspace_root(root)
        .database_path(&database_path)
        .conversation_root(&conversation_root)
        .build()
        .map_err(|error| ServerSetupError {
            code: "internal",
            category: "engine_initialization",
            message: error.to_string(),
        })?;
    let session_locator: latte_server::SessionLocator =
        std::sync::Arc::new(move |thread_id: latte_core::ThreadId| {
            catalog_engine
                .thread_session_v2(thread_id)
                .ok()
                .flatten()
                .map(|summary| PathBuf::from(summary.workspace_root))
        });

    let token = generate_server_token();
    let token_path = storage_home.join("server.token");
    Ok((
        latte_server::new_state(token.clone(), builder, session_locator),
        token,
        token_path,
    ))
}

/// Parses the optional `--port <port>` flag for `serve`.
fn parse_serve_port(args: &[String]) -> Result<u16, String> {
    let mut port = DEFAULT_SERVER_PORT;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--port requires a value".to_string())?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {value}"))?;
                index += 2;
            }
            other => return Err(format!("unexpected serve argument: {other}")),
        }
    }
    Ok(port)
}

/// Mints a Bearer token for the local server. A `UUIDv7` pair provides
/// sufficient unpredictability for the v1 local-only loopback bind.
fn generate_server_token() -> String {
    format!(
        "{}{}",
        SystemIdSource::default().next_uuid_v7().simple(),
        SystemIdSource::default().next_uuid_v7().simple()
    )
}

/// Writes the Bearer token atomically with owner-only permissions. Creates a
/// temporary file with restrictive mode, writes the token, then renames into
/// place so that the final path is never observable with partial content or
/// permissive mode.
fn write_server_token(path: &Path, token: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cannot determine parent of {}", path.display()))?;
    let tmp_path = parent.join(format!(".server.token.{}", std::process::id()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|error| format!("cannot create {}: {error}", tmp_path.display()))?;
        file.write_all(token.as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", tmp_path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, token)
            .map_err(|error| format!("cannot write {}: {error}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|error| format!("cannot publish {}: {error}", path.display()))?;
    Ok(())
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
    use super::EXIT_COMPLETED;
    use super::bind_local_listener;
    use super::{
        AppConfig, DEFAULT_SERVER_PORT, DatabaseConfig, EXIT_INTERNAL, EXIT_USAGE, ThreadConfig,
        ThreadEngineProjection, VerificationConfig, discover_workspace_root,
        dispatch_session_management_action, dot, emit_client_error, emit_data, emit_error,
        execute_serve, execute_tui, exit_for_setup, generate_server_token, merge_optional_config,
        merge_value, open_tui_engine, parse_serve_port, prepare_server, readiness_envelope,
        reconcile_thread_action, serve_bound, storage_home_with, tui_setup,
        tui_startup_presentation, verify_timeout, workspace_display_path,
        workspace_display_path_with_home, workspace_identity, write_server_token,
    };
    use latte_core::{
        IdSource, RunId, RunStatus, SystemIdSource, ThreadCommandId, ThreadId, ThreadLifecycle,
        ThreadProviderBindingV2,
    };
    use latte_engine::{
        CommitThreadRunUpdate, EngineBuilder, ThreadCommitRequest, ThreadEffectDescriptor,
        ThreadEffectRequest, ThreadEffectStartRequest,
    };
    use latte_headless::thread::{ThreadHistoryPolicy, ThreadRuntimeService};
    use latte_tui::thread::{
        SessionManagementOutcome, ThreadProjectionClient, ThreadProjectionPoll, ThreadUiAction,
        ThreadUiFeedback,
    };
    use serde_json::json;
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[test]
    fn remaining_run_statuses_and_config_value_objects_are_exact() {
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
    }

    #[test]
    fn workspace_identity_and_global_storage_home_are_canonical_and_explicit() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            workspace_identity(root.path()).unwrap(),
            std::fs::canonicalize(root.path())
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert_eq!(
            storage_home_with(None, Some(root.path())).unwrap(),
            root.path().join(".latte/latte-code")
        );
        let absolute = root.path().join("global-state");
        assert_eq!(
            storage_home_with(Some(absolute.as_os_str()), Some(Path::new("/ignored"))).unwrap(),
            absolute
        );
        assert_eq!(
            storage_home_with(Some(std::ffi::OsStr::new("")), Some(root.path())).unwrap_err(),
            "LATTE_CODE_HOME must not be empty"
        );
        assert_eq!(
            storage_home_with(Some(std::ffi::OsStr::new("relative")), Some(root.path()))
                .unwrap_err(),
            "LATTE_CODE_HOME must be an absolute path"
        );
        assert_eq!(
            storage_home_with(None, None).unwrap_err(),
            "LATTE_CODE_HOME or HOME must be set"
        );

        let configured = root.path().join("ignored.db");
        let config = AppConfig {
            version: 1,
            default_model: "primary/test".into(),
            providers: json!({}),
            database: DatabaseConfig {
                path: configured.display().to_string(),
            },
            verification: VerificationConfig {
                argv: vec!["true".into()],
                cwd: ".".into(),
                timeout_ms: 1,
            },
            thread: ThreadConfig::default(),
        };
        assert_eq!(config.database.path, configured.display().to_string());
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
        let (config, _) = AppConfig::load_with_home(root.path(), Some(home.path())).unwrap();
        assert_eq!(config.database.path, " ");
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

    #[test]
    fn tui_entrypoint_rejects_non_terminal_processes_before_loading_authority() {
        assert_eq!(execute_tui(), EXIT_USAGE);
    }

    #[test]
    fn tui_setup_reports_configuration_error_for_invalid_config() {
        // Create a temp dir with an invalid config file, then set HOME to it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".latte")).unwrap();
        std::fs::write(dir.path().join(".latte/latte-code.jsonc"), "{invalid json").unwrap();
        let result = temp_env::with_vars([("HOME", Some(dir.path().as_os_str()))], tui_setup);
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), EXIT_USAGE);
    }

    #[test]
    fn tui_setup_reports_configuration_error_when_storage_home_empty() {
        // With LATTE_CODE_HOME set to an empty string, storage_home() fails
        // and tui_setup returns EXIT_USAGE from the database_path branch.
        let dir = tempfile::tempdir().unwrap();
        let result = temp_env::with_vars(
            [
                ("HOME", Some(dir.path().as_os_str())),
                ("LATTE_CODE_HOME", Some(std::ffi::OsStr::new(""))),
            ],
            tui_setup,
        );
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), EXIT_USAGE);
    }

    #[test]
    fn server_lease_ttl_ms_parses_and_rejects_values() {
        // Valid value.
        temp_env::with_vars([("LATTE_LEASE_TTL_MS", Some("5000"))], || {
            assert_eq!(super::server_lease_ttl_ms(), Some(5000));
        });
        // Invalid value → None.
        temp_env::with_vars([("LATTE_LEASE_TTL_MS", Some("invalid"))], || {
            assert_eq!(super::server_lease_ttl_ms(), None);
        });
        // Zero → None (filtered out).
        temp_env::with_vars([("LATTE_LEASE_TTL_MS", Some("0"))], || {
            assert_eq!(super::server_lease_ttl_ms(), None);
        });
    }

    #[test]
    fn storage_paths_fail_without_home() {
        temp_env::with_vars(
            [
                ("HOME", None::<&std::ffi::OsStr>),
                ("LATTE_CODE_HOME", None::<&std::ffi::OsStr>),
            ],
            || {
                assert!(super::storage_database_path().is_err());
                assert!(super::storage_conversation_root().is_err());
            },
        );
    }

    #[test]
    fn storage_paths_succeed_with_home() {
        let temp = tempfile::tempdir().unwrap();
        temp_env::with_vars([("LATTE_CODE_HOME", Some(temp.path().as_os_str()))], || {
            let db = super::storage_database_path().unwrap();
            assert_eq!(db, temp.path().join("state.db"));
            let conv = super::storage_conversation_root().unwrap();
            assert_eq!(conv, temp.path().join("sessions"));
        });
    }

    #[test]
    fn legacy_database_path_handles_absolute_and_relative() {
        let root = Path::new("/workspace");
        // Relative path → joined with root.
        let config = AppConfig {
            version: 1,
            default_model: String::new(),
            providers: json!({}),
            database: DatabaseConfig {
                path: ".latte/latte-code.db".into(),
            },
            verification: VerificationConfig {
                argv: vec!["true".into()],
                cwd: ".".into(),
                timeout_ms: 1000,
            },
            thread: ThreadConfig::default(),
        };
        assert_eq!(
            config.legacy_database_path(root),
            root.join(".latte/latte-code.db")
        );
        // Absolute path → used as-is.
        let config = AppConfig {
            database: DatabaseConfig {
                path: "/absolute/path.db".into(),
            },
            ..config
        };
        assert_eq!(
            config.legacy_database_path(root),
            PathBuf::from("/absolute/path.db")
        );
    }

    #[test]
    fn storage_home_with_covers_all_branches() {
        use std::ffi::OsStr;
        // Override with valid absolute path (use tempdir for cross-platform).
        let temp = tempfile::tempdir().unwrap();
        let path = storage_home_with(Some(temp.path().as_os_str()), None).unwrap();
        assert_eq!(path, temp.path().to_path_buf());
        // Override with empty string → error.
        assert!(storage_home_with(Some(OsStr::new("")), None).is_err());
        // Override with relative path → error.
        assert!(storage_home_with(Some(OsStr::new("relative")), None).is_err());
        // No override, no HOME → error.
        assert!(storage_home_with(None, None).is_err());
        // No override, with HOME → ~/.latte/latte-code.
        let home = tempfile::tempdir().unwrap();
        let path = storage_home_with(None, Some(home.path())).unwrap();
        assert_eq!(path, home.path().join(".latte/latte-code"));
    }

    #[test]
    fn merge_value_deep_merges_objects() {
        let mut base = json!({
            "a": 1,
            "nested": { "x": 1, "y": 2 }
        });
        let overlay = json!({
            "a": 10,
            "b": 20,
            "nested": { "y": 20, "z": 30 }
        });
        merge_value(&mut base, overlay);
        assert_eq!(base["a"], 10);
        assert_eq!(base["b"], 20);
        assert_eq!(base["nested"]["x"], 1);
        assert_eq!(base["nested"]["y"], 20);
        assert_eq!(base["nested"]["z"], 30);
    }

    #[test]
    fn merge_value_replaces_non_object_values() {
        let mut base = json!({"a": {"nested": 1}});
        let overlay = json!({"a": "scalar"});
        merge_value(&mut base, overlay);
        assert_eq!(base["a"], "scalar");
    }

    #[test]
    fn merge_optional_config_handles_missing_and_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut base = json!({"existing": true});
        // Missing file → Ok (no merge).
        let missing = dir.path().join("missing.jsonc");
        merge_optional_config(&mut base, &missing).unwrap();
        assert_eq!(base["existing"], true);
        // Invalid JSON → Err.
        let invalid = dir.path().join("invalid.jsonc");
        std::fs::write(&invalid, "{invalid").unwrap();
        assert!(merge_optional_config(&mut base, &invalid).is_err());
        // Non-object top-level → error.
        let non_object = dir.path().join("array.jsonc");
        std::fs::write(&non_object, "[1,2,3]").unwrap();
        assert!(merge_optional_config(&mut base, &non_object).is_err());
        // Valid JSONC → merges.
        let valid = dir.path().join("valid.jsonc");
        std::fs::write(&valid, "{merged: true}").unwrap();
        merge_optional_config(&mut base, &valid).unwrap();
        assert_eq!(base["merged"], true);
        assert_eq!(base["existing"], true);
    }

    #[test]
    fn app_config_load_with_home_merges_home_config() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Write a home config that overrides the verification argv.
        std::fs::create_dir_all(home.path().join(".latte")).unwrap();
        std::fs::write(
            home.path().join(".latte/latte-code.jsonc"),
            r#"{version:1,providers:{},verification:{argv:["echo","home"]}}"#,
        )
        .unwrap();
        let (config, _registry) =
            AppConfig::load_with_home(root.path(), Some(home.path())).unwrap();
        assert_eq!(config.verification.argv, ["echo", "home"]);
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
            projection.search_session_catalog("thread-0").unwrap()[0].thread_id,
            first_thread_id
        );
        assert!(
            projection
                .search_session_catalog("missing title")
                .unwrap()
                .is_empty()
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

    #[tokio::test]
    async fn tui_session_management_adapter_renames_and_forks() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(root.path().join("state.db"))
            .conversation_root(root.path().join("sessions"))
            .build()
            .unwrap();
        let ids = SystemIdSource::default();
        let source = ThreadId::from_uuid(ids.next_uuid_v7());
        engine
            .create_thread_v2(
                source,
                RunId::from_uuid(ids.next_uuid_v7()),
                ThreadProviderBindingV2 {
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
                },
                "source prompt",
                1,
            )
            .unwrap();
        let (feedback_tx, feedback_rx) = std::sync::mpsc::channel();

        assert!(
            dispatch_session_management_action(
                &engine,
                &feedback_tx,
                ThreadUiAction::ForkSession {
                    thread_id: source,
                    title: Some("branch title".into()),
                },
            )
            .is_none()
        );
        let fork = match feedback_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
        {
            ThreadUiFeedback::SessionManagement(Ok(SessionManagementOutcome::Forked(
                thread_id,
            ))) => thread_id,
            feedback => panic!("unexpected feedback: {feedback:?}"),
        };

        assert!(
            dispatch_session_management_action(
                &engine,
                &feedback_tx,
                ThreadUiAction::RenameSession {
                    thread_id: fork,
                    title: "renamed branch".into(),
                },
            )
            .is_none()
        );
        match feedback_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
        {
            ThreadUiFeedback::SessionManagement(Ok(SessionManagementOutcome::Updated(message))) => {
                assert!(message.contains("renamed branch"));
            }
            feedback => panic!("unexpected feedback: {feedback:?}"),
        }

        let missing = ThreadId::from_uuid(ids.next_uuid_v7());
        assert!(
            dispatch_session_management_action(
                &engine,
                &feedback_tx,
                ThreadUiAction::RenameSession {
                    thread_id: missing,
                    title: "missing".into(),
                },
            )
            .is_none()
        );
        assert!(matches!(
            feedback_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            ThreadUiFeedback::SessionManagement(Err(_))
        ));
        assert!(matches!(
            dispatch_session_management_action(&engine, &feedback_tx, ThreadUiAction::Quit,),
            Some(ThreadUiAction::Quit)
        ));
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
        let engine = open_tui_engine(
            root.path(),
            &config,
            &root.path().join("global/state.db"),
            &root.path().join("global/sessions"),
            1,
        )
        .unwrap();
        let (binding, startup) = tui_startup_presentation(root.path(), &registry, &engine).unwrap();
        assert_eq!(binding.unwrap().model, "model-id");
        assert_eq!(startup.default_provider, "primary");
        assert_eq!(startup.default_model, "model-id");
        assert_eq!(startup.model_catalog.len(), 1);
        assert!(!startup.workspace_display.is_empty());
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
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let (binding, startup) = tui_startup_presentation(root.path(), &registry, &engine).unwrap();
        assert!(binding.is_none());
        assert!(startup.default_provider.is_empty());
        assert!(startup.default_model.is_empty());
        assert!(startup.model_catalog.is_empty());
        assert!(
            open_tui_engine(
                root.path(),
                &config,
                Path::new(""),
                &root.path().join("sessions"),
                1,
            )
            .is_err()
        );
        std::fs::create_dir_all(root.path().join(".latte")).unwrap();
        std::fs::write(config.legacy_database_path(root.path()), b"not sqlite").unwrap();
        assert!(
            open_tui_engine(
                root.path(),
                &config,
                &root.path().join("global/state.db"),
                &root.path().join("global/sessions"),
                2,
            )
            .is_err()
        );
        let configured_without_default = latte_headless::registry::ProviderRegistry::parse_jsonc(
            r#"{
                version: 1,
                default_model: "primary/model-id",
                providers: { primary: {
                    type: "openai-chat",
                    models: { "model-id": {} },
                    base_url: "https://provider.example/v1",
                    api_key: { source: "env", name: "TEST_PROVIDER_KEY" },
                    aliases: { unknown_tool: "unknown_alias" }
                } }
            }"#,
        )
        .unwrap();
        assert!(!configured_without_default.model_catalog().is_empty());
        assert!(
            tui_startup_presentation(root.path(), &configured_without_default, &engine).is_err()
        );
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
                    api_key: { source: "env", name: "TEST_PROVIDER_KEY" }
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

    #[test]
    fn serve_port_parsing_covers_default_explicit_and_error_shapes() {
        assert_eq!(parse_serve_port(&[]).unwrap(), DEFAULT_SERVER_PORT);
        assert_eq!(parse_serve_port(&["--port".into(), "0".into()]).unwrap(), 0);
        assert_eq!(
            parse_serve_port(&["--port".into(), "8080".into()]).unwrap(),
            8080
        );
        assert_eq!(
            parse_serve_port(&["--port".into()]).unwrap_err(),
            "--port requires a value"
        );
        assert_eq!(
            parse_serve_port(&["--port".into(), "70000".into()]).unwrap_err(),
            "invalid port: 70000"
        );
        assert_eq!(
            parse_serve_port(&["--bogus".into()]).unwrap_err(),
            "unexpected serve argument: --bogus"
        );
    }

    #[test]
    fn server_token_is_unpredictable_and_written_with_owner_only_permissions() {
        let first = generate_server_token();
        let second = generate_server_token();
        assert_ne!(first, second, "each token must be distinct");
        assert!(first.len() >= 32, "token must carry sufficient entropy");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.token");
        write_server_token(&path, &first).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        }

        let missing = dir.path().join("no-such-dir").join("server.token");
        let err = write_server_token(&missing, &first).unwrap_err();
        assert!(
            err.contains("cannot create") || err.contains("cannot write"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn prepare_server_builds_durable_per_workspace_runtime() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".latte")).unwrap();
        std::fs::write(
            root.path().join(".latte/latte-code.jsonc"),
            r#"{
                version: 1,
                default_model: "primary/model",
                providers: { primary: {
                    type: "openai-chat",
                    models: { "model": {} },
                    base_url: "https://provider.example/v1",
                    api_key: { source: "env", name: "TEST_PROVIDER_KEY" }
                } },
                verification: { argv: ["true"] }
            }"#,
        )
        .unwrap();
        let storage_home = home.path().join(".latte/latte-code");

        let (state, token, token_path) = match prepare_server(root.path(), &storage_home) {
            Ok(value) => value,
            Err(error) => panic!("prepare_server failed: {}", error.message),
        };
        assert!(!token.is_empty(), "prepared state must carry a token");
        assert_eq!(state.token, token, "state token matches the returned token");
        assert_eq!(token_path, storage_home.join("server.token"));
        // prepare_server does not write the token (bind happens first).
        assert!(!token_path.exists());

        // The runtime builder produces a durable engine for the workspace, so
        // the global database materializes under the storage home.
        let workspace = state.workspaces.get_or_create(root.path()).await.unwrap();
        assert!(workspace.list_sessions().unwrap().is_empty());
        assert!(storage_home.join("state.db").is_file());

        // The durable session locator resolves nothing for an unknown thread.
        let missing = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        assert!(
            state
                .workspaces
                .get_session_workspace(&missing)
                .await
                .is_none()
        );
    }

    #[test]
    fn prepare_server_reports_invalid_configuration() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".latte")).unwrap();
        // An empty verification.argv is a documented configuration error.
        std::fs::write(
            root.path().join(".latte/latte-code.jsonc"),
            "{ verification: { argv: [] } }",
        )
        .unwrap();

        let Err(error) = prepare_server(root.path(), &home.path().join("state")) else {
            panic!("expected invalid configuration to be rejected");
        };
        assert_eq!(error.code, "usage");
        assert_eq!(error.category, "configuration");
        assert!(error.message.contains("verification.argv"));
    }

    #[test]
    fn exit_for_setup_maps_usage_and_internal_codes() {
        assert_eq!(exit_for_setup("usage"), EXIT_USAGE);
        assert_eq!(exit_for_setup("internal"), EXIT_INTERNAL);
    }

    #[tokio::test]
    async fn execute_serve_rejects_unknown_argument_before_binding() {
        // An unparseable serve argument is a usage error returned before any
        // socket bind or configuration load.
        assert_eq!(
            execute_serve(&["--nonsense".to_string()], true).await,
            EXIT_USAGE
        );
    }

    #[tokio::test]
    async fn bind_local_listener_binds_ephemeral_and_reports_in_use() {
        // An ephemeral port binds and resolves a loopback address.
        let (listener, addr) = bind_local_listener(0).await.expect("ephemeral bind");
        assert!(addr.ip().is_loopback());
        let port = addr.port();

        // Binding the same port again is a classified server_bind failure.
        let Err(error) = bind_local_listener(port).await else {
            panic!("expected the in-use port to fail");
        };
        assert_eq!(error.code, "internal");
        assert_eq!(error.category, "server_bind");
        drop(listener);
    }

    #[test]
    fn readiness_envelope_reports_address_port_and_token_path() {
        let addr: std::net::SocketAddr = "127.0.0.1:4096".parse().unwrap();
        let token_path = Path::new("/tmp/latte/server.token");
        let envelope = readiness_envelope(addr, token_path);
        assert_eq!(envelope["address"], "127.0.0.1:4096");
        assert_eq!(envelope["port"], 4096);
        assert_eq!(envelope["token_path"], "/tmp/latte/server.token");
    }

    /// Redirects the process-global environment `serve_bound` reads
    /// (`LATTE_CODE_HOME`, and `HOME` for the optional user config layer) into
    /// scratch directories, so server tests never touch the developer's real
    /// state or token file. The mutex serializes env-mutating tests against
    /// each other; other tests in this crate only read `HOME` for display
    /// paths and tolerate a temporary value, and other test binaries are
    /// separate processes.
    static SERVE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `closure` with `HOME` and `LATTE_CODE_HOME` pointed at scratch
    /// directories, restoring the previous values on completion.
    fn with_scratch_serve_env<R>(
        home: &Path,
        storage_home: &Path,
        closure: impl FnOnce() -> R,
    ) -> R {
        let _lock = SERVE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        temp_env::with_vars(
            [
                ("HOME", Some(home.as_os_str())),
                ("LATTE_CODE_HOME", Some(storage_home.as_os_str())),
            ],
            closure,
        )
    }

    /// A scratch storage layout under a temporary HOME: `serve_bound` resolves
    /// `$LATTE_CODE_HOME` (and the optional `$HOME` config layer) from the
    /// process environment, so tests point both at scratch directories.
    /// Also creates a `.git` marker and a minimal workspace config so
    /// `discover_workspace_root` and `AppConfig::load` resolve to the scratch
    /// directory rather than the developer's real workspace.
    fn scratch_serve_env() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".git")).unwrap();
        write_valid_workspace_config(home.path());
        let storage_home = home.path().join(".latte/latte-code");
        (home, storage_home)
    }

    /// Polls the loopback port until the server accepts a connection.
    async fn wait_for_listening(port: u16) {
        for _ in 0..200 {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("server never accepted a connection on 127.0.0.1:{port}");
    }

    /// Writes a minimal valid workspace configuration so `prepare_server`
    /// passes its up-front configuration validation.
    fn write_valid_workspace_config(root: &Path) {
        std::fs::create_dir_all(root.join(".latte")).unwrap();
        std::fs::write(
            root.join(".latte/latte-code.jsonc"),
            r#"{
                version: 1,
                default_model: "primary/model",
                providers: { primary: {
                    type: "openai-chat",
                    models: { "model": {} },
                    base_url: "https://provider.example/v1",
                    api_key: { source: "env", name: "TEST_PROVIDER_KEY" }
                } },
                verification: { argv: ["true"] }
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn execute_serve_classifies_setup_errors_through_the_uniform_emitter() {
        let (home, storage_home) = scratch_serve_env();
        with_scratch_serve_env(home.path(), &storage_home, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                // Occupy a port: serve_bound finishes full preparation, then bind fails
                // and the classified error routes through execute_serve's emitter.
                let (listener, addr) = bind_local_listener(0).await.unwrap();
                let occupied = addr.port();
                let code = execute_serve(&["--port".to_string(), occupied.to_string()], true).await;
                assert_eq!(code, EXIT_INTERNAL);
                drop(listener);
            })
        })
    }

    #[test]
    fn serve_bound_announces_listening_writes_token_and_serves_health() {
        let (home, storage_home) = scratch_serve_env();
        with_scratch_serve_env(home.path(), &storage_home, || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                for json in [true, false] {
                    // Reserve a likely-free port, then hand it to serve_bound.
                    let (probe, addr) = bind_local_listener(0).await.unwrap();
                    let port = addr.port();
                    drop(probe);

                    let server = tokio::spawn(async move { serve_bound(port, json).await });
                    wait_for_listening(port).await;

                    // The token is published after bind, before serving starts.
                    // Retry briefly for slow filesystems (e.g. Windows CI).
                    let token_path = storage_home.join("server.token");
                    let mut token_published = false;
                    for _ in 0..100 {
                        if token_path.is_file() {
                            token_published = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(
                        token_published,
                        "token must be published after the listener binds"
                    );
                    let token = std::fs::read_to_string(&token_path).unwrap();
                    assert!(token.len() >= 32, "token must carry sufficient entropy");

                    // The health endpoint answers on the bound port without a token.
                    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                    stream
                        .write_all(
                            b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    let mut response = Vec::new();
                    stream.read_to_end(&mut response).await.unwrap();
                    assert!(
                        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
                        "unexpected response: {}",
                        String::from_utf8_lossy(&response)
                    );

                    // serve_on blocks on process signals; abort the task like the
                    // latte-server tests do, since signals are process-wide.
                    server.abort();
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
        })
    }

    #[test]
    fn serve_bound_rejects_relative_storage_home() {
        let home = tempfile::tempdir().unwrap();
        with_scratch_serve_env(home.path(), Path::new("relative/state"), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let Err(error) = rt.block_on(serve_bound(0, true)) else {
                panic!("expected a relative LATTE_CODE_HOME to be rejected");
            };
            assert_eq!(error.code, "usage");
            assert_eq!(error.category, "configuration");
        });
    }

    #[test]
    fn serve_bound_reports_token_write_failure() {
        let home = tempfile::tempdir().unwrap();
        let storage_home = home.path().join(".latte/latte-code");
        std::fs::create_dir_all(&storage_home).unwrap();
        // A directory at the token path makes the atomic publish fail.
        std::fs::create_dir_all(storage_home.join("server.token")).unwrap();
        with_scratch_serve_env(home.path(), &storage_home, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let Err(error) = rt.block_on(serve_bound(0, true)) else {
                panic!("expected the token write to fail");
            };
            assert_eq!(error.code, "internal");
            assert_eq!(error.category, "server_token");
        });
    }

    #[tokio::test]
    async fn workspace_builder_reports_provider_resolution_failure() {
        let root = tempfile::tempdir().unwrap();
        write_valid_workspace_config(root.path());
        let home = tempfile::tempdir().unwrap();
        let storage_home = home.path().join("state");
        let (state, _token, _token_path) = match prepare_server(root.path(), &storage_home) {
            Ok(value) => value,
            Err(error) => panic!("prepare_server failed: {}", error.message),
        };
        let workspace = state.workspaces.get_or_create(root.path()).await.unwrap();

        // A binding whose provider is no longer configured: the per-workspace
        // factory built inside prepare_server is invoked when the thread
        // starts, and its resolution failure surfaces as a retryable child
        // failure rather than a startup error.
        let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let binding = ThreadProviderBindingV2 {
            version: 1,
            provider_name: "missing".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "model".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:MISSING".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        let snapshot = workspace
            .runtime
            .start(thread_id, "hello".into(), binding, None)
            .await
            .unwrap();
        assert_eq!(
            snapshot.lifecycle,
            ThreadLifecycle::Ready,
            "provider construction failure must be a retryable child failure"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_bound_completes_gracefully_on_sigterm() {
        // SIGTERM is process-wide, so re-execute this test in a child process
        // with a scratch environment; the child signals itself, leaving the
        // parent test process and other test binaries untouched. The child
        // inherits the coverage profile with a pid suffix (the E2E harness
        // trick) so its executed lines are recorded.
        if std::env::var_os("LATTE_CODE_SIGTERM_TEST").is_none() {
            let home = tempfile::tempdir().unwrap();
            let storage_home = home.path().join(".latte/latte-code");
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("serve_bound_completes_gracefully_on_sigterm")
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .stdin(std::process::Stdio::null())
                .env("HOME", home.path())
                .env("LATTE_CODE_HOME", &storage_home)
                .env("LATTE_CODE_SIGTERM_TEST", "1");
            if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
                command.env(
                    "LLVM_PROFILE_FILE",
                    profile.replace(".profraw", "-%p.profraw"),
                );
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "child test failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // Child: reserve a port, run execute_serve, then SIGTERM self once the
        // health endpoint answers (proving serve_on is polling its shutdown
        // signal, so the signal triggers graceful shutdown instead of killing
        // the process).
        let (probe, addr) = bind_local_listener(0).await.unwrap();
        let port = addr.port();
        drop(probe);
        let args = vec!["--port".to_string(), port.to_string()];
        let server = tokio::spawn(async move { execute_serve(&args, true).await });
        wait_for_listening(port).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
            "unexpected response: {}",
            String::from_utf8_lossy(&response)
        );

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(std::process::id()).unwrap()),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let code = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server did not shut down within the deadline")
            .expect("serve task panicked");
        assert_eq!(code, EXIT_COMPLETED);
    }

    #[test]
    fn prepare_server_reports_uncreateable_storage_directory() {
        let root = tempfile::tempdir().unwrap();
        write_valid_workspace_config(root.path());
        let home = tempfile::tempdir().unwrap();
        let blocker = home.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let storage_home = blocker.join("state");

        let Err(error) = prepare_server(root.path(), &storage_home) else {
            panic!("expected storage directory creation to fail");
        };
        assert_eq!(error.code, "internal");
        assert_eq!(error.category, "storage_directory");
        assert!(
            error.message.contains("cannot create"),
            "unexpected message: {}",
            error.message
        );
    }

    #[test]
    fn prepare_server_reports_engine_initialization_failure() {
        let root = tempfile::tempdir().unwrap();
        write_valid_workspace_config(root.path());
        let home = tempfile::tempdir().unwrap();
        let storage_home = home.path().join("state");
        std::fs::create_dir_all(&storage_home).unwrap();
        // A directory at the database path makes the SQLite open fail.
        std::fs::create_dir_all(storage_home.join("state.db")).unwrap();

        let Err(error) = prepare_server(root.path(), &storage_home) else {
            panic!("expected engine initialization to fail");
        };
        assert_eq!(error.code, "internal");
        assert_eq!(error.category, "engine_initialization");
    }

    // -- Pure helper coverage ------------------------------------------------

    #[test]
    fn workspace_display_path_with_home_covers_all_branches() {
        let root = Path::new("/home/user/project");
        // No HOME → raw path.
        assert_eq!(
            workspace_display_path_with_home(root, None),
            "/home/user/project"
        );
        // Not under HOME → raw path.
        assert_eq!(
            workspace_display_path_with_home(root, Some(Path::new("/other"))),
            "/home/user/project"
        );
        // Under HOME → ~/relative.
        assert_eq!(
            workspace_display_path_with_home(root, Some(Path::new("/home/user"))),
            "~/project"
        );
        // IS home → ~.
        assert_eq!(
            workspace_display_path_with_home(
                Path::new("/home/user"),
                Some(Path::new("/home/user"))
            ),
            "~"
        );
    }

    #[test]
    fn emit_error_covers_json_help_and_plain_branches() {
        // JSON branch.
        let code = emit_error(true, "failed", "test_code", "json message", 1, false);
        assert_eq!(code, 1);
        // Plain text branch (no help).
        let code = emit_error(false, "failed", "test_code", "plain message", 2, false);
        assert_eq!(code, 2);
        // Help branch.
        let code = emit_error(false, "failed", "test_code", "help message", 2, true);
        assert_eq!(code, 2);
    }

    #[test]
    fn emit_data_outputs_envelope() {
        emit_data("completed", &json!({"session": "test"}));
    }

    #[test]
    fn emit_client_error_maps_all_variants() {
        use crate::server_client::ClientError;
        assert_eq!(
            emit_client_error(true, &ClientError::Unreachable("x".into())),
            71
        );
        assert_eq!(emit_client_error(true, &ClientError::Usage("x".into())), 2);
        assert_eq!(
            emit_client_error(true, &ClientError::NotFound("x".into())),
            4
        );
        assert_eq!(
            emit_client_error(true, &ClientError::Unauthorized("x".into())),
            70
        );
        assert_eq!(
            emit_client_error(true, &ClientError::Internal("x".into())),
            70
        );
        assert_eq!(
            emit_client_error(true, &ClientError::Conflict("x".into())),
            1
        );
        assert_eq!(emit_client_error(true, &ClientError::Failed("x".into())), 1);
        // Non-JSON branch.
        assert_eq!(
            emit_client_error(false, &ClientError::Failed("x".into())),
            1
        );
    }

    // ------------------------------------------------------------------
    // Session command coverage (execute_session_command / _inner)
    // ------------------------------------------------------------------

    /// Starts a minimal HTTP mock server. `handler` receives (method, path)
    /// and returns (status, `content_type`, body). One request per connection.
    fn start_session_mock_server<F>(mut handler: F) -> (String, std::thread::JoinHandle<()>)
    where
        F: FnMut(&str, &str) -> (u16, String, String) + Send + 'static,
    {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 8192];
                let Ok(n) = stream.read(&mut buf) else {
                    continue;
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let path = parts.next().unwrap_or("/");
                let (status, content_type, body) = handler(method, path);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    /// A terminal-ready snapshot JSON for mock server responses.
    fn terminal_snapshot_json(thread_id: &str, run_id: &str) -> String {
        format!(
            r#"{{"snapshot":{{"thread_id":"{thread_id}","revision":1,"sequence":1,"lifecycle":"ready","binding":{{"version":2,"provider_name":"test","provider_type":"test","protocol":"test","model":"test","config_fingerprint":"test","tools_fingerprint":"test","aliases":{{}},"credential_ref_id":"test","data_scope_id":"test","credential_generation":1}},"latest_run_id":"{run_id}","active_run_id":null,"runs":[{{"run_id":"{run_id}","parent_run_id":null,"ordinal":1,"status":"completed","run_revision":1,"completed_at_ms":1234567890,"failure_code":null}}],"transcript":{{"entries":[],"next_after":null,"has_more":false}}}}}}"#
        )
    }

    #[tokio::test]
    async fn execute_session_command_rejects_unknown_subcommand() {
        let args = vec!["bogus".to_string()];
        let code = super::execute_session_command(true, &args).await;
        assert_eq!(code, EXIT_USAGE);
    }

    #[test]
    fn execute_session_command_reports_unreachable_server() {
        let temp = tempfile::tempdir().unwrap();
        temp_env::with_vars([("LATTE_CODE_HOME", Some(temp.path().as_os_str()))], || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let args = vec![
                    "list".to_string(),
                    "--server".to_string(),
                    "http://127.0.0.1:0".to_string(),
                    "--token".to_string(),
                    "dummy".to_string(),
                ];
                let code = super::execute_session_command(true, &args).await;
                assert_eq!(code, 71); // EXIT_SERVER_UNREACHABLE
            });
        });
    }

    #[tokio::test]
    async fn execute_session_command_inner_list_returns_sessions() {
        let (url, _handle) = start_session_mock_server(|method, path| {
            if path == "/v1/workspaces" && method == "POST" {
                (
                    200,
                    "application/json".into(),
                    r#"{"workspace_id":"ws-test"}"#.into(),
                )
            } else if path == "/v1/workspaces/ws-test/sessions" {
                (200, "application/json".into(), r#"{"sessions":[]}"#.into())
            } else {
                (
                    404,
                    "application/json".into(),
                    r#"{"error":"not found"}"#.into(),
                )
            }
        });
        let mut client = crate::server_client::ServerClient::new(url, "dummy".into());
        let root = std::path::Path::new("/tmp");
        let command = crate::server_client::SessionCommand::List;
        let cancel = std::future::pending::<()>();
        let result =
            super::execute_session_command_inner(&mut client, command, root, true, cancel).await;
        assert_eq!(result.unwrap(), EXIT_COMPLETED);
    }

    #[tokio::test]
    async fn execute_session_command_inner_show_returns_snapshot() {
        let thread_id = uuid::Uuid::now_v7().to_string();
        let run_id = uuid::Uuid::now_v7().to_string();
        let snapshot = terminal_snapshot_json(&thread_id, &run_id);
        let (url, _handle) = start_session_mock_server(move |_method, path| {
            if path.starts_with("/v1/sessions/") {
                (200, "application/json".into(), snapshot.clone())
            } else {
                (
                    404,
                    "application/json".into(),
                    r#"{"error":"not found"}"#.into(),
                )
            }
        });
        let mut client = crate::server_client::ServerClient::new(url, "dummy".into());
        let root = std::path::Path::new("/tmp");
        let command = crate::server_client::SessionCommand::Show {
            session_id: thread_id,
        };
        let cancel = std::future::pending::<()>();
        let result =
            super::execute_session_command_inner(&mut client, command, root, true, cancel).await;
        assert_eq!(result.unwrap(), EXIT_COMPLETED);
    }

    #[tokio::test]
    async fn execute_session_command_inner_run_completes() {
        let thread_id = uuid::Uuid::now_v7().to_string();
        let run_id = uuid::Uuid::now_v7().to_string();
        let snapshot = terminal_snapshot_json(&thread_id, &run_id);
        let (url, _handle) = start_session_mock_server(move |method, path| {
            if path == "/v1/workspaces" && method == "POST" {
                (
                    200,
                    "application/json".into(),
                    r#"{"workspace_id":"ws-test"}"#.into(),
                )
            } else if path == "/v1/workspaces/ws-test/bindings" {
                (
                    200,
                    "application/json".into(),
                    r#"{"bindings":[{"is_default":true,"binding":{"version":2,"provider_name":"test","provider_type":"test","protocol":"test","model":"test","config_fingerprint":"test","tools_fingerprint":"test","aliases":{},"credential_ref_id":"test","data_scope_id":"test","credential_generation":1}}]}"#.into(),
                )
            } else if path == "/v1/workspaces/ws-test/sessions" && method == "POST" {
                (
                    200,
                    "application/json".into(),
                    r#"{"accepted_revision":1}"#.into(),
                )
            } else if path.starts_with("/v1/sessions/") {
                (200, "application/json".into(), snapshot.clone())
            } else {
                (
                    404,
                    "application/json".into(),
                    r#"{"error":"not found"}"#.into(),
                )
            }
        });
        let mut client = crate::server_client::ServerClient::new(url, "dummy".into());
        let root = std::path::Path::new("/tmp");
        let command = crate::server_client::SessionCommand::Run {
            prompt: "hello".to_string(),
            focus: None,
        };
        let cancel = std::future::pending::<()>();
        let result =
            super::execute_session_command_inner(&mut client, command, root, true, cancel).await;
        assert_eq!(result.unwrap(), EXIT_COMPLETED);
    }

    #[test]
    fn execute_session_command_merges_json_flag_from_args() {
        // json=false but args include --json → parsed.json=true → json becomes true.
        // The connect will fail (unreachable), but the merge line is covered.
        let temp = tempfile::tempdir().unwrap();
        let args = vec![
            "list".to_string(),
            "--json".to_string(),
            "--server".to_string(),
            "http://127.0.0.1:0".to_string(),
            "--token".to_string(),
            "dummy".to_string(),
        ];
        let code =
            temp_env::with_vars([("LATTE_CODE_HOME", Some(temp.path().as_os_str()))], || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(super::execute_session_command(false, &args))
            });
        assert_eq!(code, 71); // EXIT_SERVER_UNREACHABLE
    }

    #[test]
    fn parse_session_command_accepts_focus_flag_for_run() {
        let args = vec![
            "run".to_string(),
            "hello".to_string(),
            "--focus".to_string(),
            "src/main.rs".to_string(),
        ];
        let parsed = crate::server_client::parse_session_command(&args).unwrap();
        assert!(!parsed.json);
        match parsed.command {
            crate::server_client::SessionCommand::Run { prompt, focus } => {
                assert_eq!(prompt, "hello");
                assert_eq!(focus, Some(std::path::PathBuf::from("src/main.rs")));
            }
            _ => panic!("expected Run command"),
        }
    }
}
