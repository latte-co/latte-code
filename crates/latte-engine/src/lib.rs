//! Engine actor and its private durable state authority.
#![allow(clippy::missing_errors_doc)]
pub mod config;
mod policy;
mod process;
mod storage;
mod tools;
mod workspace;

pub(crate) use latte_core::wall_time_ms as wall_now_ms;
use latte_core::{
    EventEnvelope, RunId, RunState, ThreadEventEnvelope, ThreadId, ThreadProviderBindingV2,
    ThreadSnapshot, Transition,
};
pub use process::{
    CancellationToken, ProcessDecision, ProcessError, ProcessInvocation, ProcessOutput,
    ProcessTermination, classify,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};
pub use storage::{
    CommitThreadRunUpdate, EffectStatus, Lease, LeaseLossRecovery, StorageError, StoredEvent,
    StoredThreadEvent, ThreadCommitRequest, ThreadCommitResponse, ThreadEffectPolicy,
    ThreadLeaseLossRecovery,
};
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
fn manifest_map_digest(
    manifest: &std::collections::BTreeMap<String, String>,
) -> Result<String, StorageError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_thread_effect_descriptor(
    descriptor: &ThreadEffectDescriptor,
) -> Result<(), StorageError> {
    for value in [&descriptor.effect_id, &descriptor.name] {
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(StorageError::InvalidData(
                "invalid thread effect descriptor identifier".into(),
            ));
        }
    }
    if !latte_core::valid_openai_chat_tool_call_id(&descriptor.tool_call_id) {
        return Err(StorageError::InvalidData(
            "invalid provider tool call identifier".into(),
        ));
    }
    if descriptor.attempt == 0 || !descriptor.input.is_object() {
        return Err(StorageError::InvalidData(
            "invalid thread effect descriptor input".into(),
        ));
    }
    Ok(())
}

fn thread_effect_checkpoint(
    phase: &str,
    descriptor: &ThreadEffectDescriptor,
    operation_digest: &str,
) -> String {
    serde_json::json!({
        "thread_effect": {
            "phase": phase,
            "effect_id": latte_core::redact_thread_text(&descriptor.effect_id),
            "tool_call_id": latte_core::redact_thread_text(&descriptor.tool_call_id),
            "name": latte_core::redact_thread_text(&descriptor.name),
            "operation_digest": latte_core::redact_thread_text(operation_digest),
        }
    })
    .to_string()
}

const PERMISSION_SUMMARY_CAP: usize = 360;

fn summary_text(value: &str) -> String {
    let sanitized = latte_core::redact_thread_text(value);
    let mut output = String::with_capacity(sanitized.len().min(PERMISSION_SUMMARY_CAP));
    for ch in sanitized.chars() {
        if ch.is_control() {
            continue;
        }
        if output.len() + ch.len_utf8() > PERMISSION_SUMMARY_CAP {
            output.push('…');
            break;
        }
        output.push(ch);
    }
    if output.is_empty() {
        "[unspecified]".into()
    } else {
        output
    }
}

fn input_string(input: &Value, key: &str, fallback: &str) -> String {
    input
        .get(key)
        .and_then(Value::as_str)
        .map_or_else(|| fallback.into(), summary_text)
}

fn summary_argv(value: &str) -> String {
    let safe = summary_text(value);
    let Some((key, _value)) = safe.split_once('=') else {
        return safe;
    };
    let key_lower = key.to_ascii_lowercase();
    if [
        "secret",
        "token",
        "password",
        "api_key",
        "apikey",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|needle| key_lower.contains(needle))
    {
        format!("{key}=[REDACTED]")
    } else {
        safe
    }
}

/// Produces the durable, redacted operation context shown before explicit
/// approval. It intentionally summarizes content shape rather than rendering
/// raw content, while preserving enough target/invocation detail for a user
/// to distinguish the requested operation.
fn thread_effect_permission_summary(descriptor: &ThreadEffectDescriptor) -> String {
    let input = &descriptor.input;
    let summary = match descriptor.name.as_str() {
        "write_file" => {
            let path = input_string(input, "path", "[unknown path]");
            let content_bytes = input
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let intent = if input
                .get("create_intent")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "create or replace"
            } else {
                "replace existing"
            };
            format!("Write {path} ({intent}; {content_bytes} bytes of content)")
        }
        "edit_file" => {
            let path = input_string(input, "path", "[unknown path]");
            let before = input
                .get("before")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            let after = input
                .get("after")
                .and_then(Value::as_str)
                .map_or(0, str::len);
            format!("Edit {path} (replace one match; {before} bytes → {after} bytes)")
        }
        "process" => {
            let cwd = input_string(input, "cwd", ".");
            let argv = input.get("argv").and_then(Value::as_array).map(|argv| {
                argv.iter()
                    .filter_map(Value::as_str)
                    .map(summary_argv)
                    .collect::<Vec<_>>()
                    .join(" ")
            });
            match argv.filter(|argv| !argv.is_empty()) {
                Some(argv) => format!("Run argv: {argv} (cwd: {cwd})"),
                None => format!("Run shell command (cwd: {cwd})"),
            }
        }
        "read_file" | "list_directory" => {
            let verb = if descriptor.name == "read_file" {
                "Read"
            } else {
                "List"
            };
            format!("{verb} {}", input_string(input, "path", "[unknown path]"))
        }
        "search" => format!(
            "Search workspace for {}",
            input_string(input, "query", "[unspecified query]")
        ),
        _ => format!("Run {} invocation", summary_text(&descriptor.name)),
    };
    summary_text(&summary)
}

fn run_revision(snapshot: &ThreadSnapshot, _effect_id: &str) -> Option<u64> {
    snapshot.active_run_id.and_then(|run_id| {
        snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .map(|run| run.run_revision)
    })
}
#[cfg(test)]
type CompletionHook = Arc<std::sync::Mutex<Option<Arc<dyn Fn(u8) + Send + Sync>>>>;
type CompletionSnapshot = (String, std::collections::BTreeMap<String, String>);
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use tools::{ToolDescriptor, ToolError, ToolInvocation, ToolOutput};

/// Exact engine-private description of one provider-issued v2 tool call.
///
/// This value is accepted at preparation and then retained only in the
/// engine's private descriptor store. It must never be reconstructed from a
/// transcript card, checkpoint, event, or provider-history message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadEffectDescriptor {
    pub effect_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub input: Value,
    pub attempt: u64,
}

/// Exact fenced inputs for an engine-owned v2 effect operation.
#[derive(Clone, Debug)]
pub struct ThreadEffectRequest {
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub expected_thread_revision: u64,
    pub expected_run_revision: u64,
    pub command_id: latte_core::ThreadCommandId,
    pub source_key: String,
    pub descriptor: ThreadEffectDescriptor,
}

/// Fenced start request for a previously prepared v2 effect. The caller names
/// the durable effect but cannot supply or alter its executable descriptor.
#[derive(Clone, Debug)]
pub struct ThreadEffectStartRequest {
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub expected_thread_revision: u64,
    pub expected_run_revision: u64,
    pub command_id: latte_core::ThreadCommandId,
    pub source_key: String,
    pub effect_id: String,
}

/// Result of preparing an effect. Ask is returned only after a durable
/// pending permission has been committed.
#[derive(Clone, Debug)]
pub struct ThreadEffectPrepared {
    pub snapshot: ThreadSnapshot,
    pub policy: ThreadEffectPolicy,
    pub operation_digest: String,
}

/// Safe display projection accompanying an engine-started v2 effect. This is
/// intentionally distinct from the exact descriptor held by `ThreadEffectStarted`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadEffectPresentation {
    pub effect_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub input: Value,
    pub attempt: u64,
}

impl ThreadEffectPresentation {
    fn from_descriptor(descriptor: &ThreadEffectDescriptor) -> Self {
        Self {
            effect_id: latte_core::redact_thread_text(&descriptor.effect_id),
            tool_call_id: latte_core::redact_thread_text(&descriptor.tool_call_id),
            name: latte_core::redact_thread_text(&descriptor.name),
            input: latte_core::redact_thread_value(descriptor.input.clone()),
            attempt: descriptor.attempt,
        }
    }
}

/// Durable authority returned after the Started transaction. Holding this
/// value does not itself execute anything; callers must explicitly invoke the
/// engine external execution method. Its exact descriptor is private to the
/// engine; coordinators receive only `presentation`.
#[derive(Clone, Debug)]
pub struct ThreadEffectStarted {
    pub snapshot: ThreadSnapshot,
    pub presentation: ThreadEffectPresentation,
    pub operation_digest: String,
    descriptor: ThreadEffectDescriptor,
}

/// Certified result delivered to a provider as a tool message only after the
/// observation transaction succeeds.
#[derive(Clone, Debug)]
pub struct ThreadEffectObserved {
    pub snapshot: ThreadSnapshot,
    pub result: String,
    pub success: bool,
}

/// Uncommitted external execution output.  It is intentionally not exposed to
/// a provider until `observe_thread_effect` has committed it.
#[derive(Clone, Debug)]
pub struct ThreadEffectObservedValue {
    pub result: String,
    pub payload: Option<Value>,
    pub success: bool,
}

/// An execution error is deliberately classified before the v2 caller chooses
/// whether it may write an observed failure or must conservatively write
/// Unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadEffectExecutionError {
    Certified(String),
    Uncertain(String),
}

/// Verifier-produced evidence payload persisted under fenced run authority.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VerificationEvidence<'a> {
    /// Stable evidence identifier.
    pub id: &'a str,
    /// Structured evidence metadata.
    pub metadata_json: &'a str,
    /// Optional reference to an engine-external opaque artifact.
    pub blob_ref: Option<&'a str>,
}

/// Configures and starts an engine actor.
#[derive(Debug, Default)]
pub struct EngineBuilder {
    database_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    enabled_tools: Option<BTreeSet<String>>,
    deny_globs: Vec<String>,
}
impl EngineBuilder {
    /// Creates a builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            database_path: None,
            workspace_root: None,
            enabled_tools: None,
            deny_globs: Vec::new(),
        }
    }
    /// Selects the trusted workspace root for engine-owned tools.
    #[must_use]
    pub fn workspace_root(mut self, root: impl AsRef<Path>) -> Self {
        self.workspace_root = Some(root.as_ref().to_owned());
        self
    }
    /// Enables exactly the named tools. An empty set disables every tool.
    #[must_use]
    pub fn enabled_tools(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.enabled_tools = Some(names.into_iter().map(Into::into).collect());
        self
    }
    /// Adds workspace-relative glob patterns denied before any tool executes.
    #[must_use]
    pub fn deny_globs(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.deny_globs = patterns.into_iter().map(Into::into).collect();
        self
    }
    /// Selects the authoritative `SQLite` database. The default is process-local memory.
    #[must_use]
    pub fn database_path(mut self, path: impl AsRef<Path>) -> Self {
        self.database_path = Some(path.as_ref().to_owned());
        self
    }
    /// Starts the actor and initializes durable storage.
    pub fn build(self) -> Result<EngineHandle, StorageError> {
        let root = self.workspace_root.unwrap_or(
            std::env::current_dir().map_err(|e| StorageError::InvalidData(e.to_string()))?,
        );
        // Persist a stable identity rather than a presentation spelling. On
        // macOS `/var` and `/private/var` can name the same workspace; a TUI
        // started through the other spelling must still find its Sessions.
        let workspace_identity = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let workspace_root = workspace_identity
            .to_str()
            .ok_or_else(|| StorageError::InvalidData("workspace root is not valid UTF-8".into()))?
            .to_owned();
        let database_path = self.database_path;
        let tools = tools::ToolRegistry::new(
            &root,
            self.enabled_tools.as_ref(),
            &self.deny_globs,
            database_path.as_deref(),
        )
        .map_err(|e| StorageError::InvalidData(e.to_string()))?;
        let storage = match database_path {
            Some(path) => storage::Storage::open(&path)?,
            None => storage::Storage::memory()?,
        };
        let (events, _) = broadcast::channel(32);
        let (thread_events, _) = broadcast::channel(64);
        Ok(EngineHandle {
            events,
            thread_events,
            storage: Arc::new(storage),
            tools: Arc::new(tools),
            workspace_root: Arc::from(workspace_root),
            process_supervision_supported: cfg!(unix),
            process_group_probe_override: None,
            operation_gate: Arc::new(Semaphore::new(1)),
            #[cfg(test)]
            completion_hook: Arc::new(std::sync::Mutex::new(None)),
        })
    }
}
/// Restricted capability for dispatch and subscription.
///
/// Public capability matrix (kept exhaustive by `public_api_matrix_is_complete`):
///
/// | Class | Methods |
/// |---|---|
/// | Read-only | `tool_descriptors`, `changed_files`, `workspace_manifest`, `subscribe`, `show`, `list`, `effect_status`, `unknown_effects_for_run`, `runtime_checkpoint`, `permission_matches` |
/// | Bootstrap authority | `create_run`, `acquire_lease`, `renew_lease`, `release_lease` |
/// | Fenced authoritative mutation | `execute_tool`, `reissue_tool_permission`, `execute_process`, `execute_verification`, `reissue_process_permission`, `apply_transition`, `complete_verified_run`, `interrupt_after_lease_loss`, `resolve_unknown_effect_and_abort`, `persist_runtime_checkpoint` |
///
/// Raw effect-ledger mutation is intentionally absent from this public handle.
/// These compile-fail examples are an API-boundary contract, not usage examples.
///
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, run: latte_core::RunId) {
///     h.record_effect_started("effect", run, 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, run: latte_core::RunId) {
///     h.record_effect_declared("effect", run, 1, "{}", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.record_effect_prepared("effect", "digest", "{}", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.start_prepared_effect("effect", "digest", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.record_effect_finished("effect", true, "{}", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.record_effect_attempt("effect", 1, "started", "{}").unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.record_effect_observed("effect", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, run: latte_core::RunId) {
///     h.record_evidence("evidence", run, "{}", None).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, run: latte_core::RunId) {
///     h.put_runtime_checkpoint(run, "{}", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.reconcile_unknown_failed("effect", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle) {
///     h.record_command_result("command", "forged", 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, run: latte_core::RunId) {
///     h.abandon_pending_effect("effect", run, 0).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge() -> latte_engine::Lease {
///     latte_engine::Lease { owner: "fake".into(), fencing_token: 1, expires_at_ms: u64::MAX }
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, state: latte_core::RunState, lease: &latte_engine::Lease) {
///     h.append_event(&state, 0, panic!(), &panic!(), 0, lease).unwrap();
/// }
/// ```
/// ```compile_fail
/// fn forge(h: &latte_engine::EngineHandle, run: latte_core::RunId, lease: &latte_engine::Lease) {
///     h.record_verification_evidence(run, 0, lease, panic!(), 0).unwrap();
/// }
/// ```
#[derive(Clone)]
pub struct EngineHandle {
    events: broadcast::Sender<EventEnvelope>,
    thread_events: broadcast::Sender<ThreadEventEnvelope>,
    storage: Arc<storage::Storage>,
    tools: Arc<tools::ToolRegistry>,
    workspace_root: Arc<str>,
    process_supervision_supported: bool,
    process_group_probe_override: Option<process::GroupProbe>,
    operation_gate: Arc<Semaphore>,
    #[cfg(test)]
    completion_hook: CompletionHook,
}
impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineHandle")
            .finish_non_exhaustive()
    }
}
impl EngineHandle {
    fn reject_linked_run(&self, run_id: RunId) -> Result<(), StorageError> {
        if self.storage.is_thread_linked_run(run_id)? {
            Err(StorageError::LinkedRunRequiresThreadCommit)
        } else {
            Ok(())
        }
    }
    fn operation_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        loop {
            if let Ok(permit) = Arc::clone(&self.operation_gate).try_acquire_owned() {
                return permit;
            }
            std::thread::yield_now();
        }
    }
    fn manifest_digest(&self) -> Result<String, ToolError> {
        use sha2::{Digest, Sha256};
        let manifest = self.tools.workspace_manifest()?;
        let bytes =
            serde_json::to_vec(&manifest).map_err(|error| ToolError::Input(error.to_string()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
    fn stable_completion_snapshot(&self) -> Result<CompletionSnapshot, StorageError> {
        for _ in 0..3 {
            let first_manifest = match self.workspace_manifest() {
                Ok(value) => value,
                Err(ToolError::WorkspaceUnstable(_)) => continue,
                Err(error) => return Err(StorageError::InvalidData(error.to_string())),
            };
            let first = manifest_map_digest(&first_manifest)?;
            #[cfg(test)]
            self.run_completion_hook(1);
            #[cfg(test)]
            self.run_completion_hook(2);
            let second_manifest = match self.workspace_manifest() {
                Ok(value) => value,
                Err(ToolError::WorkspaceUnstable(_)) => continue,
                Err(error) => return Err(StorageError::InvalidData(error.to_string())),
            };
            let second = manifest_map_digest(&second_manifest)?;
            #[cfg(test)]
            self.run_completion_hook(3);
            if first == second {
                return Ok((second, second_manifest));
            }
        }
        Err(StorageError::InvalidData(
            "workspace remained unstable across completion sampling".into(),
        ))
    }
    #[cfg(all(test, unix))]
    fn set_completion_hook(&self, hook: impl Fn(u8) + Send + Sync + 'static) {
        *self.completion_hook.lock().expect("hook mutex poisoned") = Some(Arc::new(hook));
    }
    #[cfg(test)]
    fn run_completion_hook(&self, stage: u8) {
        let hook = self
            .completion_hook
            .lock()
            .expect("hook mutex poisoned")
            .clone();
        if let Some(hook) = hook {
            hook(stage);
        }
    }
    /// Lists only tools enabled in this engine instance; registry authority remains private.
    #[must_use]
    pub fn tool_descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.tools.descriptors();
        if self.process_supervision_supported {
            tools.push(ToolDescriptor {
                name: "process".into(),
                description: "Engine-owned supervised process operation".into(),
                input_schema: crate::tools::tool_schema("process"),
                version: 1,
                effect: "process".into(),
            });
            tools.sort_by(|a, b| a.name.cmp(&b.name));
        }
        tools
    }
    /// Computes changed paths through the private fixed safe git profile.
    pub fn changed_files(&self) -> Result<Vec<String>, ToolError> {
        self.tools.changed_files()
    }
    /// Captures a deterministic content manifest for truthful non-Git handoff fallback.
    pub fn workspace_manifest(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, ToolError> {
        self.tools.workspace_manifest()
    }
    /// Executes an engine-owned operation. Mutations cannot bypass the durable effect ledger.
    pub fn execute_tool(
        &self,
        run_id: RunId,
        lease: &Lease,
        now_ms: u64,
        invocation: &ToolInvocation<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let _operation = self.operation_permit();
        self.reject_linked_run(run_id)
            .map_err(|error| ToolError::Input(error.to_string()))?;
        if lease.owner != invocation.lease_owner || lease.fencing_token != invocation.lease_token {
            return Err(ToolError::InvalidApproval);
        }
        let (prepared, decision, digest) = self.tools.prepare_for_engine(invocation)?;
        match decision {
            policy::PolicyDecision::Deny => return Err(ToolError::Denied(invocation.name.into())),
            policy::PolicyDecision::Allow => return self.tools.execute_prepared(prepared),
            policy::PolicyDecision::Ask => {}
        }
        if invocation.approval_digest.is_none() {
            let descriptor=serde_json::json!({"tool":invocation.name,"input":invocation.input,"digest":digest}).to_string();
            self.storage
                .create_prepared_permission(
                    invocation.effect_id,
                    run_id,
                    invocation.run_revision.saturating_sub(2),
                    invocation.run_revision,
                    invocation.attempt,
                    &descriptor,
                    &digest,
                    lease,
                    now_ms,
                )
                .map_err(tool_storage)?;
            return Err(ToolError::PermissionRequired {
                target: invocation.name.into(),
                digest,
            });
        }
        let Some(supplied) = invocation.approval_digest else {
            return Err(ToolError::InvalidApproval);
        };
        if supplied != digest {
            return Err(ToolError::InvalidApproval);
        }
        let authority = self
            .storage
            .consume_permission_and_start(
                invocation.effect_id,
                run_id,
                invocation.run_revision,
                lease,
                supplied,
                now_ms,
            )
            .map_err(|_| ToolError::InvalidApproval)?;
        match self.tools.execute_prepared(prepared) {
            Ok(output) => {
                self.storage
                    .finish_effect(
                        &authority,
                        true,
                        &serde_json::json!({"output":output.value}).to_string(),
                        now_ms,
                    )
                    .map_err(tool_storage)?;
                Ok(output)
            }
            Err(error) => {
                if matches!(error, ToolError::Io(_) | ToolError::Path(_)) {
                    self.storage
                        .mark_effect_unknown(&authority, now_ms)
                        .map_err(tool_storage)?;
                } else {
                    self.storage
                        .finish_effect(
                            &authority,
                            false,
                            &serde_json::json!({"error":error.to_string()}).to_string(),
                            now_ms,
                        )
                        .map_err(tool_storage)?;
                }
                Err(error)
            }
        }
    }
    /// Rebinds an expired tool approval to a new lease without executing it.
    pub fn reissue_tool_permission(
        &self,
        old_effect_id: &str,
        run_id: RunId,
        lease: &Lease,
        now_ms: u64,
        invocation: &ToolInvocation<'_>,
    ) -> Result<String, ToolError> {
        self.reject_linked_run(run_id)
            .map_err(|error| ToolError::Input(error.to_string()))?;
        let (_prepared, decision, digest) = self.tools.prepare_for_engine(invocation)?;
        if decision != policy::PolicyDecision::Ask {
            return Err(ToolError::Input(
                "only ask operations can be reissued".into(),
            ));
        }
        let descriptor =
            serde_json::json!({"tool":invocation.name,"input":invocation.input,"digest":digest})
                .to_string();
        self.storage
            .replace_pending_effect(
                old_effect_id,
                invocation.effect_id,
                run_id,
                invocation.run_revision,
                invocation.attempt,
                &descriptor,
                &digest,
                lease,
                now_ms,
            )
            .map_err(tool_storage)?;
        Ok(digest)
    }
    /// Subscribes to subsequent events.
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        Subscription {
            receiver: self.events.subscribe(),
        }
    }
    /// Subscribes to v2 durable thread events. A lag is a signal to reload a
    /// snapshot; events are not a second source of truth.
    #[must_use]
    pub fn subscribe_threads(&self) -> ThreadSubscription {
        ThreadSubscription {
            receiver: self.thread_events.subscribe(),
        }
    }
    /// Creates a durable v2 thread and its initial linked child. This performs
    /// no provider call or credential resolution.
    #[allow(clippy::needless_pass_by_value)]
    pub fn create_thread_v2(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        binding: ThreadProviderBindingV2,
        prompt: &str,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        let baseline = self
            .workspace_manifest()
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        self.storage.create_thread_v2(
            thread_id,
            run_id,
            &binding,
            &self.workspace_root,
            prompt,
            &baseline,
            now_ms,
        )
    }
    /// Creates an immutable child run for a ready completed thread.
    pub fn create_thread_follow_up_v2(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        expected_thread_revision: u64,
        prompt: &str,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        let baseline = self
            .workspace_manifest()
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        self.storage.create_thread_follow_up_v2(
            thread_id,
            run_id,
            expected_thread_revision,
            prompt,
            &baseline,
            now_ms,
        )
    }
    /// Reads one paged thread projection.
    pub fn thread_snapshot_v2(
        &self,
        thread_id: ThreadId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<ThreadSnapshot, StorageError> {
        self.storage.thread_snapshot_v2(thread_id, after, limit)
    }
    /// Lists thread sessions with bounded recent transcript cards.
    pub fn list_threads_v2(&self) -> Result<Vec<ThreadSnapshot>, StorageError> {
        self.storage.list_threads_v2()
    }
    /// Lists bounded global Session metadata without loading transcripts.
    pub fn list_thread_sessions_v2(
        &self,
        limit: usize,
    ) -> Result<Vec<latte_core::ThreadSessionSummary>, StorageError> {
        self.storage.list_thread_sessions_v2(limit)
    }
    /// The only public mutation path for a linked v2 child run.
    #[allow(clippy::needless_pass_by_value)]
    pub fn commit_thread_run_update(
        &self,
        request: ThreadCommitRequest,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadCommitResponse, StorageError> {
        if matches!(&request.update, CommitThreadRunUpdate::Complete { .. }) {
            // `VerificationNotRequired` is legal only for a child whose
            // engine-owned baseline still equals a stable current workspace
            // snapshot.  This keeps the public v2 commit entrypoint from
            // becoming a bypass around configured verification.
            let _operation = self.operation_permit();
            let (_, current_manifest) = self.stable_completion_snapshot()?;
            if !self
                .storage
                .thread_changed_files(request.run_id, &current_manifest)?
                .is_empty()
            {
                return Err(StorageError::InvalidData(
                    "linked child changed the workspace; use verified completion".into(),
                ));
            }
        }
        let response = self
            .storage
            .commit_thread_run_update(&request, lease, now_ms)?;
        let _ = self
            .thread_events
            .send(response.thread_event.envelope.clone());
        Ok(response)
    }
    /// Returns the exact files changed since this linked v2 child began.
    /// The comparison is against an engine-owned baseline captured before the
    /// provider can receive effect authority.
    pub fn thread_run_changed_files(&self, run_id: RunId) -> Result<Vec<String>, StorageError> {
        let current = self
            .workspace_manifest()
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        self.storage.thread_changed_files(run_id, &current)
    }
    /// Persists the actual configured verification result under the linked
    /// child's current fenced revision/effect epoch.  This is deliberately
    /// separate from transcript output: a provider cannot fabricate it.
    pub fn record_thread_verification(
        &self,
        run_id: RunId,
        expected_revision: u64,
        effect_id: &str,
        output: &ProcessOutput,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let (workspace_manifest_digest, _) = self.stable_completion_snapshot()?;
        let effect_epoch = self.storage.effect_epoch(run_id)?;
        let summary = serde_json::to_string(output)
            .map(|value| latte_core::redact_thread_text(&value))
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let metadata = serde_json::to_string(&storage::VerificationRecord {
            revision: expected_revision,
            effect_epoch,
            effect_id: effect_id.to_owned(),
            passed: output.command_succeeded(),
            workspace_manifest_digest,
            summary,
        })
        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        self.storage.record_verification_evidence(
            run_id,
            expected_revision,
            lease,
            &VerificationEvidence {
                id: effect_id,
                metadata_json: &metadata,
                blob_ref: None,
            },
            now_ms,
        )
    }
    /// Atomically transitions a linked v2 child to completed only when its
    /// current engine-recorded verification evidence matches a stable current
    /// workspace manifest.
    pub fn complete_thread_verified(
        &self,
        snapshot: &ThreadSnapshot,
        summary: String,
        verification_effect_id: String,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        let _operation = self.operation_permit();
        let run_id = snapshot
            .active_run_id
            .ok_or(StorageError::ThreadActiveRunMismatch)?;
        let expected_run_revision = run_revision(snapshot, &verification_effect_id)
            .ok_or_else(|| StorageError::InvalidData("linked child is missing".into()))?;
        let (verified_manifest_digest, current_manifest) = self.stable_completion_snapshot()?;
        let files_changed = self
            .storage
            .thread_changed_files(run_id, &current_manifest)?;
        self.commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: snapshot.thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision,
                command_id: latte_core::ThreadCommandId::from_uuid(uuid::Uuid::now_v7()),
                request_id: None,
                effect_id: Some(verification_effect_id.clone()),
                update: CommitThreadRunUpdate::CompleteVerified {
                    source_key: format!("{run_id}:complete-verified"),
                    summary,
                    verification_effect_id,
                    verified_manifest_digest,
                    files_changed,
                },
            },
            lease,
            now_ms,
        )
        .map(|response| response.snapshot)
    }
    /// Validates policy and effect preconditions, then durably records a
    /// prepared v2 descriptor.  It never executes the descriptor.
    pub fn prepare_thread_effect(
        &self,
        request: ThreadEffectRequest,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadEffectPrepared, StorageError> {
        validate_thread_effect_descriptor(&request.descriptor)?;
        let (policy, mut operation_digest) = self.thread_effect_policy_and_digest(
            &request.descriptor,
            request.expected_run_revision,
            lease,
        )?;
        if policy == ThreadEffectPolicy::Ask {
            let post_approval_revision = request
                .expected_run_revision
                .checked_add(2)
                .ok_or_else(|| StorageError::InvalidData("run revision overflow".into()))?;
            let (_same_policy, rebound_digest) = self.thread_effect_policy_and_digest(
                &request.descriptor,
                post_approval_revision,
                lease,
            )?;
            operation_digest = rebound_digest;
        }
        let persisted = ThreadEffectDescriptor {
            input: latte_core::redact_thread_value(request.descriptor.input.clone()),
            ..request.descriptor.clone()
        };
        let descriptor_json = serde_json::to_string(&persisted)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let canonical_descriptor_json = serde_json::to_string(&request.descriptor)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let checkpoint_json = thread_effect_checkpoint("prepared", &persisted, &operation_digest);
        let description = thread_effect_permission_summary(&request.descriptor);
        let response = self.commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: request.thread_id,
                run_id: request.run_id,
                expected_thread_revision: request.expected_thread_revision,
                expected_run_revision: request.expected_run_revision,
                command_id: request.command_id,
                request_id: None,
                effect_id: Some(persisted.effect_id.clone()),
                update: CommitThreadRunUpdate::PrepareEffect {
                    source_key: request.source_key,
                    effect_id: persisted.effect_id,
                    operation_digest: operation_digest.clone(),
                    descriptor_json,
                    canonical_descriptor_json,
                    policy,
                    description,
                    checkpoint_json,
                },
            },
            lease,
            now_ms,
        )?;
        Ok(ThreadEffectPrepared {
            snapshot: response.snapshot,
            policy,
            operation_digest,
        })
    }
    /// Atomically consumes a durable ask approval (or the durable allow
    /// marker) and records Started before returning executable authority.
    pub fn start_thread_effect(
        &self,
        request: ThreadEffectStartRequest,
        operation_digest: String,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadEffectStarted, StorageError> {
        let descriptor = self
            .storage
            .thread_effect_canonical_descriptor(&request.effect_id, request.run_id)?;
        validate_thread_effect_descriptor(&descriptor)?;
        if descriptor.effect_id != request.effect_id {
            return Err(StorageError::InvalidData(
                "canonical thread effect identifier mismatch".into(),
            ));
        }
        let (_policy, exact_digest) = self.thread_effect_policy_and_digest(
            &descriptor,
            request.expected_run_revision,
            lease,
        )?;
        if exact_digest != operation_digest {
            return Err(StorageError::InvalidData(
                "canonical thread effect digest mismatch".into(),
            ));
        }
        let checkpoint_json = thread_effect_checkpoint("started", &descriptor, &operation_digest);
        let response = self.commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: request.thread_id,
                run_id: request.run_id,
                expected_thread_revision: request.expected_thread_revision,
                expected_run_revision: request.expected_run_revision,
                command_id: request.command_id,
                request_id: Some(request.effect_id.clone()),
                effect_id: Some(request.effect_id.clone()),
                update: CommitThreadRunUpdate::StartEffect {
                    source_key: request.source_key,
                    effect_id: request.effect_id,
                    operation_digest: operation_digest.clone(),
                    checkpoint_json,
                },
            },
            lease,
            now_ms,
        )?;
        Ok(ThreadEffectStarted {
            snapshot: response.snapshot,
            presentation: ThreadEffectPresentation::from_descriptor(&descriptor),
            operation_digest,
            descriptor,
        })
    }
    /// Runs a descriptor only after a successful Started transaction.  The
    /// effect ledger is intentionally not finalized here; observation is a
    /// separate fenced commit so a crash in between remains Unknown-safe.
    pub async fn execute_started_thread_effect(
        &self,
        started: &ThreadEffectStarted,
        lease: &Lease,
        cancellation: &CancellationToken,
    ) -> Result<ThreadEffectObservedValue, ThreadEffectExecutionError> {
        let descriptor = &started.descriptor;
        let (_policy, digest) = self
            .thread_effect_policy_and_digest(
                descriptor,
                run_revision(&started.snapshot, descriptor.effect_id.as_str()).ok_or_else(
                    || ThreadEffectExecutionError::Uncertain("started run is missing".into()),
                )?,
                lease,
            )
            .map_err(|error| ThreadEffectExecutionError::Uncertain(error.to_string()))?;
        if digest != started.operation_digest {
            return Err(ThreadEffectExecutionError::Uncertain(
                "prepared descriptor no longer has the exact operation digest".into(),
            ));
        }
        if descriptor.name == "process" {
            self.execute_started_thread_process(
                descriptor,
                run_revision(&started.snapshot, descriptor.effect_id.as_str()).ok_or_else(
                    || ThreadEffectExecutionError::Uncertain("started run is missing".into()),
                )?,
                lease,
                cancellation,
            )
            .await
        } else {
            let revision = run_revision(&started.snapshot, descriptor.effect_id.as_str())
                .ok_or_else(|| {
                    ThreadEffectExecutionError::Uncertain("started run is missing".into())
                })?;
            let engine = self.clone();
            let descriptor = descriptor.clone();
            let operation_digest = started.operation_digest.clone();
            let lease = lease.clone();
            let cancellation = cancellation.clone();
            tokio::task::spawn_blocking(move || {
                engine.execute_started_thread_tool(
                    &descriptor,
                    revision,
                    &operation_digest,
                    &lease,
                    &cancellation,
                )
            })
            .await
            .map_err(|error| {
                ThreadEffectExecutionError::Uncertain(format!(
                    "started tool worker terminated before observation: {error}"
                ))
            })?
        }
    }

    /// Runs a non-process descriptor away from the async lease heartbeat.  A
    /// filesystem stall must not block the coordinator reactor and silently
    /// let a Started effect outlive its authority window.
    fn execute_started_thread_tool(
        &self,
        descriptor: &ThreadEffectDescriptor,
        revision: u64,
        operation_digest: &str,
        lease: &Lease,
        cancellation: &CancellationToken,
    ) -> Result<ThreadEffectObservedValue, ThreadEffectExecutionError> {
        if cancellation.is_cancelled() {
            return Err(ThreadEffectExecutionError::Uncertain(
                "tool cancelled after Started".into(),
            ));
        }
        let invocation = ToolInvocation {
            name: &descriptor.name,
            input: &descriptor.input,
            run_revision: revision,
            effect_id: &descriptor.effect_id,
            attempt: descriptor.attempt,
            precondition: descriptor.input.get("precondition").and_then(Value::as_str),
            timeout_ms: 30_000,
            output_cap: 64 * 1024,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        let (prepared, decision, exact) =
            self.tools
                .prepare_for_engine(&invocation)
                .map_err(|error| {
                    ThreadEffectExecutionError::Uncertain(format!(
                        "effect precondition changed: {error}"
                    ))
                })?;
        if exact != operation_digest || decision == policy::PolicyDecision::Deny {
            return Err(ThreadEffectExecutionError::Uncertain(
                "effect authorization changed before execution".into(),
            ));
        }
        let _operation = self.operation_permit();
        match self.tools.execute_prepared(prepared) {
            Ok(output) => Ok(ThreadEffectObservedValue {
                result: serde_json::to_string(&output.value).unwrap_or_else(|_| "null".into()),
                payload: Some(latte_core::redact_thread_value(serde_json::json!({
                    "tool_call_id": descriptor.tool_call_id,
                    "name": descriptor.name,
                    "output": output.value,
                    "truncated": output.truncated,
                }))),
                success: true,
            }),
            Err(
                error @ (ToolError::Io(_) | ToolError::Path(_) | ToolError::WorkspaceUnsafe(_)),
            ) => Err(ThreadEffectExecutionError::Uncertain(error.to_string())),
            Err(error) => Ok(ThreadEffectObservedValue {
                result: serde_json::json!({"error":error.to_string()}).to_string(),
                payload: Some(serde_json::json!({
                    "tool_call_id":descriptor.tool_call_id,
                    "name":descriptor.name,
                    "error":error.to_string(),
                })),
                success: false,
            }),
        }
    }
    /// Durably observes an already started effect and returns the next
    /// authoritative thread snapshot.
    pub fn observe_thread_effect(
        &self,
        started: &ThreadEffectStarted,
        source_key: String,
        command_id: latte_core::ThreadCommandId,
        value: ThreadEffectObservedValue,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadEffectObserved, StorageError> {
        let revision = run_revision(&started.snapshot, started.descriptor.effect_id.as_str())
            .ok_or_else(|| StorageError::InvalidData("started run is missing".into()))?;
        let response = self.commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: started.snapshot.thread_id,
                run_id: started
                    .snapshot
                    .active_run_id
                    .ok_or(StorageError::ThreadActiveRunMismatch)?,
                expected_thread_revision: started.snapshot.revision,
                expected_run_revision: revision,
                command_id,
                request_id: Some(started.descriptor.effect_id.clone()),
                effect_id: Some(started.descriptor.effect_id.clone()),
                update: CommitThreadRunUpdate::ObserveEffect {
                    source_key,
                    effect_id: started.descriptor.effect_id.clone(),
                    operation_digest: started.operation_digest.clone(),
                    success: value.success,
                    result: value.result.clone(),
                    payload: value.payload,
                    checkpoint_json: thread_effect_checkpoint(
                        if value.success {
                            "observed_success"
                        } else {
                            "observed_failed"
                        },
                        &started.descriptor,
                        &started.operation_digest,
                    ),
                },
            },
            lease,
            now_ms,
        )?;
        Ok(ThreadEffectObserved {
            snapshot: response.snapshot,
            result: value.result,
            success: value.success,
        })
    }
    /// Maps an uncertain post-Started boundary to Unknown and removes the
    /// active child through the v2 transaction path.
    pub fn mark_thread_effect_unknown(
        &self,
        started: &ThreadEffectStarted,
        source_key: String,
        command_id: latte_core::ThreadCommandId,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        let revision = run_revision(&started.snapshot, started.descriptor.effect_id.as_str())
            .ok_or_else(|| StorageError::InvalidData("started run is missing".into()))?;
        self.commit_thread_run_update(
            ThreadCommitRequest {
                thread_id: started.snapshot.thread_id,
                run_id: started
                    .snapshot
                    .active_run_id
                    .ok_or(StorageError::ThreadActiveRunMismatch)?,
                expected_thread_revision: started.snapshot.revision,
                expected_run_revision: revision,
                command_id,
                request_id: Some(started.descriptor.effect_id.clone()),
                effect_id: Some(started.descriptor.effect_id.clone()),
                update: CommitThreadRunUpdate::UnknownEffect {
                    source_key,
                    effect_id: started.descriptor.effect_id.clone(),
                    operation_digest: started.operation_digest.clone(),
                    checkpoint_json: thread_effect_checkpoint(
                        "unknown",
                        &started.descriptor,
                        &started.operation_digest,
                    ),
                },
            },
            lease,
            now_ms,
        )
        .map(|response| response.snapshot)
    }
    /// Explicitly acknowledges an unknown v2 effect and terminalizes the
    /// linked child without reaching a legacy reconciliation entrypoint.
    #[allow(clippy::too_many_arguments)]
    pub fn reconcile_thread_effect_unknown(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        expected_thread_revision: u64,
        expected_run_revision: u64,
        effect_id: String,
        source_key: String,
        command_id: latte_core::ThreadCommandId,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        self.commit_thread_run_update(
            ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision,
                expected_run_revision,
                command_id,
                request_id: Some(effect_id.clone()),
                effect_id: Some(effect_id.clone()),
                update: CommitThreadRunUpdate::ReconcileUnknownEffect {
                    source_key,
                    effect_id,
                    checkpoint_json: serde_json::json!({"thread_effect":"reconciled_unknown"})
                        .to_string(),
                },
            },
            lease,
            now_ms,
        )
        .map(|response| response.snapshot)
    }
    /// Conservatively recovers a linked v2 child after its lease renewal has
    /// failed.  The storage transaction updates the legacy run/effects and
    /// the v2 thread projection together; only the committed final thread
    /// event is broadcast to connected clients.
    pub fn recover_thread_after_lease_loss(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        stale: &Lease,
        expected_run_revision: u64,
        now_ms: u64,
    ) -> Result<ThreadLeaseLossRecovery, StorageError> {
        let result = self.storage.recover_thread_after_lease_loss(
            thread_id,
            run_id,
            stale,
            expected_run_revision,
            now_ms,
        )?;
        if let ThreadLeaseLossRecovery::Recovered(response) = &result {
            let _ = self
                .thread_events
                .send(response.thread_event.envelope.clone());
        }
        Ok(result)
    }
    fn thread_effect_policy_and_digest(
        &self,
        descriptor: &ThreadEffectDescriptor,
        run_revision: u64,
        lease: &Lease,
    ) -> Result<(ThreadEffectPolicy, String), StorageError> {
        if descriptor.name == "process" {
            if !self.process_supervision_supported {
                return Err(StorageError::InvalidData(
                    "process supervision is unsupported on this platform".into(),
                ));
            }
            let spec = process::ThreadProcessSpec::from_input(&descriptor.input)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?;
            self.tools
                .resolve_cwd(&spec.cwd)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?;
            let invocation = spec.invocation(run_revision, descriptor, lease);
            let decision = process::classify(&invocation);
            let policy = match decision {
                ProcessDecision::Allow => ThreadEffectPolicy::Allow,
                ProcessDecision::Ask => ThreadEffectPolicy::Ask,
                ProcessDecision::Deny => {
                    return Err(StorageError::InvalidData(
                        "process policy denied operation".into(),
                    ));
                }
            };
            return Ok((policy, process::digest(&invocation)));
        }
        let invocation = ToolInvocation {
            name: &descriptor.name,
            input: &descriptor.input,
            run_revision,
            effect_id: &descriptor.effect_id,
            attempt: descriptor.attempt,
            precondition: descriptor.input.get("precondition").and_then(Value::as_str),
            timeout_ms: 30_000,
            output_cap: 64 * 1024,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        let (_prepared, decision, digest) = self
            .tools
            .prepare_for_engine(&invocation)
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let policy = match decision {
            policy::PolicyDecision::Allow => ThreadEffectPolicy::Allow,
            policy::PolicyDecision::Ask => ThreadEffectPolicy::Ask,
            policy::PolicyDecision::Deny => {
                return Err(StorageError::InvalidData(
                    "tool policy denied operation".into(),
                ));
            }
        };
        Ok((policy, digest))
    }
    /// Persists a new queued run.
    pub fn create_run(&self, run_id: RunId, now_ms: u64) -> Result<RunState, StorageError> {
        let state = RunState::queued(run_id);
        let baseline = self
            .workspace_manifest()
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        self.storage
            .create_run_with_baseline(&state, now_ms, Some(&baseline))?;
        Ok(state)
    }
    /// Reads one durable run projection.
    pub fn show(&self, run_id: RunId) -> Result<RunState, StorageError> {
        self.storage.load_run(run_id)
    }
    /// Lists durable run projections.
    pub fn list(&self) -> Result<Vec<RunState>, StorageError> {
        self.storage.list_runs()
    }
    /// Applies one core-validated transition and emits its canonical event atomically.
    pub fn apply_transition(
        &self,
        run_id: RunId,
        expected_revision: u64,
        transition: latte_core::Transition,
        now_ms: u64,
        lease: &Lease,
    ) -> Result<RunState, StorageError> {
        self.reject_linked_run(run_id)?;
        if matches!(transition, Transition::Complete { .. }) {
            return Err(StorageError::InvalidData(
                "Complete is engine-owned; use complete_verified_run".into(),
            ));
        }
        let (next, stored) =
            self.storage
                .apply_transition(run_id, expected_revision, transition, now_ms, lease)?;
        let _ = self.events.send(stored.envelope);
        Ok(next)
    }
    /// Completes only from engine-recorded passing verification at this exact revision.
    pub fn complete_verified_run(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        summary: String,
        now_ms: u64,
    ) -> Result<RunState, StorageError> {
        let _operation = self.operation_permit();
        self.reject_linked_run(run_id)?;
        let (manifest_digest, current_manifest) = self.stable_completion_snapshot()?;
        let (next, stored) = self.storage.complete_verified(
            run_id,
            expected_revision,
            lease,
            summary,
            &current_manifest,
            &manifest_digest,
            now_ms,
        )?;
        let _ = self.events.send(stored.envelope);
        Ok(next)
    }
    /// Atomically cancels a run that is blocked on input or permission.
    pub fn cancel_waiting_run(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<RunState, StorageError> {
        self.reject_linked_run(run_id)?;
        let (state, event) =
            self.storage
                .cancel_waiting(run_id, expected_revision, lease, now_ms, false)?;
        if let Some(event) = event {
            let _ = self.events.send(event.envelope);
        }
        Ok(state)
    }
    /// Atomically denies a waiting permission without constructing a provider.
    pub fn deny_waiting_permission(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<RunState, StorageError> {
        self.reject_linked_run(run_id)?;
        let (state, event) =
            self.storage
                .cancel_waiting(run_id, expected_revision, lease, now_ms, true)?;
        if let Some(event) = event {
            let _ = self.events.send(event.envelope);
        }
        Ok(state)
    }
    /// Acquires or takes over the single runtime lease after expiry.
    pub fn acquire_lease(
        &self,
        owner: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        self.storage.acquire_lease(owner, now_ms, ttl_ms)
    }
    /// Renews a currently valid lease.
    pub fn renew_lease(
        &self,
        lease: &Lease,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        self.storage.renew_lease(lease, now_ms, ttl_ms)
    }
    /// Releases a lease if its fencing token still matches.
    pub fn release_lease(&self, lease: &Lease) -> Result<(), StorageError> {
        self.storage.release_lease(lease)
    }
    /// Reads the durable effect reconciliation status.
    pub fn effect_status(&self, effect_id: &str) -> Result<EffectStatus, StorageError> {
        self.storage.effect_status(effect_id)
    }
    /// Reads the private durable digest for a v2 prepared effect.  The value
    /// is returned only to the engine-owned thread coordinator so transcript
    /// redaction never becomes an approval transport.
    pub fn thread_effect_digest(&self, effect_id: &str) -> Result<String, StorageError> {
        self.storage.thread_effect_digest(effect_id)
    }
    /// Lists every unknown effect for a run, including allow-path effects without approval rows.
    pub fn unknown_effects_for_run(&self, run_id: RunId) -> Result<Vec<String>, StorageError> {
        self.reject_linked_run(run_id)?;
        self.storage.unknown_effects_for_run(run_id)
    }
    /// Fences a stale owner and durably interrupts its run after heartbeat loss.
    pub fn interrupt_after_lease_loss(
        &self,
        run_id: RunId,
        stale: &Lease,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<LeaseLossRecovery, StorageError> {
        self.reject_linked_run(run_id)?;
        self.storage
            .interrupt_after_lease_loss(run_id, stale, expected_revision, now_ms)
    }
    /// Atomically reconciles one run-owned unknown effect and aborts that exact run.
    pub fn resolve_unknown_effect_and_abort(
        &self,
        run_id: RunId,
        effect_id: &str,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<RunState, StorageError> {
        self.reject_linked_run(run_id)?;
        self.storage.reconcile_unknown_and_abort(
            run_id,
            effect_id,
            expected_revision,
            lease,
            now_ms,
        )
    }
    /// Persists renderer-neutral runtime state under fenced run authority.
    pub fn persist_runtime_checkpoint(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        payload_json: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        self.reject_linked_run(run_id)?;
        self.storage
            .put_checkpoint(run_id, expected_revision, lease, payload_json, now_ms)
    }
    /// Loads renderer-neutral agent runtime state.
    pub fn runtime_checkpoint(&self, run_id: RunId) -> Result<Option<String>, StorageError> {
        self.reject_linked_run(run_id)?;
        self.storage.checkpoint(run_id)
    }
    /// Validates a durable permission without consuming it or starting the effect.
    pub fn permission_matches(
        &self,
        effect_id: &str,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        digest: &str,
        now_ms: u64,
    ) -> Result<bool, StorageError> {
        self.reject_linked_run(run_id)?;
        self.storage
            .permission_matches(effect_id, run_id, expected_revision, lease, digest, now_ms)
    }
}
/// Event subscription.
#[derive(Debug)]
pub struct Subscription {
    receiver: broadcast::Receiver<EventEnvelope>,
}

/// V2 thread event subscription. It intentionally shares the same lag/closed
/// semantics as the legacy run stream while carrying only v2 envelopes.
#[derive(Debug)]
pub struct ThreadSubscription {
    receiver: broadcast::Receiver<ThreadEventEnvelope>,
}
impl ThreadSubscription {
    /// Polls without blocking a terminal renderer.
    pub fn try_recv(&mut self) -> Result<Option<ThreadEventEnvelope>, SubscriptionError> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Closed) => Err(SubscriptionError::Closed),
            Err(broadcast::error::TryRecvError::Lagged(count)) => {
                Err(SubscriptionError::Lagged(count))
            }
        }
    }
    /// Receives the next thread event.
    pub async fn recv(&mut self) -> Result<ThreadEventEnvelope, SubscriptionError> {
        self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Closed => SubscriptionError::Closed,
            broadcast::error::RecvError::Lagged(count) => SubscriptionError::Lagged(count),
        })
    }
}
impl Subscription {
    /// Polls an event without blocking the terminal renderer.
    pub fn try_recv(&mut self) -> Result<Option<EventEnvelope>, SubscriptionError> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Closed) => Err(SubscriptionError::Closed),
            Err(broadcast::error::TryRecvError::Lagged(n)) => Err(SubscriptionError::Lagged(n)),
        }
    }
    /// Receives an event.
    ///
    /// # Errors
    /// Returns an error when the actor closes or this subscriber lags.
    pub async fn recv(&mut self) -> Result<EventEnvelope, SubscriptionError> {
        self.receiver.recv().await.map_err(|e| match e {
            broadcast::error::RecvError::Closed => SubscriptionError::Closed,
            broadcast::error::RecvError::Lagged(n) => SubscriptionError::Lagged(n),
        })
    }
}
/// Subscription failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionError {
    /// Actor stopped.
    Closed,
    /// Subscriber missed this many events.
    Lagged(u64),
}
#[allow(clippy::needless_pass_by_value)]
fn tool_storage(error: StorageError) -> ToolError {
    ToolError::Input(format!("durable tool sequencing failed: {error}"))
}

#[cfg(test)]
mod tool_effect_tests {
    use super::*;
    use latte_core::{IdSource, SystemIdSource};
    use serde_json::json;

    fn descriptor(name: &str, input: Value) -> ThreadEffectDescriptor {
        ThreadEffectDescriptor {
            effect_id: format!("effect-{name}"),
            tool_call_id: format!("call-{name}"),
            name: name.into(),
            input,
            attempt: 1,
        }
    }

    fn test_thread_binding() -> ThreadProviderBindingV2 {
        ThreadProviderBindingV2 {
            version: 1,
            provider_name: "test".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "test-model".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:TEST_KEY".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        }
    }

    #[test]
    fn descriptor_validation_checkpoint_and_safe_presentations_fail_closed() {
        let valid = descriptor(
            "write_file",
            json!({"path":"safe.txt","content":"ok","create_intent":false}),
        );
        assert!(validate_thread_effect_descriptor(&valid).is_ok());
        let checkpoint =
            thread_effect_checkpoint("prepared", &valid, "authorization=Bearer live-secret-value");
        assert!(checkpoint.contains("prepared"));
        assert!(!checkpoint.contains("live-secret-value"));
        let presentation = ThreadEffectPresentation::from_descriptor(&ThreadEffectDescriptor {
            input: json!({"token":"live-secret-value","path":"safe.txt"}),
            ..valid.clone()
        });
        assert_eq!(presentation.input["path"], "safe.txt");
        assert_ne!(presentation.input["token"], "live-secret-value");

        for invalid in [
            ThreadEffectDescriptor {
                effect_id: String::new(),
                ..valid.clone()
            },
            ThreadEffectDescriptor {
                name: "bad\nname".into(),
                ..valid.clone()
            },
            ThreadEffectDescriptor {
                tool_call_id: "bad id".into(),
                ..valid.clone()
            },
            ThreadEffectDescriptor {
                attempt: 0,
                ..valid.clone()
            },
            ThreadEffectDescriptor {
                input: json!([]),
                ..valid.clone()
            },
        ] {
            assert!(matches!(
                validate_thread_effect_descriptor(&invalid),
                Err(StorageError::InvalidData(_))
            ));
        }
    }

    #[test]
    fn every_permission_summary_is_bounded_specific_and_secret_safe() {
        let cases = [
            (
                descriptor(
                    "write_file",
                    json!({"path":"a.txt","content":"abc","create_intent":false}),
                ),
                "replace existing",
            ),
            (
                descriptor(
                    "edit_file",
                    json!({"path":"a.txt","before":"old","after":"newer"}),
                ),
                "replace one match; 3 bytes → 5 bytes",
            ),
            (
                descriptor("process", json!({"shell":"echo unsafe","cwd":"."})),
                "Run shell command",
            ),
            (descriptor("read_file", json!({})), "Read [unknown path]"),
            (
                descriptor("list_directory", json!({"path":"src"})),
                "List src",
            ),
            (descriptor("search", json!({})), "[unspecified query]"),
            (
                descriptor("custom_tool", json!({})),
                "custom_tool invocation",
            ),
        ];
        for (descriptor, expected) in cases {
            let summary = thread_effect_permission_summary(&descriptor);
            assert!(summary.contains(expected), "summary={summary}");
            assert!(summary.len() <= PERMISSION_SUMMARY_CAP + '…'.len_utf8());
            assert!(!summary.chars().any(char::is_control));
        }
        assert_eq!(summary_text("\n\r\t"), "[unspecified]");
        assert_eq!(summary_argv("MODE=release"), "MODE=release");
        assert_eq!(summary_argv("api_key=secret"), "api_key=[REDACTED]");
        let long = "x".repeat(PERMISSION_SUMMARY_CAP + 100);
        assert!(summary_text(&long).ends_with('…'));
    }

    #[test]
    fn permission_summaries_show_operation_context_without_values_or_controls() {
        let write = ThreadEffectDescriptor {
            effect_id: "write".into(),
            tool_call_id: "call-write".into(),
            name: "write_file".into(),
            input: json!({
                "path": "src/generated.rs",
                "content": "const token=value;\napi_key=live-secret-value",
                "create_intent": true,
            }),
            attempt: 1,
        };
        let write_summary = thread_effect_permission_summary(&write);
        assert!(write_summary.contains("Write src/generated.rs"));
        assert!(write_summary.contains("create or replace"));
        assert!(write_summary.contains("bytes of content"));
        assert!(!write_summary.contains("live-secret-value"));
        assert!(!write_summary.contains("token=value"));

        let process = ThreadEffectDescriptor {
            effect_id: "process".into(),
            tool_call_id: "call-process".into(),
            name: "process".into(),
            input: json!({
                "argv": ["cargo", "test", "--token=live-secret-value"],
                "cwd": "crates/latte-engine\n\u{1b}[31m",
            }),
            attempt: 1,
        };
        let process_summary = thread_effect_permission_summary(&process);
        assert!(process_summary.contains("Run argv: cargo test"));
        assert!(process_summary.contains("cwd: crates/latte-engine"));
        assert!(!process_summary.contains("live-secret-value"));
        assert!(!process_summary.chars().any(char::is_control));
    }

    #[test]
    fn public_api_matrix_is_complete_and_legacy_mutators_are_absent() {
        let source = include_str!("lib.rs");
        for method in [
            "execute_tool",
            "reissue_tool_permission",
            "apply_transition",
            "complete_verified_run",
            "resolve_unknown_effect_and_abort",
            "persist_runtime_checkpoint",
        ] {
            assert!(source.contains(&format!("pub fn {method}")));
        }
        assert!(source.contains("Public capability matrix"));
    }
    #[tokio::test]
    async fn public_transition_cannot_claim_completion() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, latte_core::Transition::Start, 3, &lease)
            .unwrap();
        let mut sub = engine.subscribe();
        let handoff = latte_core::Handoff {
            summary: "false".into(),
            files_changed: vec![],
            evidence: vec![latte_core::Evidence {
                name: "verify".into(),
                status: latte_core::VerificationStatus::Failed,
                summary: "failed".into(),
            }],
        };
        let error = engine
            .apply_transition(
                run,
                running.revision,
                latte_core::Transition::Complete {
                    handoff,
                    policy: latte_core::CompletionPolicy::VerificationRequired,
                },
                4,
                &lease,
            )
            .unwrap_err();
        assert!(error.to_string().contains("engine-owned"));
        assert_eq!(engine.show(run).unwrap(), running);
        assert!(sub.try_recv().unwrap().is_none());
    }

    #[test]
    fn waiting_input_cancel_is_terminal_atomic_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 3, &lease)
            .unwrap();
        assert!(matches!(
            engine.cancel_waiting_run(run, running.revision, &lease, 4),
            Err(StorageError::InvalidData(_))
        ));
        let waiting = engine
            .apply_transition(
                run,
                running.revision,
                Transition::RequestInput(latte_core::PendingInput {
                    request_id: "input".into(),
                    prompt: "answer".into(),
                }),
                4,
                &lease,
            )
            .unwrap();
        engine
            .persist_runtime_checkpoint(run, waiting.revision, &lease, "{}", 5)
            .unwrap();
        assert!(matches!(
            engine.cancel_waiting_run(run, 0, &lease, 6),
            Err(StorageError::StaleRevision { .. })
        ));
        let fresh = engine.acquire_lease("fresh", 200, 100).unwrap();
        assert!(matches!(
            engine.cancel_waiting_run(run, waiting.revision, &lease, 201),
            Err(StorageError::LeaseLost)
        ));
        let cancelled = engine
            .cancel_waiting_run(run, waiting.revision, &fresh, 201)
            .unwrap();
        assert_eq!(cancelled.status, latte_core::RunStatus::Failed);
        assert_eq!(
            cancelled.failure.as_ref().unwrap().code,
            latte_core::FailureCode::Cancelled
        );
        assert!(cancelled.pending_input.is_none());
        assert!(engine.runtime_checkpoint(run).unwrap().is_none());
        assert_eq!(
            engine
                .cancel_waiting_run(run, cancelled.revision, &fresh, 202)
                .unwrap(),
            cancelled
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn waiting_permission_cancel_revokes_prepared_effect() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 3, &lease)
            .unwrap();
        let argv = vec!["/usr/bin/env".into()];
        let env = std::collections::BTreeMap::new();
        let request = ProcessInvocation {
            argv: &argv,
            shell: None,
            cwd: ".",
            env: &env,
            timeout_ms: 1000,
            grace_ms: 10,
            stdout_cap: 1024,
            stderr_cap: 1024,
            run_revision: running.revision + 2,
            effect_id: "cancel-effect",
            attempt: 1,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        let digest = match engine
            .execute_process(run, &lease, 4, &request, &CancellationToken::new())
            .await
            .unwrap_err()
        {
            ProcessError::PermissionRequired { digest } => digest,
            error => panic!("{error}"),
        };
        let waiting = engine
            .apply_transition(
                run,
                running.revision,
                Transition::RequestPermission(latte_core::PendingPermission {
                    request_id: "cancel-effect".into(),
                    operation_digest: digest.clone(),
                    description: "allow".into(),
                }),
                5,
                &lease,
            )
            .unwrap();
        let cancelled = engine
            .cancel_waiting_run(run, waiting.revision, &lease, 6)
            .unwrap();
        assert_eq!(
            cancelled.failure.unwrap().code,
            latte_core::FailureCode::Cancelled
        );
        assert_eq!(
            engine.effect_status("cancel-effect").unwrap(),
            EffectStatus::ObservedFailed
        );
        assert!(
            !engine
                .permission_matches("cancel-effect", run, running.revision, &lease, &digest, 7)
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn completion_requires_current_engine_executed_passing_verification() {
        use std::collections::BTreeMap;

        let dir = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 3, &lease)
            .unwrap();

        assert!(
            engine
                .complete_verified_run(run, running.revision, &lease, "no proof".into(), 4)
                .unwrap_err()
                .to_string()
                .contains("missing current")
        );

        let argv = vec![
            "/usr/bin/grep".into(),
            "-q".into(),
            "missing".into(),
            "absent.txt".into(),
        ];
        let env = BTreeMap::new();
        let failed = ProcessInvocation {
            argv: &argv,
            shell: None,
            cwd: ".",
            env: &env,
            timeout_ms: 1_000,
            grace_ms: 10,
            stdout_cap: 1024,
            stderr_cap: 1024,
            run_revision: running.revision,
            effect_id: "failed-verification",
            attempt: 1,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        let output = engine
            .execute_verification(
                run,
                running.revision,
                &lease,
                5,
                &failed,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!output.command_succeeded());
        assert!(
            engine
                .complete_verified_run(run, running.revision, &lease, "failed".into(), 6)
                .unwrap_err()
                .to_string()
                .contains("verification failed")
        );

        let argv = vec!["/bin/pwd".into()];
        let passed = ProcessInvocation {
            argv: &argv,
            effect_id: "passed-verification",
            ..failed
        };
        engine
            .execute_verification(
                run,
                running.revision,
                &lease,
                7,
                &passed,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std::fs::write(dir.path().join("external.txt"), "changed outside engine").unwrap();
        assert!(
            engine
                .complete_verified_run(run, running.revision, &lease, "changed".into(), 8)
                .unwrap_err()
                .to_string()
                .contains("workspace changed")
        );
        std::fs::remove_file(dir.path().join("external.txt")).unwrap();
        assert!(matches!(
            engine.complete_verified_run(run, running.revision + 1, &lease, "stale".into(), 9),
            Err(StorageError::StaleRevision { .. })
        ));
        let argv = vec![
            "/usr/bin/grep".into(),
            "-q".into(),
            "missing".into(),
            "absent.txt".into(),
        ];
        let newer_failed = ProcessInvocation {
            argv: &argv,
            effect_id: "newer-failed-verification",
            ..passed
        };
        engine
            .execute_verification(
                run,
                running.revision,
                &lease,
                10,
                &newer_failed,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            engine
                .complete_verified_run(run, running.revision, &lease, "newer failed".into(), 11)
                .unwrap_err()
                .to_string()
                .contains("verification failed")
        );
        let argv = vec!["/bin/pwd".into()];
        for (stage, effect_id, now) in [
            (1, "pass-before-a-changed", 12),
            (2, "pass-before-b-changed", 20),
        ] {
            let pass = ProcessInvocation {
                argv: &argv,
                effect_id,
                ..newer_failed
            };
            engine
                .execute_verification(
                    run,
                    running.revision,
                    &lease,
                    now,
                    &pass,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            let path = dir.path().join(format!("race-{stage}.txt"));
            let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            engine.set_completion_hook({
                let counter = Arc::clone(&counter);
                move |seen| {
                    if seen == stage {
                        let value = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::fs::write(&path, value.to_string()).unwrap();
                    }
                }
            });
            assert!(
                engine
                    .complete_verified_run(run, running.revision, &lease, "raced".into(), now + 1)
                    .is_err()
            );
            engine.set_completion_hook(|_| {});
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            std::fs::write(dir.path().join("link-a"), "same").unwrap();
            std::fs::write(dir.path().join("link-b"), "same").unwrap();
            symlink("link-a", dir.path().join("topology-link")).unwrap();
            let topology_pass = ProcessInvocation {
                argv: &argv,
                effect_id: "topology-pass",
                ..newer_failed
            };
            engine
                .execute_verification(
                    run,
                    running.revision,
                    &lease,
                    29,
                    &topology_pass,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            std::fs::remove_file(dir.path().join("topology-link")).unwrap();
            symlink("link-b", dir.path().join("topology-link")).unwrap();
            assert!(
                engine
                    .complete_verified_run(run, running.revision, &lease, "swapped link".into(), 30)
                    .is_err()
            );
        }
        let final_pass = ProcessInvocation {
            argv: &argv,
            effect_id: "final-passed-verification",
            ..newer_failed
        };
        engine
            .execute_verification(
                run,
                running.revision,
                &lease,
                31,
                &final_pass,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let completed = engine
            .complete_verified_run(run, running.revision, &lease, "done".into(), 32)
            .unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        let handoff = completed.handoff.unwrap();
        assert_eq!(handoff.summary, "done");
        assert_eq!(
            handoff.evidence[0].status,
            latte_core::VerificationStatus::Passed
        );
    }

    #[tokio::test]
    async fn mutation_permission_is_durable_resumable_exact_and_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "old").unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 10_000).unwrap();
        let read_input = json!({"path":"a.txt"});
        let read = ToolInvocation {
            name: "read_file",
            input: &read_input,
            run_revision: 0,
            effect_id: "read",
            attempt: 1,
            precondition: None,
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: &lease.owner,
            lease_token: lease.fencing_token,
        };
        let snapshot = engine.execute_tool(run, &lease, 3, &read).unwrap();
        let hash = snapshot.value["sha256"].as_str().unwrap().to_owned();
        let write_input = json!({"path":"a.txt","content":"new"});
        let ask = ToolInvocation {
            name: "write_file",
            input: &write_input,
            run_revision: 0,
            effect_id: "write",
            attempt: 1,
            precondition: Some(&hash),
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: &lease.owner,
            lease_token: lease.fencing_token,
        };
        let digest = match engine.execute_tool(run, &lease, 4, &ask).unwrap_err() {
            ToolError::PermissionRequired { digest, .. } => digest,
            other => panic!("{other}"),
        };
        assert_eq!(
            engine.effect_status("write").unwrap(),
            EffectStatus::Prepared
        );
        drop(engine);
        let reopened = EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let lease = reopened.acquire_lease("owner", 5, 10_000).unwrap();
        let approved = ToolInvocation {
            approval_digest: Some(&digest),
            lease_owner: &lease.owner,
            lease_token: lease.fencing_token,
            ..ask
        };
        reopened.execute_tool(run, &lease, 6, &approved).unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap(), "new");
        assert_eq!(
            reopened.effect_status("write").unwrap(),
            EffectStatus::ObservedSuccess
        );
        assert!(matches!(
            reopened.execute_tool(run, &lease, 7, &approved),
            Err(ToolError::InvalidApproval)
        ));
    }

    #[tokio::test]
    async fn unsupported_safe_path_fails_before_consuming_permission_or_starting() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "old").unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let read_input = json!({"path":"a.txt"});
        let read = ToolInvocation {
            name: "read_file",
            input: &read_input,
            run_revision: 0,
            effect_id: "read-u",
            attempt: 1,
            precondition: None,
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: &lease.owner,
            lease_token: lease.fencing_token,
        };
        let hash = engine.execute_tool(run, &lease, 3, &read).unwrap().value["sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        let write_input = json!({"path":"a.txt","content":"new"});
        let ask = ToolInvocation {
            name: "write_file",
            input: &write_input,
            run_revision: 0,
            effect_id: "unsupported",
            attempt: 1,
            precondition: Some(&hash),
            timeout_ms: 0,
            output_cap: 1024,
            approval_digest: None,
            lease_owner: &lease.owner,
            lease_token: lease.fencing_token,
        };
        let ToolError::PermissionRequired { digest, .. } =
            engine.execute_tool(run, &lease, 4, &ask).unwrap_err()
        else {
            unreachable!()
        };
        let unsupported = EngineHandle {
            tools: Arc::new(
                tools::ToolRegistry::new_with_safe_support(dir.path(), None, &[], false).unwrap(),
            ),
            ..engine.clone()
        };
        let approved = ToolInvocation {
            approval_digest: Some(&digest),
            ..ask
        };
        assert!(matches!(
            unsupported.execute_tool(run, &lease, 5, &approved),
            Err(ToolError::Path(_))
        ));
        assert_eq!(
            engine.effect_status("unsupported").unwrap(),
            EffectStatus::Prepared
        );
        engine.execute_tool(run, &lease, 6, &approved).unwrap();
        assert_eq!(
            engine.effect_status("unsupported").unwrap(),
            EffectStatus::ObservedSuccess
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn public_authority_fences_lease_checkpoint_recovery_and_linked_run_bypasses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), "visible").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join("state.db"))
            .enabled_tools(["read_file", "write_file", "edit_file"])
            .deny_globs(["blocked/**"])
            .build()
            .unwrap();
        assert_eq!(format!("{engine:?}"), "EngineHandle { .. }");
        let names = engine
            .tool_descriptors()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(names.contains(&"read_file".to_owned()));
        #[cfg(unix)]
        assert!(names.contains(&"process".to_owned()));
        let manifest = engine.workspace_manifest().unwrap();
        assert!(!manifest.is_empty());
        assert!(manifest.keys().all(|path| !path.contains("state.db")));
        assert!(engine.changed_files().is_err());

        let ids = SystemIdSource::default();
        let run = RunId::from_uuid(ids.next_uuid_v7());
        assert_eq!(
            engine.create_run(run, 1).unwrap().status,
            latte_core::RunStatus::Queued
        );
        assert_eq!(engine.show(run).unwrap().run_id, run);
        assert_eq!(engine.list().unwrap().len(), 1);

        let lease = engine.acquire_lease("owner-a", 2, 5).unwrap();
        assert_eq!(lease.owner(), "owner-a");
        assert_eq!(lease.expires_at_ms(), 7);
        assert!(matches!(
            engine.acquire_lease("owner-b", 3, 5),
            Err(StorageError::EngineUnavailable)
        ));
        let lease = engine.acquire_lease("owner-a", 4, 8).unwrap();
        assert_eq!(lease.fencing_token(), 1);
        assert_eq!(lease.expires_at_ms(), 12);
        let renewed = engine.renew_lease(&lease, 5, 10).unwrap();
        assert_eq!(renewed.expires_at_ms(), 15);

        let mut events = engine.subscribe();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 6, &renewed)
            .unwrap();
        assert_eq!(running.status, latte_core::RunStatus::Running);
        assert_eq!(events.try_recv().unwrap().unwrap().run_id, run);
        assert!(events.try_recv().unwrap().is_none());
        engine
            .persist_runtime_checkpoint(
                run,
                running.revision,
                &renewed,
                r#"{"phase":"running"}"#,
                7,
            )
            .unwrap();
        assert_eq!(
            engine.runtime_checkpoint(run).unwrap().as_deref(),
            Some(r#"{"phase":"running"}"#)
        );
        assert!(matches!(
            engine.persist_runtime_checkpoint(
                run,
                running.revision + 1,
                &renewed,
                r#"{"phase":"stale"}"#,
                8,
            ),
            Err(StorageError::LeaseLost)
        ));
        assert!(engine.unknown_effects_for_run(run).unwrap().is_empty());
        assert!(engine.effect_status("missing-effect").is_err());
        assert!(
            !engine
                .permission_matches(
                    "missing-effect",
                    run,
                    running.revision,
                    &renewed,
                    "missing-digest",
                    9,
                )
                .unwrap()
        );
        assert!(matches!(
            engine.cancel_waiting_run(run, running.revision, &renewed, 9),
            Err(StorageError::InvalidData(_))
        ));
        assert!(matches!(
            engine.deny_waiting_permission(run, running.revision, &renewed, 9),
            Err(StorageError::InvalidData(_))
        ));
        assert!(matches!(
            engine.interrupt_after_lease_loss(run, &renewed, running.revision, 10),
            Err(StorageError::InvalidData(_))
        ));

        let takeover = engine.acquire_lease("owner-b", 15, 10).unwrap();
        assert_eq!(takeover.fencing_token(), renewed.fencing_token() + 1);
        assert!(matches!(
            engine.renew_lease(&renewed, 16, 10),
            Err(StorageError::LeaseLost)
        ));
        assert!(matches!(
            engine.release_lease(&renewed),
            Err(StorageError::LeaseLost)
        ));
        let interrupted = engine
            .interrupt_after_lease_loss(run, &renewed, running.revision, 16)
            .unwrap();
        let interrupted_revision = match interrupted {
            LeaseLossRecovery::Interrupted(state) => {
                assert_eq!(state.status, latte_core::RunStatus::Interrupted);
                state.revision
            }
            other => panic!("expected interruption, got {other:?}"),
        };
        assert_eq!(
            engine
                .interrupt_after_lease_loss(run, &renewed, running.revision, 17)
                .unwrap(),
            LeaseLossRecovery::FencedNoop
        );
        assert!(matches!(
            engine
                .interrupt_after_lease_loss(run, &renewed, interrupted_revision, 18)
                .unwrap(),
            LeaseLossRecovery::AlreadyTerminal(state)
                if state.status == latte_core::RunStatus::Interrupted
        ));
        engine.release_lease(&takeover).unwrap();
        assert!(matches!(
            engine.release_lease(&takeover),
            Err(StorageError::LeaseLost)
        ));

        let overflow = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        assert!(matches!(
            overflow.acquire_lease("overflow", u64::MAX, 1),
            Err(StorageError::InvalidData(_))
        ));

        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let linked_run = RunId::from_uuid(ids.next_uuid_v7());
        engine
            .create_thread_v2(thread_id, linked_run, test_thread_binding(), "linked", 20)
            .unwrap();
        let linked_lease = engine.acquire_lease("linked-owner", 21, 100).unwrap();
        for result in [
            engine.runtime_checkpoint(linked_run).map(|_| ()),
            engine.unknown_effects_for_run(linked_run).map(|_| ()),
            engine.persist_runtime_checkpoint(linked_run, 0, &linked_lease, "{}", 22),
            engine
                .interrupt_after_lease_loss(linked_run, &linked_lease, 0, 22)
                .map(|_| ()),
            engine
                .resolve_unknown_effect_and_abort(linked_run, "missing", 0, &linked_lease, 22)
                .map(|_| ()),
            engine
                .cancel_waiting_run(linked_run, 0, &linked_lease, 22)
                .map(|_| ()),
            engine
                .deny_waiting_permission(linked_run, 0, &linked_lease, 22)
                .map(|_| ()),
        ] {
            assert!(matches!(
                result,
                Err(StorageError::LinkedRunRequiresThreadCommit)
            ));
        }
        assert!(matches!(
            engine.apply_transition(linked_run, 0, Transition::Start, 22, &linked_lease),
            Err(StorageError::LinkedRunRequiresThreadCommit)
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn subscriptions_report_empty_event_lag_and_closed_without_becoming_authority() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let ids = SystemIdSource::default();
        let mut sync_events = engine.subscribe();
        let mut async_events = engine.subscribe();
        assert!(sync_events.try_recv().unwrap().is_none());
        let mut runs = Vec::new();
        for index in 0..40 {
            let run_id = RunId::from_uuid(ids.next_uuid_v7());
            engine
                .create_run(run_id, u64::try_from(index).unwrap())
                .unwrap();
            runs.push(run_id);
        }
        let lease = engine.acquire_lease("event-producer", 100, 10_000).unwrap();
        for (index, run_id) in runs.into_iter().enumerate() {
            engine
                .apply_transition(
                    run_id,
                    0,
                    Transition::Start,
                    u64::try_from(index + 101).unwrap(),
                    &lease,
                )
                .unwrap();
        }
        assert!(matches!(
            sync_events.try_recv(),
            Err(SubscriptionError::Lagged(count)) if count > 0
        ));
        assert!(matches!(
            async_events.recv().await,
            Err(SubscriptionError::Lagged(count)) if count > 0
        ));
        assert!(sync_events.try_recv().unwrap().is_some());

        let mut sync_threads = engine.subscribe_threads();
        let mut async_threads = engine.subscribe_threads();
        for index in 0..70 {
            let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
            let run_id = RunId::from_uuid(ids.next_uuid_v7());
            engine
                .create_thread_v2(
                    thread_id,
                    run_id,
                    test_thread_binding(),
                    &format!("thread-{index}"),
                    u64::try_from(index + 1_000).unwrap(),
                )
                .unwrap();
            engine
                .commit_thread_run_update(
                    ThreadCommitRequest {
                        thread_id,
                        run_id,
                        expected_thread_revision: 0,
                        expected_run_revision: 0,
                        command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: None,
                        update: CommitThreadRunUpdate::Start {
                            source_key: format!("start-{index}"),
                        },
                    },
                    &lease,
                    u64::try_from(index + 2_000).unwrap(),
                )
                .unwrap();
        }
        assert!(matches!(
            sync_threads.try_recv(),
            Err(SubscriptionError::Lagged(count)) if count > 0
        ));
        assert!(matches!(
            async_threads.recv().await,
            Err(SubscriptionError::Lagged(count)) if count > 0
        ));
        assert!(sync_threads.try_recv().unwrap().is_some());

        let closed = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let mut legacy_events_receiver = closed.subscribe();
        let mut async_events_receiver = closed.subscribe();
        let mut legacy_threads_receiver = closed.subscribe_threads();
        let mut async_threads_receiver = closed.subscribe_threads();
        drop(closed);
        assert_eq!(
            legacy_events_receiver.try_recv(),
            Err(SubscriptionError::Closed)
        );
        assert_eq!(
            async_events_receiver.recv().await,
            Err(SubscriptionError::Closed)
        );
        assert_eq!(
            legacy_threads_receiver.try_recv(),
            Err(SubscriptionError::Closed)
        );
        assert_eq!(
            async_threads_receiver.recv().await,
            Err(SubscriptionError::Closed)
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn started_effect_workers_revalidate_exact_policy_and_classify_observed_failures() {
        use sha2::{Digest, Sha256};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("read.txt"), "read-value").unwrap();
        std::fs::write(dir.path().join("duplicate.txt"), "same\nsame\n").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let lease = engine.acquire_lease("worker", 1, 10_000).unwrap();

        let read = descriptor("read_file", json!({"path":"read.txt"}));
        let (policy, read_digest) = engine
            .thread_effect_policy_and_digest(&read, 1, &lease)
            .unwrap();
        assert_eq!(policy, ThreadEffectPolicy::Allow);
        let observed = engine
            .execute_started_thread_tool(&read, 1, &read_digest, &lease, &CancellationToken::new())
            .unwrap();
        assert!(observed.success);
        assert!(observed.result.contains("read-value"));

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            engine.execute_started_thread_tool(&read, 1, &read_digest, &lease, &cancelled),
            Err(ThreadEffectExecutionError::Uncertain(message))
                if message.contains("cancelled")
        ));
        assert!(matches!(
            engine.execute_started_thread_tool(
                &read,
                1,
                "wrong-digest",
                &lease,
                &CancellationToken::new(),
            ),
            Err(ThreadEffectExecutionError::Uncertain(message))
                if message.contains("authorization changed")
        ));

        let disappearing = descriptor("read_file", json!({"path":"disappearing.txt"}));
        std::fs::write(dir.path().join("disappearing.txt"), "present").unwrap();
        let (_, disappearing_digest) = engine
            .thread_effect_policy_and_digest(&disappearing, 1, &lease)
            .unwrap();
        std::fs::remove_file(dir.path().join("disappearing.txt")).unwrap();
        assert!(matches!(
            engine.execute_started_thread_tool(
                &disappearing,
                1,
                &disappearing_digest,
                &lease,
                &CancellationToken::new(),
            ),
            Err(ThreadEffectExecutionError::Uncertain(message))
                if message.contains("precondition changed")
        ));

        let duplicate_hash = format!("{:x}", Sha256::digest(b"same\nsame\n"));
        let edit = descriptor(
            "edit_file",
            json!({
                "path":"duplicate.txt",
                "before":"same",
                "after":"changed",
                "precondition":duplicate_hash,
            }),
        );
        let (policy, edit_digest) = engine
            .thread_effect_policy_and_digest(&edit, 2, &lease)
            .unwrap();
        assert_eq!(policy, ThreadEffectPolicy::Ask);
        let observed_failure = engine
            .execute_started_thread_tool(&edit, 2, &edit_digest, &lease, &CancellationToken::new())
            .unwrap();
        assert!(!observed_failure.success);
        assert!(observed_failure.result.contains("match"));

        let denied = EngineBuilder::new()
            .workspace_root(dir.path())
            .deny_globs(["duplicate.txt"])
            .build()
            .unwrap();
        assert!(
            denied
                .thread_effect_policy_and_digest(&edit, 2, &lease)
                .unwrap_err()
                .to_string()
                .contains("denied")
        );
        let dangerous_process = descriptor("process", json!({"shell":"rm -rf /","cwd":"."}));
        let dangerous_error = engine
            .thread_effect_policy_and_digest(&dangerous_process, 2, &lease)
            .unwrap_err()
            .to_string();
        #[cfg(unix)]
        assert!(dangerous_error.contains("process policy denied"));
        #[cfg(not(unix))]
        assert!(dangerous_error.contains("unsupported on this platform"));
        let mut unsupported = engine.clone();
        unsupported.process_supervision_supported = false;
        assert!(
            unsupported
                .thread_effect_policy_and_digest(
                    &descriptor("process", json!({"argv":["/bin/pwd"]})),
                    2,
                    &lease,
                )
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );

        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let snapshot = engine
            .create_thread_v2(thread_id, run_id, test_thread_binding(), "worker", 10)
            .unwrap();
        let snapshot = engine
            .commit_thread_run_update(
                ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: snapshot.revision,
                    expected_run_revision: snapshot.runs[0].run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitThreadRunUpdate::Start {
                        source_key: "worker:start".into(),
                    },
                },
                &lease,
                11,
            )
            .unwrap()
            .snapshot;
        let run_revision = snapshot.runs[0].run_revision;

        let read = descriptor("read_file", json!({"path":"read.txt"}));
        let (_, read_digest) = engine
            .thread_effect_policy_and_digest(&read, run_revision, &lease)
            .unwrap();
        let read_started = ThreadEffectStarted {
            snapshot: snapshot.clone(),
            presentation: ThreadEffectPresentation::from_descriptor(&read),
            operation_digest: read_digest,
            descriptor: read,
        };
        assert!(
            engine
                .execute_started_thread_effect(&read_started, &lease, &CancellationToken::new(),)
                .await
                .unwrap()
                .success
        );
        let completion_descriptor = read_started.descriptor.clone();
        let mut missing_run = read_started.clone();
        missing_run.snapshot.active_run_id = None;
        assert!(matches!(
            engine
                .execute_started_thread_effect(
                    &missing_run,
                    &lease,
                    &CancellationToken::new(),
                )
                .await,
            Err(ThreadEffectExecutionError::Uncertain(message))
                if message.contains("started run is missing")
        ));
        let mut stale_digest = read_started;
        stale_digest.operation_digest = "stale".into();
        assert!(matches!(
            engine
                .execute_started_thread_effect(
                    &stale_digest,
                    &lease,
                    &CancellationToken::new(),
                )
                .await,
            Err(ThreadEffectExecutionError::Uncertain(message))
                if message.contains("exact operation digest")
        ));

        let prepared = engine
            .prepare_thread_effect(
                ThreadEffectRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: snapshot.revision,
                    expected_run_revision: run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    source_key: "worker:prepare-read".into(),
                    descriptor: completion_descriptor,
                },
                &lease,
                12,
            )
            .unwrap();
        assert_eq!(
            engine.thread_effect_digest("effect-read_file").unwrap(),
            prepared.operation_digest
        );
        let started = engine
            .start_thread_effect(
                ThreadEffectStartRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: prepared.snapshot.revision,
                    expected_run_revision: prepared.snapshot.runs[0].run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    source_key: "worker:start-read".into(),
                    effect_id: "effect-read_file".into(),
                },
                prepared.operation_digest,
                &lease,
                13,
            )
            .unwrap();
        let value = engine
            .execute_started_thread_effect(&started, &lease, &CancellationToken::new())
            .await
            .unwrap();
        let verification = ProcessOutput {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            termination: ProcessTermination::Exited,
        };
        let observed = engine
            .observe_thread_effect(
                &started,
                "worker:observe-read".into(),
                latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                value,
                &lease,
                14,
            )
            .unwrap();
        let observed_revision = observed.snapshot.runs[0].run_revision;
        engine
            .record_thread_verification(
                run_id,
                observed_revision,
                "effect-read_file",
                &verification,
                &lease,
                15,
            )
            .unwrap();
        let completed = engine
            .complete_thread_verified(
                &observed.snapshot,
                "verified worker".into(),
                "effect-read_file".into(),
                &lease,
                16,
            )
            .unwrap();
        assert_eq!(completed.lifecycle, latte_core::ThreadLifecycle::Ready);
        assert_eq!(engine.list_threads_v2().unwrap()[0], completed);
        assert_eq!(
            engine.thread_snapshot_v2(thread_id, None, 100).unwrap(),
            completed
        );
        let follow_up_run = RunId::from_uuid(ids.next_uuid_v7());
        let follow_up = engine
            .create_thread_follow_up_v2(
                thread_id,
                follow_up_run,
                completed.revision,
                "follow up",
                17,
            )
            .unwrap();
        assert_eq!(follow_up.active_run_id, Some(follow_up_run));
    }
}
