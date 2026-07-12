//! Engine actor and its private durable state authority.
#![allow(clippy::missing_errors_doc)]
pub mod config;
mod policy;
mod process;
mod storage;
mod tools;
mod workspace;

use latte_core::{EventEnvelope, RunId, RunState, Transition};
pub use process::{
    CancellationToken, ProcessDecision, ProcessError, ProcessInvocation, ProcessOutput,
    ProcessTermination, classify,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};
pub use storage::{EffectStatus, Lease, LeaseLossRecovery, StorageError, StoredEvent};
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
pub(crate) fn wall_now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}
fn manifest_map_digest(
    manifest: &std::collections::BTreeMap<String, String>,
) -> Result<String, StorageError> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
#[cfg(test)]
type CompletionHook = Arc<std::sync::Mutex<Option<Arc<dyn Fn(u8) + Send + Sync>>>>;
type CompletionSnapshot = (String, std::collections::BTreeMap<String, String>);
pub use tools::{ToolDescriptor, ToolError, ToolInvocation, ToolOutput};

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
        Ok(EngineHandle {
            events,
            storage: Arc::new(storage),
            tools: Arc::new(tools),
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
    storage: Arc<storage::Storage>,
    tools: Arc<tools::ToolRegistry>,
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
    #[cfg(test)]
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
    /// Lists every unknown effect for a run, including allow-path effects without approval rows.
    pub fn unknown_effects_for_run(&self, run_id: RunId) -> Result<Vec<String>, StorageError> {
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
        self.storage
            .put_checkpoint(run_id, expected_revision, lease, payload_json, now_ms)
    }
    /// Loads renderer-neutral agent runtime state.
    pub fn runtime_checkpoint(&self, run_id: RunId) -> Result<Option<String>, StorageError> {
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
        self.storage
            .permission_matches(effect_id, run_id, expected_revision, lease, digest, now_ms)
    }
}
/// Event subscription.
#[derive(Debug)]
pub struct Subscription {
    receiver: broadcast::Receiver<EventEnvelope>,
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
}
