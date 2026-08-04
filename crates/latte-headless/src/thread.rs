//! Thread v2 composition for the transcript-first clients.
//!
//! The service is deliberately a coordinator, not an effect authority: every
//! durable change goes through `EngineHandle::commit_thread_run_update` and
//! provider calls receive no direct repository capability.

use crate::{
    context,
    provider::{
        Message, Provider, ProviderContext, ProviderError, ProviderEvent, ProviderEventSink,
        ProviderRequest, valid_tool_call_id,
    },
    registry::ResolvedProvider,
    runtime::VerificationPlan,
};
use latte_core::{
    FailureCode, Retryability, RunFailure, RunId, ThreadCommandId, ThreadId, ThreadLifecycle,
    ThreadProviderBindingV2, ThreadSnapshot, ThreadTransientProgress, TranscriptKind,
    redact_thread_text, valid_openai_chat_input_request_id, wall_time_ms as now_ms,
};
use latte_engine::{
    CancellationToken, CommitThreadRunUpdate, EngineHandle, Lease, StorageError,
    ThreadCommitRequest, ThreadEffectDescriptor, ThreadEffectExecutionError,
    ThreadEffectPresentation, ThreadEffectRequest, ThreadEffectStartRequest, ThreadEffectStarted,
    ThreadLeaseLossRecovery,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

const THREAD_VERIFICATION_EFFECT_PREFIX: &str = "thread-verification:";

fn append_denied_tool_results(segment: &mut Vec<Message>) {
    let calls = segment
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant { tool_calls, .. } => Some(tool_calls.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let observed = segment
        .iter()
        .filter_map(|message| match message {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    segment.extend(calls.into_iter().filter_map(|call| {
        (!observed.contains(&call.id)).then_some(Message::Tool {
            tool_call_id: call.id,
            name: Some(call.name),
            content: "permission denied by user; tool was not executed".into(),
        })
    }));
}

/// Exact request-size policy for child history construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadHistoryPolicy {
    pub max_request_bytes: usize,
    pub max_input_bytes: usize,
    pub reserved_output_bytes: usize,
    pub context_cap_bytes: usize,
}

impl Default for ThreadHistoryPolicy {
    fn default() -> Self {
        Self {
            max_request_bytes: 512 * 1024,
            max_input_bytes: 384 * 1024,
            reserved_output_bytes: 128 * 1024,
            context_cap_bytes: 64 * 1024,
        }
    }
}

impl ThreadHistoryPolicy {
    /// Validates the exact input/output budget without constructing a request.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_request_bytes == 0
            || self.max_input_bytes == 0
            || self.reserved_output_bytes >= self.max_input_bytes
            || self.context_cap_bytes == 0
        {
            return Err("max_request_bytes/context cap must be nonzero and reserved output must be smaller than input budget".into());
        }
        Ok(())
    }
    fn budget(&self) -> Result<usize, ThreadRuntimeError> {
        self.validate().map_err(ThreadRuntimeError::History)?;
        Ok(self
            .max_request_bytes
            .min(self.max_input_bytes - self.reserved_output_bytes))
    }
}

/// A secret-resolving provider constructor. Callers must validate the supplied
/// binding first; the registry's `resolve_thread_bound` does exactly that.
pub type ThreadProviderFactory =
    Arc<dyn Fn(&ThreadProviderBindingV2) -> Result<ResolvedProvider, String> + Send + Sync>;

/// Non-durable provider progress bridge. It is intentionally separate from
/// transcript persistence and is cleared by the TUI on a gap or reconnect.
pub trait ThreadProgressSink: Send + Sync {
    fn observe(&self, progress: ThreadTransientProgress);
}
impl<F: Fn(ThreadTransientProgress) + Send + Sync> ThreadProgressSink for F {
    fn observe(&self, progress: ThreadTransientProgress) {
        self(progress);
    }
}

#[derive(Debug, Error)]
pub enum ThreadRuntimeError {
    #[error("thread storage: {0}")]
    Storage(#[from] StorageError),
    #[error("thread provider configuration: {0}")]
    ProviderConfiguration(String),
    #[error("thread provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("thread history: {0}")]
    History(String),
    #[error("thread is not in the requested active state")]
    InvalidState,
    #[error("thread effect: {0}")]
    Effect(String),
}

/// Transcript-first provider coordinator. The active cancellation map is
/// process-local only; durable recovery remains in the engine.
#[derive(Clone)]
pub struct ThreadRuntimeService {
    engine: EngineHandle,
    root: PathBuf,
    policy: ThreadHistoryPolicy,
    provider: ThreadProviderFactory,
    active: Arc<Mutex<HashMap<ThreadId, CancellationToken>>>,
    progress: Option<Arc<dyn ThreadProgressSink>>,
    verification: Option<VerificationPlan>,
    lease_ttl_ms: u64,
}

struct ThreadLeaseGuard {
    engine: EngineHandle,
    lease: Lease,
}

impl std::ops::Deref for ThreadLeaseGuard {
    type Target = Lease;

    fn deref(&self) -> &Self::Target {
        &self.lease
    }
}

impl Drop for ThreadLeaseGuard {
    fn drop(&mut self) {
        let _ = self.engine.release_lease(&self.lease);
    }
}

impl ThreadRuntimeService {
    #[must_use]
    pub fn new(
        engine: EngineHandle,
        root: impl AsRef<Path>,
        policy: ThreadHistoryPolicy,
        provider: ThreadProviderFactory,
    ) -> Self {
        Self {
            engine,
            root: root.as_ref().to_owned(),
            policy,
            provider,
            active: Arc::new(Mutex::new(HashMap::new())),
            progress: None,
            verification: None,
            lease_ttl_ms: 60_000,
        }
    }

    /// Connects typed transient provider progress to an interactive frontend.
    #[must_use]
    pub fn with_progress_sink(mut self, sink: Arc<dyn ThreadProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    /// Adds the workspace-configured verification contract.  A child which
    /// changed the engine-owned workspace cannot complete until this process
    /// has been observed and its evidence has been fenced into the handoff.
    #[must_use]
    pub fn with_verification(mut self, plan: VerificationPlan) -> Self {
        self.verification = Some(plan);
        self
    }

    /// Overrides the coordinator lease duration.  Production uses the
    /// conservative one-minute default; a shorter bounded value is useful for
    /// deterministic restart and heartbeat tests.
    #[must_use]
    pub fn with_lease_ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.lease_ttl_ms = ttl_ms.max(10);
        self
    }

    /// Starts a new v2 conversation. The complete non-secret binding is
    /// validated and the accepted user submission is persisted before
    /// provider construction can resolve an environment key.
    pub async fn start(
        &self,
        thread_id: ThreadId,
        prompt: String,
        binding: ThreadProviderBindingV2,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        binding
            .validate()
            .map_err(ThreadRuntimeError::ProviderConfiguration)?;
        let messages = self.initial_messages(&prompt)?;
        let run_id = new_run_id();
        let now = now_ms();
        let lease = self.acquire(thread_id)?;
        let started = self
            .engine
            .create_started_thread_v2(thread_id, run_id, binding, &prompt, &lease, now)?;
        // Provider construction is runtime work, not submission validation.
        // Once the user card is durable, a missing credential or invalid model
        // becomes a visible retryable child failure instead of restoring the
        // composer and making the accepted prompt appear to vanish.
        let Ok(provider) = (self.provider)(&started.binding) else {
            return self.fail_retryable(
                thread_id,
                run_id,
                started.revision,
                active_run_revision(&started)?,
                provider_configuration_failure_message(),
                &lease,
            );
        };
        self.run_provider_turn(started, messages, provider.provider, lease)
            .await
    }

    /// Creates a child only after the complete history fits exactly. The
    /// completed parent remains untouched in both v1 and v2 tables.
    pub async fn follow_up(
        &self,
        thread_id: ThreadId,
        expected_thread_revision: u64,
        prompt: String,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let snapshot = self.load_full(thread_id)?;
        if snapshot.revision != expected_thread_revision || !snapshot.lifecycle.accepts_follow_up()
        {
            return Err(ThreadRuntimeError::InvalidState);
        }
        let messages = self.history_with_prompt(&snapshot, &prompt)?;
        let run_id = new_run_id();
        let lease = self.acquire(thread_id)?;
        let started = self.engine.create_started_thread_follow_up_v2(
            thread_id,
            run_id,
            expected_thread_revision,
            &prompt,
            &lease,
            now_ms(),
        )?;
        let Ok(provider) = (self.provider)(&started.binding) else {
            return self.fail_retryable(
                thread_id,
                run_id,
                started.revision,
                active_run_revision(&started)?,
                provider_configuration_failure_message(),
                &lease,
            );
        };
        self.run_provider_turn(started, messages, provider.provider, lease)
            .await
    }

    /// Persists an explicit provider/model selection for subsequent children.
    /// Credential resolution remains deferred until the next accepted prompt.
    pub fn switch_model(
        &self,
        thread_id: ThreadId,
        expected_thread_revision: u64,
        binding: &ThreadProviderBindingV2,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        binding
            .validate()
            .map_err(ThreadRuntimeError::ProviderConfiguration)?;
        let snapshot = self.load_full(thread_id)?;
        if snapshot.revision != expected_thread_revision
            || !snapshot.lifecycle.accepts_follow_up()
            || snapshot.active_run_id.is_some()
        {
            return Err(ThreadRuntimeError::InvalidState);
        }
        if snapshot.binding == *binding {
            return Ok(snapshot);
        }
        let lease = self.acquire(thread_id)?;
        self.engine
            .switch_thread_binding_v2(
                thread_id,
                expected_thread_revision,
                binding,
                &lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    /// Provides a non-secret request value and continues the same child.
    pub async fn provide_input(
        &self,
        thread_id: ThreadId,
        expected_thread_revision: u64,
        request_id: String,
        value: String,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let snapshot = self.load_full(thread_id)?;
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let run = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .ok_or(ThreadRuntimeError::InvalidState)?;
        if snapshot.lifecycle != ThreadLifecycle::WaitingInput
            || snapshot.revision != expected_thread_revision
        {
            return Err(ThreadRuntimeError::InvalidState);
        }
        let messages = self.history_with_prompt(&snapshot, &value)?;
        let provider = (self.provider)(&snapshot.binding)
            .map_err(ThreadRuntimeError::ProviderConfiguration)?;
        let lease = self.acquire(thread_id)?;
        let running = self.commit(
            thread_id,
            run_id,
            snapshot.revision,
            run.run_revision,
            CommitThreadRunUpdate::ProvideInput {
                source_key: format!("{run_id}:input:{request_id}"),
                request_id,
                value,
            },
            &lease,
        )?;
        self.run_provider_turn(running, messages, provider.provider, lease)
            .await
    }

    /// Resolves a durable v2 effect permission. Allow consumes the exact
    /// prepared approval through the engine Started transaction before any
    /// external operation is invoked; denial terminalizes the prepared effect
    /// without executing it.
    pub async fn resolve_permission(
        &self,
        thread_id: ThreadId,
        expected_thread_revision: u64,
        request_id: String,
        allow: bool,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let snapshot = self.load_full(thread_id)?;
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let run_revision = active_run_revision(&snapshot)?;
        if snapshot.lifecycle != ThreadLifecycle::WaitingPermission
            || snapshot.revision != expected_thread_revision
            || snapshot.pending.as_ref().and_then(|pending| match pending {
                latte_core::ThreadPendingRequest::Permission { request_id, .. } => {
                    Some(request_id.as_str())
                }
                latte_core::ThreadPendingRequest::Input { .. } => None,
            }) != Some(request_id.as_str())
        {
            return Err(ThreadRuntimeError::InvalidState);
        }
        let verification = request_id.starts_with(THREAD_VERIFICATION_EFFECT_PREFIX);
        // Validate the immutable Provider binding before approval can start an
        // external effect. A configuration/model mismatch is not authority to
        // consume permission or execute the tool; the Session remains at the
        // same durable waiting boundary so the user can choose another path.
        let provider = if allow && !verification {
            Some(
                (self.provider)(&snapshot.binding)
                    .map_err(ThreadRuntimeError::ProviderConfiguration)?,
            )
        } else {
            None
        };
        let lease = self.acquire(thread_id)?;
        let resolved = self.engine.resolve_thread_effect_permission(
            thread_id,
            run_id,
            snapshot.revision,
            run_revision,
            request_id.clone(),
            format!(
                "{run_id}:permission:{request_id}:{}",
                if allow { "allow" } else { "deny" }
            ),
            allow,
            ThreadCommandId::from_uuid(Uuid::now_v7()),
            &lease,
            now_ms(),
        )?;
        if !allow {
            return Ok(resolved);
        }
        // The assistant card is the durable, ordered queue for the complete
        // provider tool round.  Do not reconstruct a new provider turn after
        // this one approved call: OpenAI-compatible history requires a tool
        // result for every call in the original assistant message, in order.
        let started = self.engine.start_thread_effect(
            thread_effect_start_request(
                &resolved,
                request_id.clone(),
                format!("{run_id}:effect:{request_id}:start"),
            )?,
            self.engine.thread_effect_digest(&request_id)?,
            &lease,
            now_ms(),
        )?;
        let presentation = started.presentation.clone();
        // The assistant card is the durable, ordered queue for the complete
        // provider tool round. The presentation is redacted, but its call ID
        // is enough to find that queue; executable input remains engine-only.
        let continuation = (!verification)
            .then(|| tool_round_for_call(&resolved, &presentation.tool_call_id))
            .transpose()?;
        let after_effect = self.execute_and_observe_effect(started, &lease).await?;
        if after_effect.lifecycle != ThreadLifecycle::Running {
            return Ok(after_effect);
        }
        if verification {
            return self.finish_verification(&after_effect, &presentation, &lease);
        }
        let (round_sequence, calls, ordinal) = continuation.ok_or_else(|| {
            ThreadRuntimeError::Effect("provider tool continuation is missing".into())
        })?;
        let provider = provider.ok_or_else(|| {
            ThreadRuntimeError::ProviderConfiguration(
                "provider was not resolved before effect approval".into(),
            )
        })?;
        let messages = self.history_from_snapshot(&after_effect)?;
        self.continue_provider_tool_round(
            after_effect,
            messages,
            calls,
            ordinal.saturating_add(1),
            round_sequence,
            provider.provider,
            lease,
        )
        .await
    }

    /// Explicitly resolves an Unknown v2 effect through the v2 commit path.
    pub fn reconcile_unknown_effect(
        &self,
        thread_id: ThreadId,
        effect_id: &str,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let snapshot = self.load_full(thread_id)?;
        let run_id = snapshot
            .latest_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        if snapshot.lifecycle != ThreadLifecycle::ReconciliationRequired {
            return Err(ThreadRuntimeError::InvalidState);
        }
        let run_revision = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .map(|run| run.run_revision)
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let lease = self.acquire(thread_id)?;
        self.engine
            .reconcile_thread_effect_unknown(
                thread_id,
                run_id,
                snapshot.revision,
                run_revision,
                effect_id.to_owned(),
                format!("{run_id}:effect:{effect_id}:reconcile"),
                ThreadCommandId::from_uuid(Uuid::now_v7()),
                &lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    /// Cancellation is explicit. No composer input has a run ID before start,
    /// so canceling it is necessarily local and never reaches this method.
    pub fn cancel(&self, thread_id: ThreadId) {
        if let Some(token) = self
            .active
            .lock()
            .expect("active mutex poisoned")
            .get(&thread_id)
        {
            token.cancel();
        }
    }

    /// Cancels a durable waiting/idle active child. An in-flight provider call
    /// is signalled first and commits its own interruption without a partial
    /// assistant card; a waiting request is terminally cancelled immediately.
    pub fn cancel_durable(
        &self,
        thread_id: ThreadId,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        if self
            .active
            .lock()
            .expect("active mutex poisoned")
            .contains_key(&thread_id)
        {
            self.cancel(thread_id);
            return self.load_full(thread_id);
        }
        let snapshot = self.load_full(thread_id)?;
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let run_revision = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .ok_or(ThreadRuntimeError::InvalidState)?
            .run_revision;
        let lease = self.acquire(thread_id)?;
        self.commit(
            thread_id,
            run_id,
            snapshot.revision,
            run_revision,
            CommitThreadRunUpdate::Interrupt {
                source_key: format!("{run_id}:cancel"),
                reconciliation_effect_id: None,
            },
            &lease,
        )
    }

    fn initial_messages(&self, prompt: &str) -> Result<Vec<Message>, ThreadRuntimeError> {
        let context = context::build(&self.root, None, self.policy.context_cap_bytes)
            .map_err(|error| ThreadRuntimeError::History(error.to_string()))?;
        let system = format!(
            "You are Latte Code. Work only in the supplied repository context.{}",
            context.text
        );
        self.enforce_budget(vec![
            Message::System {
                content: redact_thread_text(&system),
            },
            Message::User {
                content: redact_thread_text(prompt),
            },
        ])
    }

    fn history_with_prompt(
        &self,
        snapshot: &ThreadSnapshot,
        prompt: &str,
    ) -> Result<Vec<Message>, ThreadRuntimeError> {
        let context = context::build(&self.root, None, self.policy.context_cap_bytes)
            .map_err(|error| ThreadRuntimeError::History(error.to_string()))?;
        let system = Message::System {
            content: redact_thread_text(&format!(
                "You are Latte Code. Work only in the supplied repository context.{}",
                context.text
            )),
        };
        let mut segments: Vec<Vec<Message>> = Vec::new();
        for entry in &snapshot.transcript.entries {
            match entry.kind {
                TranscriptKind::User => segments.push(vec![Message::User {
                    content: entry.text.clone(),
                }]),
                TranscriptKind::Assistant => {
                    if let Some(segment) = segments.last_mut() {
                        let tool_calls = entry
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("tool_calls"))
                            .and_then(|calls| serde_json::from_value(calls.clone()).ok())
                            .unwrap_or_default();
                        segment.push(Message::Assistant {
                            content: Some(entry.text.clone()),
                            tool_calls,
                        });
                    }
                }
                TranscriptKind::ToolResult => {
                    if let Some(segment) = segments.last_mut()
                        && let Some(payload) = entry.payload.as_ref()
                        && let (Some(tool_call_id), Some(content)) = (
                            payload
                                .get("tool_call_id")
                                .and_then(serde_json::Value::as_str),
                            payload
                                .get("provider_content")
                                .and_then(serde_json::Value::as_str),
                        )
                    {
                        segment.push(Message::Tool {
                            tool_call_id: tool_call_id.into(),
                            name: payload
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned),
                            content: content.into(),
                        });
                    }
                }
                TranscriptKind::Failure
                    if entry.payload.as_ref().is_some_and(|payload| {
                        payload
                            .get("provider_tool_round_aborted")
                            .and_then(serde_json::Value::as_str)
                            == Some("permission_denied")
                    }) =>
                {
                    // OpenAI-compatible history requires one tool result for
                    // every call in an assistant tool round. A denial ends
                    // the immutable child before execution, so synthesize
                    // bounded non-execution results for every unobserved call
                    // when constructing the next child's provider history.
                    if let Some(segment) = segments.last_mut() {
                        append_denied_tool_results(segment);
                    }
                }
                // ToolCall cards describe the engine ledger rather than a
                // provider grammar. The preceding assistant card carries the
                // exact tool-call envelope.
                TranscriptKind::ToolCall
                | TranscriptKind::Permission
                | TranscriptKind::Input
                | TranscriptKind::Failure
                | TranscriptKind::Completion
                | TranscriptKind::System => {}
            }
        }
        segments.push(vec![Message::User {
            content: redact_thread_text(prompt),
        }]);
        let budget = self.policy.budget()?;
        let mut selected = Vec::new();
        for segment in segments.into_iter().rev() {
            let mut candidate = Vec::with_capacity(selected.len() + segment.len() + 1);
            candidate.push(system.clone());
            candidate.extend(segment.iter().cloned());
            candidate.extend(selected.iter().cloned());
            if wire_bytes(&candidate)? > budget {
                if selected.is_empty() {
                    return Err(ThreadRuntimeError::History(
                        "newest complete user segment exceeds the exact request budget".into(),
                    ));
                }
                break;
            }
            let mut next = segment;
            next.extend(selected);
            selected = next;
        }
        let mut messages = vec![system];
        messages.extend(selected);
        self.enforce_budget(messages)
    }

    fn enforce_budget(&self, messages: Vec<Message>) -> Result<Vec<Message>, ThreadRuntimeError> {
        let bytes = wire_bytes(&messages)?;
        if bytes > self.policy.budget()? {
            return Err(ThreadRuntimeError::History(format!(
                "request is {bytes} bytes and exceeds the exact budget"
            )));
        }
        Ok(messages)
    }

    fn history_from_snapshot(
        &self,
        snapshot: &ThreadSnapshot,
    ) -> Result<Vec<Message>, ThreadRuntimeError> {
        let mut messages = self.history_with_prompt(snapshot, "")?;
        if matches!(messages.last(), Some(Message::User { content }) if content.is_empty()) {
            let _ = messages.pop();
        }
        Ok(messages)
    }

    fn verification_descriptor(
        &self,
        snapshot: &ThreadSnapshot,
        summary: &str,
    ) -> Result<ThreadEffectDescriptor, ThreadRuntimeError> {
        let plan = self.verification.as_ref().ok_or_else(|| {
            ThreadRuntimeError::Effect(
                "workspace changed but no configured verification plan is available".into(),
            )
        })?;
        if plan.argv.is_empty() {
            return Err(ThreadRuntimeError::Effect(
                "configured verification argv is empty".into(),
            ));
        }
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        Ok(ThreadEffectDescriptor {
            effect_id: format!("{THREAD_VERIFICATION_EFFECT_PREFIX}{run_id}"),
            tool_call_id: format!("verification-{run_id}"),
            name: "process".into(),
            input: serde_json::json!({
                "argv": plan.argv,
                "cwd": plan.cwd,
                "env": BTreeMap::<String, String>::new(),
                "timeout_ms": plan.timeout_ms,
                "grace_ms": plan.grace_ms,
                "stdout_cap": plan.stdout_cap,
                "stderr_cap": plan.stderr_cap,
                // The summary is non-secret transcript content. Keeping it
                // with the durable verification descriptor lets an Ask
                // approval resume after restart without trusting RAM.
                "completion_summary": redact_thread_text(summary),
            }),
            attempt: 1,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn begin_verification(
        &self,
        snapshot: ThreadSnapshot,
        summary: String,
        lease: ThreadLeaseGuard,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let descriptor = self.verification_descriptor(&snapshot, &summary)?;
        let prepared = self.engine.prepare_thread_effect(
            thread_effect_request(
                &snapshot,
                descriptor.clone(),
                format!("{run_id}:verification:prepare"),
            )?,
            &lease,
            now_ms(),
        )?;
        if prepared.policy == latte_engine::ThreadEffectPolicy::Ask {
            return Ok(prepared.snapshot);
        }
        let started = self.engine.start_thread_effect(
            thread_effect_start_request(
                &prepared.snapshot,
                descriptor.effect_id.clone(),
                format!("{run_id}:verification:start"),
            )?,
            prepared.operation_digest,
            &lease,
            now_ms(),
        )?;
        let presentation = started.presentation.clone();
        let observed = self.execute_and_observe_effect(started, &lease).await?;
        if observed.lifecycle != ThreadLifecycle::Running {
            return Ok(observed);
        }
        self.finish_verification(&observed, &presentation, &lease)
    }

    fn finish_verification(
        &self,
        snapshot: &ThreadSnapshot,
        descriptor: &ThreadEffectPresentation,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let run_revision = active_run_revision(snapshot)?;
        let raw_output =
            effect_provider_result(snapshot, &descriptor.tool_call_id).ok_or_else(|| {
                ThreadRuntimeError::Effect("verification observation is missing".into())
            })?;
        let output: latte_engine::ProcessOutput =
            serde_json::from_str(&raw_output).map_err(|_| {
                ThreadRuntimeError::Effect(
                    "verification observation is not a process result".into(),
                )
            })?;
        self.engine.record_thread_verification(
            run_id,
            run_revision,
            &descriptor.effect_id,
            &output,
            lease,
            now_ms(),
        )?;
        if !output.command_succeeded() {
            return self.fail(
                snapshot.thread_id,
                run_id,
                snapshot.revision,
                run_revision,
                "configured verification failed; evidence was recorded".into(),
                lease,
            );
        }
        let summary = descriptor
            .input
            .get("completion_summary")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ThreadRuntimeError::Effect("verification completion summary is missing".into())
            })?
            .to_owned();
        self.engine
            .complete_thread_verified(
                snapshot,
                summary,
                descriptor.effect_id.clone(),
                lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_provider_tool_round(
        &self,
        snapshot: ThreadSnapshot,
        mut messages: Vec<Message>,
        response: crate::provider::ProviderResponse,
        provider: Arc<dyn Provider>,
        lease: ThreadLeaseGuard,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let thread_id = snapshot.thread_id;
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let run_revision = active_run_revision(&snapshot)?;
        let known_tools = self
            .engine
            .tool_descriptors()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<std::collections::BTreeSet<_>>();
        let mut ids = std::collections::BTreeSet::new();
        if response.tool_calls.iter().any(|call| {
            !valid_tool_call_id(&call.id)
                || !ids.insert(call.id.clone())
                || !known_tools.contains(&call.name)
                || !call.input.is_object()
        }) {
            return self.fail(
                thread_id,
                run_id,
                snapshot.revision,
                run_revision,
                "provider tool call ids must match [A-Za-z0-9_-]{1,256}, be unique, known, and object-shaped".into(),
                &lease,
            );
        }
        let assistant_text = response.message.clone().unwrap_or_default();
        let first_tool_call_id = response.tool_calls[0].id.clone();
        let tool_calls = response.tool_calls;
        let current = self.commit(
            thread_id,
            run_id,
            snapshot.revision,
            run_revision,
            CommitThreadRunUpdate::AppendTranscript {
                source_key: format!("{run_id}:assistant-tool-round:{first_tool_call_id}"),
                kind: TranscriptKind::Assistant,
                text: assistant_text.clone(),
                // This is intentionally more than display data: it is the
                // durable ordered continuation queue.  A restart while an
                // Ask call waits for approval reloads this exact assistant
                // envelope and completes the remaining calls before another
                // provider request can be made.
                payload: Some(serde_json::json!({"tool_calls":tool_calls.clone()})),
            },
            &lease,
        )?;
        messages.push(Message::Assistant {
            content: response.message,
            tool_calls: tool_calls.clone(),
        });
        let round_sequence = current.sequence;
        self.continue_provider_tool_round(
            current,
            messages,
            tool_calls,
            0,
            round_sequence,
            provider,
            lease,
        )
        .await
    }

    /// Continues one persisted assistant tool round. `start_ordinal` is
    /// durable through the `ToolResult` cards already in history; callers only
    /// use this helper after loading the authoritative thread snapshot.
    #[allow(clippy::too_many_arguments)]
    async fn continue_provider_tool_round(
        &self,
        mut current: ThreadSnapshot,
        mut messages: Vec<Message>,
        calls: Vec<crate::provider::ToolCall>,
        start_ordinal: usize,
        round_sequence: u64,
        provider: Arc<dyn Provider>,
        lease: ThreadLeaseGuard,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let run_id = current
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        for (ordinal, call) in calls.into_iter().enumerate().skip(start_ordinal) {
            let descriptor = ThreadEffectDescriptor {
                // Provider IDs only need to be unique within one response.
                // Include the durable assistant sequence to prevent a later
                // response reusing an ID from colliding with this effect.
                effect_id: format!(
                    "thread-effect:{run_id}:{round_sequence}:{ordinal}:{}",
                    call.id
                ),
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
                attempt: 1,
            };
            let prepared = self.engine.prepare_thread_effect(
                thread_effect_request(
                    &current,
                    descriptor.clone(),
                    format!("{run_id}:effect:{}:{ordinal}:prepare", call.id),
                )?,
                &lease,
                now_ms(),
            )?;
            current = prepared.snapshot;
            if prepared.policy == latte_engine::ThreadEffectPolicy::Ask {
                return Ok(current);
            }
            let started = self.engine.start_thread_effect(
                thread_effect_start_request(
                    &current,
                    descriptor.effect_id,
                    format!("{run_id}:effect:{}:{ordinal}:start", call.id),
                )?,
                prepared.operation_digest,
                &lease,
                now_ms(),
            )?;
            current = self.execute_and_observe_effect(started, &lease).await?;
            if current.lifecycle != ThreadLifecycle::Running {
                return Ok(current);
            }
            let result = effect_provider_result(&current, &call.id)
                .ok_or_else(|| ThreadRuntimeError::Effect("missing observed tool result".into()))?;
            messages.push(Message::Tool {
                tool_call_id: call.id,
                name: Some(call.name),
                content: result,
            });
        }
        Box::pin(self.run_provider_turn(current, messages, provider, lease)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_and_observe_effect(
        &self,
        started: ThreadEffectStarted,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let thread_id = started.snapshot.thread_id;
        let cancellation = CancellationToken::new();
        self.active
            .lock()
            .expect("active mutex poisoned")
            .insert(thread_id, cancellation.clone());
        let execution = self
            .engine
            .execute_started_thread_effect(&started, lease, &cancellation);
        tokio::pin!(execution);
        let heartbeat = tokio::time::sleep(self.heartbeat_interval());
        tokio::pin!(heartbeat);
        let execution = loop {
            tokio::select! {
                result = &mut execution => break result,
                () = &mut heartbeat => {
                    if self.engine.renew_lease(lease, now_ms(), self.authority_ttl()).is_err() {
                        cancellation.cancel();
                        let _ = execution.await;
                        self.active.lock().expect("active mutex poisoned").remove(&thread_id);
                        return Err(self.recover_lease_loss(&started.snapshot, lease, "started effect"));
                    }
                    heartbeat
                        .as_mut()
                        .reset(tokio::time::Instant::now() + self.heartbeat_interval());
                }
                () = cancellation.cancelled() => {
                    let _ = execution.await;
                    self.active.lock().expect("active mutex poisoned").remove(&thread_id);
                    return self.mark_cancelled_started_effect_unknown(&started, lease);
                }
            }
        };
        let cancelled = cancellation.is_cancelled();
        self.active
            .lock()
            .expect("active mutex poisoned")
            .remove(&thread_id);
        if cancelled {
            return self.mark_cancelled_started_effect_unknown(&started, lease);
        }
        match execution {
            Ok(mut value) => {
                let content = value.result.clone();
                let mut payload = value
                    .payload
                    .take()
                    .unwrap_or_else(|| serde_json::json!({}));
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "provider_content".into(),
                        serde_json::Value::String(content),
                    );
                    object.insert(
                        "tool_call_id".into(),
                        serde_json::Value::String(started.presentation.tool_call_id.clone()),
                    );
                    object.insert(
                        "name".into(),
                        serde_json::Value::String(started.presentation.name.clone()),
                    );
                }
                value.payload = Some(payload);
                self.engine
                    .observe_thread_effect(
                        &started,
                        format!(
                            "{}:effect:{}:observe",
                            started
                                .snapshot
                                .active_run_id
                                .ok_or(ThreadRuntimeError::InvalidState)?,
                            started.presentation.effect_id
                        ),
                        ThreadCommandId::from_uuid(Uuid::now_v7()),
                        value,
                        lease,
                        now_ms(),
                    )
                    .map(|observed| observed.snapshot)
                    .map_err(Into::into)
            }
            Err(ThreadEffectExecutionError::Certified(error)) => self
                .engine
                .observe_thread_effect(
                    &started,
                    format!(
                        "{}:effect:{}:observe-failed",
                        started
                            .snapshot
                            .active_run_id
                            .ok_or(ThreadRuntimeError::InvalidState)?,
                        started.presentation.effect_id
                    ),
                    ThreadCommandId::from_uuid(Uuid::now_v7()),
                    latte_engine::ThreadEffectObservedValue {
                        result: serde_json::json!({"error":error}).to_string(),
                        payload: Some(serde_json::json!({
                            "tool_call_id":started.presentation.tool_call_id,
                            "name":started.presentation.name,
                            "error":error,
                        })),
                        success: false,
                    },
                    lease,
                    now_ms(),
                )
                .map(|observed| observed.snapshot)
                .map_err(Into::into),
            Err(ThreadEffectExecutionError::Uncertain(_error)) => self
                .engine
                .mark_thread_effect_unknown(
                    &started,
                    format!(
                        "{}:effect:{}:unknown",
                        started
                            .snapshot
                            .active_run_id
                            .ok_or(ThreadRuntimeError::InvalidState)?,
                        started.presentation.effect_id
                    ),
                    ThreadCommandId::from_uuid(Uuid::now_v7()),
                    lease,
                    now_ms(),
                )
                .map_err(Into::into),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_provider_turn(
        &self,
        snapshot: ThreadSnapshot,
        messages: Vec<Message>,
        provider: Arc<dyn Provider>,
        lease: ThreadLeaseGuard,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let thread_id = snapshot.thread_id;
        let run_id = snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?;
        let run_revision = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .ok_or(ThreadRuntimeError::InvalidState)?
            .run_revision;
        let cancellation = CancellationToken::new();
        self.active
            .lock()
            .expect("active mutex poisoned")
            .insert(thread_id, cancellation.clone());
        let output = {
            let output = provider.complete(
                ProviderRequest {
                    messages: messages.clone(),
                    // Declarations are data only. The provider receives no
                    // capability: every returned call still crosses the
                    // engine-owned prepare/start/observe lifecycle below.
                    tools: self.engine.tool_descriptors(),
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_mins(1),
                    cancellation: cancellation.clone(),
                    events: self.progress.as_ref().map(|sink| {
                        Arc::new(ProviderProgress {
                            run_id,
                            sink: Arc::clone(sink),
                        }) as Arc<dyn ProviderEventSink>
                    }),
                },
            );
            tokio::pin!(output);
            let heartbeat = tokio::time::sleep(self.heartbeat_interval());
            tokio::pin!(heartbeat);
            loop {
                tokio::select! {
                    output = &mut output => break output,
                    () = &mut heartbeat => {
                        if self.engine.renew_lease(&lease, now_ms(), self.authority_ttl()).is_err() {
                            cancellation.cancel();
                            let _ = output.await;
                            self.active.lock().expect("active mutex poisoned").remove(&thread_id);
                            return Err(self.recover_lease_loss(&snapshot, &lease, "provider call"));
                        }
                        heartbeat
                            .as_mut()
                            .reset(tokio::time::Instant::now() + self.heartbeat_interval());
                    }
                    () = cancellation.cancelled() => {
                        // Do not drop an in-flight provider future and race it
                        // against a terminal write.  Providers receive the same
                        // token and must finish cancellation before we record a
                        // v2 interruption.
                        break output.await;
                    }
                }
            }
        };
        self.active
            .lock()
            .expect("active mutex poisoned")
            .remove(&thread_id);
        match output {
            Err(ProviderError::Cancelled) => self.commit(
                thread_id,
                run_id,
                snapshot.revision,
                run_revision,
                CommitThreadRunUpdate::Interrupt {
                    source_key: format!("{run_id}:provider-cancel"),
                    reconciliation_effect_id: None,
                },
                &lease,
            ),
            Err(error) => self.fail_retryable(
                thread_id,
                run_id,
                snapshot.revision,
                run_revision,
                format!("provider: {error}"),
                &lease,
            ),
            Ok(response) if response.input_request.is_some() => {
                let input = response.input_request.expect("checked is some");
                // The provider controls this value, but it becomes part of a
                // durable source key, request binding, and deduplication
                // identity. Do not redact then reuse an unsafe identifier:
                // redaction can collide and would still preserve a secret in
                // the durable command shape. It must be rejected before any
                // request/card/deduplication write.
                if input.secret
                    || !valid_openai_chat_input_request_id(&input.id)
                    || input.prompt.trim().is_empty()
                {
                    return self.fail(
                        thread_id,
                        run_id,
                        snapshot.revision,
                        run_revision,
                        "provider requested unsupported secret or invalid input".into(),
                        &lease,
                    );
                }
                self.commit(
                    thread_id,
                    run_id,
                    snapshot.revision,
                    run_revision,
                    CommitThreadRunUpdate::RequestInput {
                        source_key: format!("{run_id}:input-request:{}", input.id),
                        request: latte_core::PendingInput {
                            request_id: input.id,
                            prompt: input.prompt,
                        },
                    },
                    &lease,
                )
            }
            Ok(response) if !response.tool_calls.is_empty() => {
                self.handle_provider_tool_round(snapshot, messages, response, provider, lease)
                    .await
            }
            Ok(response) => {
                let Some(message) = response.message.filter(|value| !value.trim().is_empty())
                else {
                    return self.fail(
                        thread_id,
                        run_id,
                        snapshot.revision,
                        run_revision,
                        "provider returned an empty assistant outcome".into(),
                        &lease,
                    );
                };
                let appended = self.commit(
                    thread_id,
                    run_id,
                    snapshot.revision,
                    run_revision,
                    CommitThreadRunUpdate::AppendTranscript {
                        source_key: format!("{run_id}:assistant-final"),
                        kind: TranscriptKind::Assistant,
                        text: message.clone(),
                        payload: None,
                    },
                    &lease,
                )?;
                let changed = self.engine.thread_run_changed_files(run_id)?;
                if !changed.is_empty() {
                    if self.verification.is_none() {
                        return self.fail(
                            thread_id,
                            run_id,
                            appended.revision,
                            run_revision,
                            "workspace changed but no configured verification plan is available"
                                .into(),
                            &lease,
                        );
                    }
                    return self.begin_verification(appended, message, lease).await;
                }
                self.commit(
                    thread_id,
                    run_id,
                    appended.revision,
                    run_revision,
                    CommitThreadRunUpdate::Complete {
                        source_key: format!("{run_id}:complete"),
                        handoff: latte_core::Handoff {
                            summary: message,
                            files_changed: Vec::new(),
                            evidence: Vec::new(),
                        },
                    },
                    &lease,
                )
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn fail(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        thread_revision: u64,
        run_revision: u64,
        message: String,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        self.fail_with_retryability(
            thread_id,
            run_id,
            thread_revision,
            run_revision,
            &message,
            Retryability::Terminal,
            lease,
        )
    }

    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    fn fail_retryable(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        thread_revision: u64,
        run_revision: u64,
        message: String,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        self.fail_with_retryability(
            thread_id,
            run_id,
            thread_revision,
            run_revision,
            &message,
            Retryability::Retryable,
            lease,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_with_retryability(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        thread_revision: u64,
        run_revision: u64,
        message: &str,
        retryability: Retryability,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        self.commit(
            thread_id,
            run_id,
            thread_revision,
            run_revision,
            CommitThreadRunUpdate::Fail {
                source_key: format!("{run_id}:failure"),
                failure: RunFailure {
                    code: FailureCode::RuntimeFailed,
                    message: redact_thread_text(message),
                    retryability,
                },
            },
            lease,
        )
    }

    fn commit(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        expected_thread_revision: u64,
        expected_run_revision: u64,
        update: CommitThreadRunUpdate,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let command_id = ThreadCommandId::from_uuid(Uuid::now_v7());
        self.engine
            .commit_thread_run_update(
                ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision,
                    expected_run_revision,
                    command_id,
                    request_id: None,
                    effect_id: None,
                    update,
                },
                lease,
                now_ms(),
            )
            .map(|response| response.snapshot)
            .map_err(Into::into)
    }

    fn acquire(&self, thread_id: ThreadId) -> Result<ThreadLeaseGuard, ThreadRuntimeError> {
        self.engine
            .acquire_thread_lease(thread_id, now_ms(), self.authority_ttl())
            .map(|lease| ThreadLeaseGuard {
                engine: self.engine.clone(),
                lease,
            })
            .map_err(Into::into)
    }

    fn recover_lease_loss(
        &self,
        snapshot: &ThreadSnapshot,
        lease: &Lease,
        phase: &str,
    ) -> ThreadRuntimeError {
        let Some(run_id) = snapshot.active_run_id else {
            return ThreadRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; active linked run is unavailable"
            ));
        };
        let Some(revision) = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .map(|run| run.run_revision)
        else {
            return ThreadRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; active linked run revision is unavailable"
            ));
        };
        match self.engine.recover_thread_after_lease_loss(
            snapshot.thread_id,
            run_id,
            lease,
            revision,
            now_ms(),
        ) {
            Ok(ThreadLeaseLossRecovery::Recovered(_)) => ThreadRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; linked run requires reconciliation"
            )),
            Ok(ThreadLeaseLossRecovery::FencedNoop) => ThreadRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; newer owner fenced stale recovery"
            )),
            Ok(ThreadLeaseLossRecovery::AlreadyTerminal(_)) => ThreadRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; linked run already terminal"
            )),
            Err(error) => ThreadRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; recovery failed: {error}"
            )),
        }
    }

    fn mark_cancelled_started_effect_unknown(
        &self,
        started: &ThreadEffectStarted,
        lease: &Lease,
    ) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        self.engine
            .mark_thread_effect_unknown(
                started,
                format!(
                    "{}:effect:{}:cancelled-after-start",
                    started
                        .snapshot
                        .active_run_id
                        .ok_or(ThreadRuntimeError::InvalidState)?,
                    started.presentation.effect_id
                ),
                ThreadCommandId::from_uuid(Uuid::now_v7()),
                lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    const fn authority_ttl(&self) -> u64 {
        self.lease_ttl_ms
    }

    fn heartbeat_interval(&self) -> Duration {
        Duration::from_millis((self.lease_ttl_ms / 3).max(1))
    }

    fn load_full(&self, thread_id: ThreadId) -> Result<ThreadSnapshot, ThreadRuntimeError> {
        let mut snapshot = self.engine.thread_snapshot_v2(thread_id, None, 500)?;
        while snapshot.transcript.has_more {
            let after = snapshot.transcript.next_after;
            let next = self.engine.thread_snapshot_v2(thread_id, after, 500)?;
            if next.transcript.entries.is_empty() {
                break;
            }
            snapshot.transcript.entries.extend(next.transcript.entries);
            snapshot.transcript.next_after = next.transcript.next_after;
            snapshot.transcript.has_more = next.transcript.has_more;
        }
        Ok(snapshot)
    }
}

fn provider_configuration_failure_message() -> String {
    "The selected model could not be started. Check provider configuration and credentials, then retry in this conversation."
        .into()
}

struct ProviderProgress {
    run_id: RunId,
    sink: Arc<dyn ThreadProgressSink>,
}
impl ProviderEventSink for ProviderProgress {
    fn observe(&self, event: ProviderEvent) {
        match event {
            ProviderEvent::Attempt { number } => {
                self.sink.observe(ThreadTransientProgress::ProviderAttempt {
                    run_id: self.run_id,
                    number,
                });
            }
            ProviderEvent::AssistantDelta { text } => {
                self.sink.observe(ThreadTransientProgress::AssistantDelta {
                    run_id: self.run_id,
                    text: redact_thread_text(&text),
                });
            }
        }
    }
}

fn wire_bytes(messages: &[Message]) -> Result<usize, ThreadRuntimeError> {
    serde_json::to_vec(messages)
        .map(|bytes| bytes.len())
        .map_err(|error| ThreadRuntimeError::History(error.to_string()))
}

fn new_run_id() -> RunId {
    RunId::from_uuid(Uuid::now_v7())
}

fn active_run_revision(snapshot: &ThreadSnapshot) -> Result<u64, ThreadRuntimeError> {
    let run_id = snapshot
        .active_run_id
        .ok_or(ThreadRuntimeError::InvalidState)?;
    snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .map(|run| run.run_revision)
        .ok_or(ThreadRuntimeError::InvalidState)
}

fn thread_effect_request(
    snapshot: &ThreadSnapshot,
    descriptor: ThreadEffectDescriptor,
    source_key: String,
) -> Result<ThreadEffectRequest, ThreadRuntimeError> {
    Ok(ThreadEffectRequest {
        thread_id: snapshot.thread_id,
        run_id: snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?,
        expected_thread_revision: snapshot.revision,
        expected_run_revision: active_run_revision(snapshot)?,
        command_id: ThreadCommandId::from_uuid(Uuid::now_v7()),
        source_key,
        descriptor,
    })
}

fn thread_effect_start_request(
    snapshot: &ThreadSnapshot,
    effect_id: String,
    source_key: String,
) -> Result<ThreadEffectStartRequest, ThreadRuntimeError> {
    Ok(ThreadEffectStartRequest {
        thread_id: snapshot.thread_id,
        run_id: snapshot
            .active_run_id
            .ok_or(ThreadRuntimeError::InvalidState)?,
        expected_thread_revision: snapshot.revision,
        expected_run_revision: active_run_revision(snapshot)?,
        command_id: ThreadCommandId::from_uuid(Uuid::now_v7()),
        source_key,
        effect_id,
    })
}

/// Finds the exact persisted provider round containing an approved descriptor.
///
/// Assistant `tool_calls` are provider grammar, not a best-effort display
/// summary. Keeping them in the transcript gives a restarted coordinator the
/// same ordered queue it had before the permission pause.
fn tool_round_for_call(
    snapshot: &ThreadSnapshot,
    tool_call_id: &str,
) -> Result<(u64, Vec<crate::provider::ToolCall>, usize), ThreadRuntimeError> {
    snapshot
        .transcript
        .entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == TranscriptKind::Assistant)
        .find_map(|entry| {
            let calls = entry
                .payload
                .as_ref()?
                .get("tool_calls")
                .cloned()
                .and_then(|value| {
                    serde_json::from_value::<Vec<crate::provider::ToolCall>>(value).ok()
                })?;
            calls
                .iter()
                .position(|call| call.id == tool_call_id)
                .map(|ordinal| (entry.sequence, calls, ordinal))
        })
        .ok_or_else(|| {
            ThreadRuntimeError::Effect(
                "prepared tool call has no durable provider round continuation".into(),
            )
        })
}

fn effect_provider_result(snapshot: &ThreadSnapshot, tool_call_id: &str) -> Option<String> {
    for entry in snapshot.transcript.entries.iter().rev() {
        if entry.kind != TranscriptKind::ToolResult {
            continue;
        }
        let Some(payload) = entry.payload.as_ref() else {
            continue;
        };
        if payload
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)
            == Some(tool_call_id)
        {
            return payload
                .get("provider_content")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FakeProvider, InputRequest, ProviderResponse};
    use latte_engine::EngineBuilder;

    struct DelayedProvider {
        responses: Mutex<std::collections::VecDeque<(Duration, ProviderResponse)>>,
    }

    impl DelayedProvider {
        fn scripted(values: impl IntoIterator<Item = (Duration, ProviderResponse)>) -> Self {
            Self {
                responses: Mutex::new(values.into_iter().collect()),
            }
        }
    }

    impl Provider for DelayedProvider {
        fn complete(
            &self,
            _: ProviderRequest,
            context: ProviderContext,
        ) -> crate::provider::ProviderFuture<'_> {
            let result = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                let Some((delay, response)) = result else {
                    return Err(ProviderError::Malformed(
                        "delayed provider exhausted".into(),
                    ));
                };
                tokio::select! {
                    () = tokio::time::sleep(delay) => Ok(response),
                    () = context.cancellation.cancelled() => Err(ProviderError::Cancelled),
                }
            })
        }
    }

    struct RecordingProvider {
        responses: Mutex<std::collections::VecDeque<ProviderResponse>>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl RecordingProvider {
        fn scripted(values: impl IntoIterator<Item = ProviderResponse>) -> Self {
            Self {
                responses: Mutex::new(values.into_iter().collect()),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Provider for RecordingProvider {
        fn complete(
            &self,
            request: ProviderRequest,
            _: ProviderContext,
        ) -> crate::provider::ProviderFuture<'_> {
            self.requests.lock().unwrap().push(request.messages);
            let response = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                response
                    .ok_or_else(|| ProviderError::Malformed("recording provider exhausted".into()))
            })
        }
    }

    fn binding() -> ThreadProviderBindingV2 {
        ThreadProviderBindingV2 {
            version: 1,
            provider_name: "p".into(),
            provider_type: "test".into(),
            protocol: "test".into(),
            model: "m".into(),
            config_fingerprint: "c".into(),
            tools_fingerprint: "t".into(),
            aliases: std::collections::BTreeMap::default(),
            credential_ref_id: "ref".into(),
            data_scope_id: "scope".into(),
            credential_generation: 1,
        }
    }

    #[tokio::test]
    async fn start_provider_configuration_failure_is_durable_and_retryable() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let factory: ThreadProviderFactory =
            Arc::new(|_| Err("missing environment variable PROVIDER_SECRET_NAME".into()));
        let service = ThreadRuntimeService::new(
            engine.clone(),
            root.path(),
            ThreadHistoryPolicy::default(),
            factory,
        );
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());

        let failed = service
            .start(thread_id, "durable prompt".into(), binding())
            .await
            .unwrap();

        assert_eq!(failed.lifecycle, ThreadLifecycle::Ready);
        assert!(failed.active_run_id.is_none());
        assert_eq!(failed.runs.len(), 1);
        assert_eq!(failed.runs[0].status, latte_core::ThreadRunStatus::Failed);
        let expected_failure = provider_configuration_failure_message();
        assert_eq!(
            failed
                .transcript
                .entries
                .iter()
                .map(|entry| (entry.kind, entry.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (TranscriptKind::User, "durable prompt"),
                (TranscriptKind::Failure, expected_failure.as_str())
            ]
        );
        assert_eq!(
            engine.show(failed.runs[0].run_id).unwrap().failure,
            Some(RunFailure {
                code: FailureCode::RuntimeFailed,
                message: expected_failure,
                retryability: Retryability::Retryable,
            })
        );
        assert_eq!(engine.list_threads_v2().unwrap(), [failed]);
    }

    #[tokio::test]
    async fn follow_up_provider_configuration_failure_records_child_and_allows_retry() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let provider = Arc::new(FakeProvider::scripted([
            ProviderResponse {
                message: Some("first complete".into()),
                tool_calls: vec![],
                input_request: None,
                usage: crate::provider::ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            ProviderResponse {
                message: Some("retry complete".into()),
                tool_calls: vec![],
                input_request: None,
                usage: crate::provider::ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
        ]));
        let factory: ThreadProviderFactory = Arc::new(move |_| {
            if factory_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Err("secret reference PROVIDER_SECRET_NAME is unavailable".into())
            } else {
                Ok(ResolvedProvider {
                    provider: provider.clone(),
                    binding: crate::registry::ProviderBinding::direct(&[]),
                })
            }
        });
        let service = ThreadRuntimeService::new(
            engine.clone(),
            root.path(),
            ThreadHistoryPolicy::default(),
            factory,
        );
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let complete = service
            .start(thread_id, "first".into(), binding())
            .await
            .unwrap();

        let failed = service
            .follow_up(thread_id, complete.revision, "durable follow-up".into())
            .await
            .unwrap();

        assert_eq!(failed.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(failed.runs.len(), 2);
        assert_eq!(
            failed.runs[0].status,
            latte_core::ThreadRunStatus::Completed
        );
        assert_eq!(failed.runs[1].status, latte_core::ThreadRunStatus::Failed);
        assert!(failed.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::User && entry.text == "durable follow-up"
        }));
        assert!(failed.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::Failure
                && entry.text == provider_configuration_failure_message()
        }));

        let retried = service
            .follow_up(thread_id, failed.revision, "retry after config fix".into())
            .await
            .unwrap();
        assert_eq!(retried.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(retried.runs.len(), 3);
        assert_eq!(
            retried.runs[2].status,
            latte_core::ThreadRunStatus::Completed
        );
        assert!(retried.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::User && entry.text == "retry after config fix"
        }));
        assert!(retried.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::Assistant && entry.text == "retry complete"
        }));
    }

    #[tokio::test]
    async fn child_history_is_bounded_and_parent_stays_completed() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let factory: ThreadProviderFactory = Arc::new(|_| {
            Ok(ResolvedProvider {
                provider: Arc::new(FakeProvider::scripted([
                    ProviderResponse {
                        message: Some("first".into()),
                        tool_calls: vec![],
                        input_request: None,
                        usage: crate::provider::ProviderUsage::default(),
                        finish_reason: None,
                        provider_state: None,
                    },
                    ProviderResponse {
                        message: Some("second".into()),
                        tool_calls: vec![],
                        input_request: None,
                        usage: crate::provider::ProviderUsage::default(),
                        finish_reason: None,
                        provider_state: None,
                    },
                ])),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = ThreadRuntimeService::new(
            engine.clone(),
            root.path(),
            ThreadHistoryPolicy::default(),
            factory,
        );
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let complete = service
            .start(thread_id, "one".into(), binding())
            .await
            .unwrap();
        let parent = complete.latest_run_id.unwrap();
        let child = service
            .follow_up(thread_id, complete.revision, "two".into())
            .await
            .unwrap();
        assert_eq!(child.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(
            engine.show(parent).unwrap().status,
            latte_core::RunStatus::Completed
        );
        assert_eq!(child.runs.len(), 2);
    }

    fn response(
        message: Option<&str>,
        tool_calls: Vec<crate::provider::ToolCall>,
    ) -> ProviderResponse {
        ProviderResponse {
            message: message.map(str::to_owned),
            tool_calls,
            input_request: None,
            usage: crate::provider::ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        }
    }

    fn scripted_service(
        root: &std::path::Path,
        engine: EngineHandle,
        responses: Vec<ProviderResponse>,
    ) -> ThreadRuntimeService {
        let provider = Arc::new(FakeProvider::scripted(responses));
        let factory: ThreadProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        ThreadRuntimeService::new(engine, root, ThreadHistoryPolicy::default(), factory)
    }

    fn delayed_service(
        root: &std::path::Path,
        engine: EngineHandle,
        responses: impl IntoIterator<Item = (Duration, ProviderResponse)>,
    ) -> ThreadRuntimeService {
        let provider = Arc::new(DelayedProvider::scripted(responses));
        let factory: ThreadProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        ThreadRuntimeService::new(engine, root, ThreadHistoryPolicy::default(), factory)
    }

    fn recording_service(
        root: &std::path::Path,
        engine: EngineHandle,
        provider: Arc<RecordingProvider>,
    ) -> ThreadRuntimeService {
        let factory: ThreadProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        ThreadRuntimeService::new(engine, root, ThreadHistoryPolicy::default(), factory)
    }

    #[tokio::test]
    async fn ready_session_model_switch_is_validated_persisted_and_revision_guarded() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![response(Some("complete"), vec![])],
        );
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(thread_id, "initial".into(), binding())
            .await
            .unwrap();

        assert_eq!(
            service
                .switch_model(thread_id, ready.revision, &ready.binding)
                .unwrap(),
            ready
        );
        let mut invalid = binding();
        invalid.provider_name.clear();
        assert!(matches!(
            service.switch_model(thread_id, ready.revision, &invalid),
            Err(ThreadRuntimeError::ProviderConfiguration(_))
        ));
        let mut next = binding();
        next.provider_name = "other".into();
        next.model = "reasoning".into();
        next.config_fingerprint = "other-config".into();
        assert!(matches!(
            service.switch_model(thread_id, ready.revision + 1, &next),
            Err(ThreadRuntimeError::InvalidState)
        ));

        let switched = service
            .switch_model(thread_id, ready.revision, &next)
            .unwrap();
        assert_eq!(switched.binding, next);
        assert!(switched.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::System
                && entry.text == "Model switched to other/reasoning"
        }));
    }

    #[cfg(unix)]
    fn passing_verification() -> VerificationPlan {
        VerificationPlan {
            // `process::classify` recognizes this argv-only probe as an
            // engine-allowed verification command, so mutation tests exercise
            // observed evidence/completion rather than a second approval UI.
            argv: vec!["/bin/pwd".into()],
            cwd: ".".into(),
            timeout_ms: 5_000,
            grace_ms: 25,
            stdout_cap: 4 * 1024,
            stderr_cap: 4 * 1024,
        }
    }

    #[cfg(unix)]
    fn failing_verification() -> VerificationPlan {
        VerificationPlan {
            // This exact argv shape is engine-allowed but exits one for the
            // write fixture, giving a certified failed verification result.
            argv: vec![
                "/usr/bin/grep".into(),
                "-q".into(),
                "not-present".into(),
                "new.txt".into(),
            ],
            ..passing_verification()
        }
    }

    /// Test-only fault injection at the durable lease boundary.  Deleting the
    /// row is the smallest deterministic representation of another authority
    /// having fenced this coordinator: the next renewal must fail and the
    /// recovery transaction must not depend on a process restart.
    fn force_lease_renewal_failure(database: &std::path::Path) {
        let changed = rusqlite::Connection::open(database)
            .unwrap()
            .execute("DELETE FROM runtime_lease", [])
            .unwrap();
        assert_eq!(changed, 1, "the active coordinator lease must exist");
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool, description: &str) {
        for _ in 0..100 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {description}");
    }

    #[tokio::test]
    async fn v2_allowed_read_is_started_observed_and_reenters_provider() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "hello").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("reading"),
                    vec![crate::provider::ToolCall {
                        id: "read-note".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path":"note.txt"}),
                    }],
                ),
                response(Some("done"), vec![]),
            ],
        );
        let snapshot = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "read it".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, ThreadLifecycle::Ready);
        assert!(
            snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::ToolResult)
        );
        let effect_id = snapshot
            .transcript
            .entries
            .iter()
            .find_map(|entry| {
                entry
                    .payload
                    .as_ref()?
                    .get("descriptor")?
                    .get("effect_id")?
                    .as_str()
            })
            .unwrap();
        assert_eq!(
            engine.effect_status(effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedSuccess
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_uncertain_process_launch_requires_reconciliation_without_tool_result() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("attempting failed process"),
                    vec![crate::provider::ToolCall {
                        id: "failed-process".into(),
                        name: "process".into(),
                        input: serde_json::json!({"argv":["/definitely-missing-latte-command"]}),
                    }],
                ),
                response(Some("must not be reached"), vec![]),
            ],
        );
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "run a failed process".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let terminal = service
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(terminal.lifecycle, ThreadLifecycle::ReconciliationRequired);
        let effect_id = terminal
            .transcript
            .entries
            .iter()
            .find_map(|entry| {
                entry
                    .payload
                    .as_ref()?
                    .get("descriptor")?
                    .get("effect_id")?
                    .as_str()
            })
            .unwrap();
        assert_eq!(
            engine.effect_status(effect_id).unwrap(),
            latte_engine::EffectStatus::Unknown
        );
        assert!(
            !terminal
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::ToolResult)
        );
        assert_eq!(
            service
                .reconcile_unknown_effect(terminal.thread_id, effect_id)
                .unwrap()
                .lifecycle,
            ThreadLifecycle::Failed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_asked_write_waits_then_consumes_exact_permission() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("creating"),
                    vec![crate::provider::ToolCall {
                        id: "create-note".into(),
                        name: "write_file".into(),
                        input: serde_json::json!({
                            "path":"created.txt",
                            // `token=value` is ordinary source text here,
                            // but the transcript redactor treats the shape
                            // conservatively. Approval must still execute
                            // the exact engine-private descriptor rather than
                            // the display projection.
                            "content":"const token=value;\n",
                            "create_intent":true
                        }),
                    }],
                ),
                response(Some("completed"), vec![]),
            ],
        )
        .with_verification(passing_verification());
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "create it".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let done = service
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(done.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(
            std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
            "const token=value;\n"
        );
        assert!(
            done.runs
                .iter()
                .any(|run| run.status == latte_core::ThreadRunStatus::Completed)
        );
        let run_id = done.latest_run_id.unwrap();
        let handoff = engine.show(run_id).unwrap().handoff.unwrap();
        assert_eq!(handoff.evidence.len(), 1);
        assert_eq!(
            handoff.evidence[0].status,
            latte_core::VerificationStatus::Passed
        );
        let projected_handoff = done
            .transcript
            .entries
            .iter()
            .rev()
            .find(|entry| entry.kind == TranscriptKind::Completion)
            .and_then(|entry| entry.payload.as_ref())
            .and_then(|payload| payload.get("handoff"))
            .expect("completed thread snapshot projects the redacted handoff");
        assert_eq!(
            projected_handoff["files_changed"],
            serde_json::json!(["created.txt"])
        );
        assert_eq!(projected_handoff["evidence"][0]["status"], "passed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_private_descriptor_executes_approved_code_without_transcript_or_history_secret_egress()
     {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&database)
            .build()
            .unwrap();
        let source = "const token=value;\nconst api_key=live-secret-value;\n";
        let provider = Arc::new(RecordingProvider::scripted([
            response(
                Some("create secret-shaped source"),
                vec![write_call("write-source", "generated.rs", source)],
            ),
            response(
                Some("read it back"),
                vec![read_call("read-source", "generated.rs")],
            ),
            response(Some("done"), vec![]),
        ]));
        let service = recording_service(root.path(), engine.clone(), provider.clone())
            .with_verification(passing_verification());
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "write and inspect source".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
        let waiting_json = serde_json::to_string(&waiting).unwrap();
        assert!(!waiting_json.contains("live-secret-value"));
        assert!(!waiting_json.contains("token=value"));
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let completed = service
            .resolve_permission(
                waiting.thread_id,
                waiting.revision,
                request_id.clone(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(
            std::fs::read_to_string(root.path().join("generated.rs")).unwrap(),
            source
        );
        let effect_id = completed
            .transcript
            .entries
            .iter()
            .find_map(|entry| {
                entry
                    .payload
                    .as_ref()?
                    .get("descriptor")?
                    .get("effect_id")?
                    .as_str()
            })
            .unwrap();
        assert_eq!(
            engine.effect_status(effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedSuccess
        );
        let completed_json = serde_json::to_string(&completed).unwrap();
        assert!(!completed_json.contains("live-secret-value"));
        assert!(!completed_json.contains("token=value"));
        let requests = provider.requests.lock().unwrap();
        let history = serde_json::to_string(&*requests).unwrap();
        assert!(!history.contains("live-secret-value"));
        assert!(!history.contains("token=value"));
        drop(requests);

        let connection = rusqlite::Connection::open(&database).unwrap();
        for table_and_column in [
            ("effects", "descriptor_json"),
            ("thread_transcript_v2", "entry_json"),
            ("runtime_checkpoints", "payload_json"),
            ("thread_command_dedup_v2", "result_json"),
        ] {
            let (table, column) = table_and_column;
            let query = format!("SELECT COALESCE(group_concat({column}, '\\n'), '') FROM {table}");
            let durable: String = connection.query_row(&query, [], |row| row.get(0)).unwrap();
            assert!(!durable.contains("live-secret-value"), "{table}.{column}");
            assert!(!durable.contains("token=value"), "{table}.{column}");
        }
        let canonical: String = connection
            .query_row(
                "SELECT descriptor_json FROM thread_effect_canonical_v2 WHERE effect_id=?1",
                [effect_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(canonical.contains("live-secret-value"));
        assert!(canonical.contains("token=value"));
    }

    fn write_call(id: &str, path: &str, content: &str) -> crate::provider::ToolCall {
        crate::provider::ToolCall {
            id: id.into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": path,
                "content": content,
                "create_intent": true,
            }),
        }
    }

    fn read_call(id: &str, path: &str) -> crate::provider::ToolCall {
        crate::provider::ToolCall {
            id: id.into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": path}),
        }
    }

    fn tool_result_ids(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_ask_first_tool_round_continues_all_calls_in_provider_order() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("existing.txt"), "before").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(
                Some("write then read"),
                vec![
                    write_call("call_ask-write", "new.txt", "created"),
                    read_call("call_allowed-read", "new.txt"),
                ],
            ),
            response(Some("done after both"), vec![]),
        ]));
        let service = recording_service(root.path(), engine, provider.clone())
            .with_verification(passing_verification());
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        assert!(request_id.contains("call_ask-write"));
        assert!(!request_id.contains("[REDACTED]"));
        let completed = service
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.txt")).unwrap(),
            "created"
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "no provider retry between queued calls");
        assert!(requests[1]
            .iter()
            .any(|message| matches!(message, Message::Assistant { tool_calls, .. } if tool_calls.len() == 2)));
        assert_eq!(
            tool_result_ids(&requests[1]),
            ["call_ask-write", "call_allowed-read"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_allowed_first_tool_round_waits_then_replays_remaining_call_in_order() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source.txt"), "source").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(
                Some("read then write"),
                vec![
                    read_call("allowed-read", "source.txt"),
                    write_call("ask-write", "new.txt", "created"),
                ],
            ),
            response(Some("done after both"), vec![]),
        ]));
        let service = recording_service(root.path(), engine, provider.clone())
            .with_verification(passing_verification());
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let completed = service
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(
            std::fs::read_to_string(root.path().join("new.txt")).unwrap(),
            "created"
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_result_ids(&requests[1]), ["allowed-read", "ask-write"]);
    }

    #[tokio::test]
    async fn v2_tool_round_denial_preserves_remaining_queue_without_provider_reentry() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(
                Some("write then read"),
                vec![
                    write_call("ask-write", "new.txt", "created"),
                    read_call("allowed-read", "new.txt"),
                ],
            ),
            response(Some("continued without denied tools"), vec![]),
        ]));
        let service = recording_service(root.path(), engine, provider.clone());
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let denied = service
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, false)
            .await
            .unwrap();
        assert_eq!(denied.lifecycle, ThreadLifecycle::Ready);
        assert!(denied.active_run_id.is_none());
        assert!(denied.pending.is_none());
        assert!(!root.path().join("new.txt").exists());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        let continued = service
            .follow_up(
                denied.thread_id,
                denied.revision,
                "continue without those tools".into(),
            )
            .await
            .unwrap();
        assert_eq!(continued.lifecycle, ThreadLifecycle::Ready);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_result_ids(&requests[1]), ["ask-write", "allowed-read"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_tool_round_resume_after_restart_uses_durable_assistant_queue() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&database)
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(
                Some("write then read"),
                vec![
                    write_call("call_ask-write", "new.txt", "created"),
                    read_call("call_allowed-read", "new.txt"),
                ],
            ),
            response(Some("done after both"), vec![]),
        ]));
        let first = recording_service(root.path(), engine.clone(), provider.clone())
            .with_verification(passing_verification());
        let waiting = first
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        assert!(request_id.contains("call_ask-write"));
        assert!(!request_id.contains("[REDACTED]"));
        drop(first);
        let resumed = recording_service(root.path(), engine, provider.clone())
            .with_verification(passing_verification())
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(resumed.lifecycle, ThreadLifecycle::Ready);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            tool_result_ids(&requests[1]),
            ["call_ask-write", "call_allowed-read"]
        );
    }

    #[tokio::test]
    async fn v2_mutation_requires_configured_passing_verification_but_read_only_does_not() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "read only").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let read_only = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("read complete"),
                    vec![read_call("read-note", "note.txt")],
                ),
                response(Some("done"), vec![]),
            ],
        );
        let complete = read_only
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "read".into(),
                binding(),
            )
            .await
            .unwrap();
        let handoff = engine
            .show(complete.latest_run_id.unwrap())
            .unwrap()
            .handoff
            .unwrap();
        assert!(
            handoff.evidence.is_empty(),
            "read-only child does not run verification"
        );

        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let mutated = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("write"),
                    vec![write_call("ask-write", "new.txt", "created")],
                ),
                response(Some("done"), vec![]),
            ],
        );
        let waiting = mutated
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "write".into(),
                binding(),
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let failed = mutated
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, ThreadLifecycle::Failed);
        assert!(
            engine
                .show(failed.latest_run_id.unwrap())
                .unwrap()
                .handoff
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_failed_verification_blocks_completion_after_mutation() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("write"),
                    vec![write_call("ask-write", "new.txt", "created")],
                ),
                response(Some("done"), vec![]),
            ],
        )
        .with_verification(failing_verification());
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "write".into(),
                binding(),
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let failed = service
            .resolve_permission(waiting.thread_id, waiting.revision, request_id, true)
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, ThreadLifecycle::Failed);
        let run = engine.show(failed.latest_run_id.unwrap()).unwrap();
        assert_eq!(run.status, latte_core::RunStatus::Failed);
        assert!(run.handoff.is_none());
        assert!(
            failed
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::ToolResult
                    && entry.text.contains("exit_code"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_verification_permission_is_durable_and_completion_waits_for_approval() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let verification = VerificationPlan {
            argv: vec!["/bin/echo".into(), "verified".into()],
            ..passing_verification()
        };
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("write"),
                    vec![write_call("ask-write", "new.txt", "created")],
                ),
                response(Some("done"), vec![]),
            ],
        )
        .with_verification(verification);
        let waiting_tool = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "write".into(),
                binding(),
            )
            .await
            .unwrap();
        let tool_request = match waiting_tool.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let waiting_verification = service
            .resolve_permission(
                waiting_tool.thread_id,
                waiting_tool.revision,
                tool_request,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            waiting_verification.lifecycle,
            ThreadLifecycle::WaitingPermission
        );
        let verification_request = match waiting_verification.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => {
                panic!("expected verification permission")
            }
        };
        let complete = service
            .resolve_permission(
                waiting_verification.thread_id,
                waiting_verification.revision,
                verification_request,
                true,
            )
            .await
            .unwrap();
        assert_eq!(complete.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(
            engine
                .show(complete.latest_run_id.unwrap())
                .unwrap()
                .handoff
                .unwrap()
                .evidence[0]
                .status,
            latte_core::VerificationStatus::Passed
        );
    }

    #[tokio::test]
    async fn v2_input_request_resumes_same_child_with_nonsecret_history() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                ProviderResponse {
                    message: None,
                    tool_calls: vec![],
                    input_request: Some(InputRequest {
                        id: "language".into(),
                        prompt: "Which language?".into(),
                        secret: false,
                    }),
                    usage: crate::provider::ProviderUsage::default(),
                    finish_reason: None,
                    provider_state: None,
                },
                response(Some("Rust selected"), vec![]),
            ],
        );
        let waiting = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "choose a language".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingInput);
        let completed = service
            .provide_input(
                waiting.thread_id,
                waiting.revision,
                "language".into(),
                "Rust".into(),
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, ThreadLifecycle::Ready);
        assert!(
            completed
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::Input)
        );
        assert!(matches!(
            service
                .provide_input(
                    waiting.thread_id,
                    waiting.revision,
                    "language".into(),
                    "again".into()
                )
                .await,
            Err(ThreadRuntimeError::InvalidState)
        ));
        assert_eq!(
            engine
                .thread_snapshot_v2(completed.thread_id, None, 100)
                .unwrap()
                .lifecycle,
            ThreadLifecycle::Ready
        );
    }

    #[tokio::test]
    #[rustfmt::skip]
    async fn v2_invalid_provider_outcomes_fail_closed_without_effect_execution() {
        let invalid = [
            ProviderResponse {
                message: None,
                tool_calls: vec![],
                input_request: Some(InputRequest {
                    id: "secret".into(),
                    prompt: "secret please".into(),
                    secret: true,
                }),
                usage: crate::provider::ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            response(Some(""), vec![]),
            response(
                Some("bad tool"),
                vec![crate::provider::ToolCall {
                    id: "bad\nidentifier".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path":"missing"}),
                }],
            ),
        ];
        for outcome in invalid {
            let root = tempfile::tempdir().unwrap();
            let engine = EngineBuilder::new()
                .workspace_root(root.path())
                .build()
                .unwrap();
            let service = scripted_service(root.path(), engine, vec![outcome]);
            let failed = service
                .start(
                    ThreadId::from_uuid(Uuid::now_v7()),
                    "must fail closed".into(),
                    binding(),
                )
                .await
                .unwrap();
            assert_eq!(failed.lifecycle, ThreadLifecycle::Failed);
            assert!(failed.active_run_id.is_none());
            assert!(
                !failed
                    .transcript
                    .entries
                    .iter()
                    .any(|entry| entry.kind == TranscriptKind::ToolResult)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new().workspace_root(root.path()).build().unwrap();
        let service = recording_service(root.path(), engine, Arc::new(RecordingProvider::scripted([]))).with_progress_sink(Arc::new(|_| {}));
        let failed = service.start(ThreadId::from_uuid(Uuid::now_v7()), "provider error".into(), binding()).await.unwrap();
        assert_eq!(failed.lifecycle, ThreadLifecycle::Ready); assert!(failed.active_run_id.is_none()); assert!(failed.transcript.entries.iter().any(|entry| entry.kind == TranscriptKind::Failure)); let mut invalid_binding = binding(); invalid_binding.provider_name.clear(); assert!(matches!(service.start(ThreadId::from_uuid(Uuid::now_v7()), "invalid binding".into(), invalid_binding).await, Err(ThreadRuntimeError::ProviderConfiguration(_)))); let missing = ThreadId::from_uuid(Uuid::now_v7()); assert!(service.follow_up(missing, 0, "missing".into()).await.is_err()); assert!(service.provide_input(missing, 0, "missing".into(), "value".into()).await.is_err()); assert!(service.resolve_permission(missing, 0, "missing".into(), false).await.is_err()); assert!(service.reconcile_unknown_effect(missing, "missing").is_err()); assert!(service.cancel_durable(missing).is_err());

        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new().workspace_root(root.path()).build().unwrap();
        let service = delayed_service(
            root.path(),
            engine,
            [(Duration::from_mins(1), response(Some("unused"), vec![]))],
        );
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let cancel = async { wait_until(|| service.active.lock().unwrap().contains_key(&thread_id), "provider registration").await; service.cancel(thread_id); };
        let (cancelled, ()) = tokio::join!(service.start(thread_id, "cancel provider".into(), binding()), cancel);
        assert_eq!(cancelled.unwrap().lifecycle, ThreadLifecycle::Interrupted);
    }

    #[tokio::test]
    async fn v2_rejects_secret_shaped_provider_tool_id_before_any_durable_tool_state() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&database)
            .build()
            .unwrap();
        let unsafe_id = "token=provider-secret-value";
        let service = scripted_service(
            root.path(),
            engine,
            vec![response(
                Some("this assistant envelope must not persist"),
                vec![crate::provider::ToolCall {
                    id: unsafe_id.into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "must-not-exist.txt",
                        "content": "not executed",
                        "create_intent": true,
                    }),
                }],
            )],
        );

        let failed = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "reject unsafe provider identity".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, ThreadLifecycle::Failed);
        assert!(failed.active_run_id.is_none());
        assert!(!root.path().join("must-not-exist.txt").exists());
        assert!(failed.pending.is_none());
        let failure = failed
            .transcript
            .entries
            .iter()
            .find(|entry| entry.kind == TranscriptKind::Failure)
            .expect("unsafe provider ID must produce a visible terminal failure card");
        assert!(failure.text.contains("tool call ids must match"));
        assert!(!failure.text.contains(unsafe_id));
        assert!(!failed.transcript.entries.iter().any(|entry| {
            matches!(
                entry.kind,
                TranscriptKind::Assistant
                    | TranscriptKind::ToolCall
                    | TranscriptKind::Permission
                    | TranscriptKind::ToolResult
            )
        }));

        let connection = rusqlite::Connection::open(&database).unwrap();
        for (table, column) in [
            ("effects", "effect_id"),
            ("effects", "descriptor_json"),
            ("thread_effect_canonical_v2", "descriptor_json"),
            ("pending_permissions", "effect_id"),
            ("thread_transcript_v2", "entry_json"),
            ("thread_events_v2", "event_json"),
            ("runtime_checkpoints", "payload_json"),
            ("thread_command_dedup_v2", "result_json"),
        ] {
            let query = format!("SELECT COALESCE(group_concat({column}, '\\n'), '') FROM {table}");
            let durable: String = connection.query_row(&query, [], |row| row.get(0)).unwrap();
            assert!(!durable.contains(unsafe_id), "{table}.{column}");
            assert!(
                !durable.contains("[REDACTED]"),
                "{table}.{column} must not contain a corrupted provider ID"
            );
        }
    }

    #[tokio::test]
    async fn v2_rejects_secret_shaped_input_id_before_any_durable_request_state() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("thread.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&database)
            .build()
            .unwrap();
        let unsafe_id = "token=provider-secret-value";
        let service = scripted_service(
            root.path(),
            engine,
            vec![ProviderResponse {
                message: None,
                tool_calls: vec![],
                input_request: Some(InputRequest {
                    id: unsafe_id.into(),
                    prompt: "What should I do next?".into(),
                    secret: false,
                }),
                usage: crate::provider::ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            }],
        );

        let failed = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "reject unsafe input identity".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, ThreadLifecycle::Failed);
        assert!(failed.pending.is_none());
        let failure = failed
            .transcript
            .entries
            .iter()
            .find(|entry| entry.kind == TranscriptKind::Failure)
            .expect("unsafe input ID must produce a visible terminal failure card");
        assert!(failure.text.contains("unsupported secret or invalid input"));
        assert!(!failure.text.contains(unsafe_id));
        assert!(
            !failed
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::Input)
        );

        let connection = rusqlite::Connection::open(&database).unwrap();
        let input_request_sources: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM thread_commit_sources_v2 WHERE source_key LIKE '%:input-request:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            input_request_sources, 0,
            "unsafe input must not create a source key"
        );
        let pending_inputs: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE json_extract(state_json, '$.pending_input') IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending_inputs, 0,
            "unsafe input must not create durable pending state"
        );
        for (table, column) in [
            ("thread_transcript_v2", "entry_json"),
            ("thread_events_v2", "event_json"),
            ("thread_command_dedup_v2", "digest"),
            ("thread_command_dedup_v2", "result_json"),
            ("thread_commit_sources_v2", "source_key"),
            ("thread_commit_sources_v2", "digest"),
            ("thread_commit_sources_v2", "result_json"),
        ] {
            let query = format!("SELECT COALESCE(group_concat({column}, '\\n'), '') FROM {table}");
            let durable: String = connection.query_row(&query, [], |row| row.get(0)).unwrap();
            assert!(!durable.contains(unsafe_id), "{table}.{column}");
            assert!(
                !durable.contains("token=[REDACTED]"),
                "{table}.{column} must not contain a transformed provider ID"
            );
        }
    }

    #[test]
    fn history_policy_progress_and_request_helpers_are_bounded_and_typed() {
        assert!(
            ThreadHistoryPolicy {
                max_request_bytes: 0,
                ..ThreadHistoryPolicy::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ThreadHistoryPolicy {
                max_request_bytes: 10,
                max_input_bytes: 10,
                reserved_output_bytes: 10,
                context_cap_bytes: 1,
            }
            .validate()
            .is_err()
        );

        let received = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<dyn ThreadProgressSink> = {
            let received = Arc::clone(&received);
            Arc::new(move |progress| received.lock().unwrap().push(progress))
        };
        let run_id = RunId::from_uuid(Uuid::now_v7());
        let progress = ProviderProgress { run_id, sink };
        progress.observe(ProviderEvent::Attempt { number: 2 });
        progress.observe(ProviderEvent::AssistantDelta {
            text: "ok\u{1b}[31m sk-hidden".into(),
        });
        assert!(matches!(
            received.lock().unwrap().as_slice(),
            [
                ThreadTransientProgress::ProviderAttempt { run_id: observed, number: 2 },
                ThreadTransientProgress::AssistantDelta { text, .. },
            ] if *observed == run_id && !text.contains("sk-hidden")
        ));
        assert_eq!(
            ThreadRuntimeService::new(
                EngineBuilder::new().build().unwrap(),
                std::env::current_dir().unwrap(),
                ThreadHistoryPolicy::default(),
                Arc::new(|_| Err("not used".into())),
            )
            .with_lease_ttl_ms(1)
            .heartbeat_interval(),
            Duration::from_millis(10 / 3)
        );
    }

    #[tokio::test]
    #[rustfmt::skip]
    async fn history_replays_durable_tool_exchange_and_rejects_oversized_newest_segment() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "history fixture").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(
                    Some("read it"),
                    vec![crate::provider::ToolCall {
                        id: "history-read".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path":"note.txt"}),
                    }],
                ),
                response(Some("complete"), vec![]),
            ],
        );
        let completed = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "read for history".into(),
                binding(),
            )
            .await
            .unwrap();
        let replay = service
            .history_with_prompt(&completed, "follow up")
            .unwrap();
        assert!(replay.iter().any(
            |message| matches!(message, Message::Assistant { tool_calls, .. } if tool_calls.len() == 1)
        ));
        assert!(replay.iter().any(
            |message| matches!(message, Message::Tool { tool_call_id, content, .. } if tool_call_id == "history-read" && content.contains("history fixture"))
        ));
        let without_empty_tail = service.history_from_snapshot(&completed).unwrap();
        assert!(!matches!(
            without_empty_tail.last(),
            Some(Message::User { content }) if content.is_empty()
        ));

        let constrained = ThreadRuntimeService::new(
            EngineBuilder::new()
                .workspace_root(root.path())
                .build()
                .unwrap(),
            root.path(),
            ThreadHistoryPolicy {
                max_request_bytes: 512,
                max_input_bytes: 512,
                reserved_output_bytes: 1,
                context_cap_bytes: 1,
            },
            Arc::new(|_| Err("not used".into())),
        );
        let mut bounded = completed.clone(); bounded.transcript.entries.iter_mut().find(|entry| entry.kind == TranscriptKind::User).unwrap().text = "word ".repeat(1_000);
        assert!(constrained.history_with_prompt(&bounded, "small").unwrap().iter().any(|message| matches!(message, Message::User { content } if content == "small")));
        assert!(constrained.enforce_budget(vec![Message::User { content: "word ".repeat(1_000) }]).is_err());
        let mut orphan = completed.clone(); orphan.transcript.entries.retain(|entry| entry.kind != TranscriptKind::User); assert!(constrained.history_with_prompt(&orphan, "small").is_ok());
        let error = constrained
            .history_with_prompt(&completed, &"word ".repeat(1_000))
            .unwrap_err();
        assert!(
            error.to_string().contains("newest complete user segment"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn v2_read_file_secrets_are_redacted_before_persistence_and_history_replay() {
        let root = tempfile::tempdir().unwrap();
        let secret = "sk-proj-0123456789abcdefghijklmnopqrstuvwxyz";
        std::fs::write(
            root.path().join("provider.env"),
            format!("OPENAI_API_KEY={secret}\n"),
        )
        .unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(
                    Some("reading credentials"),
                    vec![crate::provider::ToolCall {
                        id: "read-provider-env".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path":"provider.env"}),
                    }],
                ),
                response(Some("done"), vec![]),
            ],
        );
        let completed = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "inspect provider settings".into(),
                binding(),
            )
            .await
            .unwrap();
        let durable = engine
            .thread_snapshot_v2(completed.thread_id, None, 100)
            .unwrap();
        let transcript = serde_json::to_string(&durable.transcript).unwrap();
        assert!(!transcript.contains(secret));
        assert!(transcript.contains("[REDACTED]"));

        let history = service.history_with_prompt(&durable, "continue").unwrap();
        let replay = serde_json::to_string(&history).unwrap();
        assert!(!replay.contains(secret));
        assert!(replay.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn v2_permission_denial_and_waiting_cancel_never_execute_prepared_write() {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let engine = EngineBuilder::new()
                .workspace_root(root.path())
                .build()
                .unwrap();
            let service = scripted_service(
                root.path(),
                engine,
                vec![response(
                    Some("creating"),
                    vec![crate::provider::ToolCall {
                        id: format!("create-{cancel}"),
                        name: "write_file".into(),
                        input: serde_json::json!({
                            "path":"created.txt",
                            "content":"must not appear",
                            "create_intent":true
                        }),
                    }],
                )],
            );
            let waiting = service
                .start(
                    ThreadId::from_uuid(Uuid::now_v7()),
                    "create it".into(),
                    binding(),
                )
                .await
                .unwrap();
            let terminal = if cancel {
                service.cancel_durable(waiting.thread_id).unwrap()
            } else {
                let request_id = match waiting.pending.as_ref().unwrap() {
                    latte_core::ThreadPendingRequest::Permission { request_id, .. } => {
                        request_id.clone()
                    }
                    latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
                };
                service
                    .resolve_permission(waiting.thread_id, waiting.revision, request_id, false)
                    .await
                    .unwrap()
            };
            assert_eq!(
                terminal.lifecycle,
                if cancel {
                    ThreadLifecycle::Failed
                } else {
                    ThreadLifecycle::Ready
                }
            );
            assert!(!root.path().join("created.txt").exists());
            assert!(terminal.active_run_id.is_none());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_allowed_process_is_supervised_and_reenters_provider() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(
                    Some("pwd"),
                    vec![crate::provider::ToolCall {
                        id: "pwd".into(),
                        name: "process".into(),
                        input: serde_json::json!({"argv":["/bin/pwd"]}),
                    }],
                ),
                response(Some("done"), vec![]),
            ],
        );
        let snapshot = service
            .start(
                ThreadId::from_uuid(Uuid::now_v7()),
                "show cwd".into(),
                binding(),
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, ThreadLifecycle::Ready);
        assert!(
            snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::ToolResult)
        );
    }

    #[tokio::test]
    async fn v2_provider_heartbeat_renews_past_initial_lease() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = delayed_service(
            root.path(),
            engine.clone(),
            [(Duration::from_millis(900), response(Some("done"), vec![]))],
        )
        .with_lease_ttl_ms(300);
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let runner = service.clone();
        let run =
            tokio::spawn(async move { runner.start(thread_id, "wait".into(), binding()).await });
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(matches!(
            engine.acquire_thread_lease(thread_id, now_ms(), 60),
            Err(StorageError::EngineUnavailable)
        ));
        let parallel = engine
            .acquire_thread_lease(ThreadId::from_uuid(Uuid::now_v7()), now_ms(), 60)
            .unwrap();
        engine.release_lease(&parallel).unwrap();
        assert_eq!(
            run.await.unwrap().unwrap().lifecycle,
            ThreadLifecycle::Ready
        );
        let resumed = engine
            .acquire_thread_lease(thread_id, now_ms(), 60)
            .unwrap();
        engine.release_lease(&resumed).unwrap();
    }

    #[tokio::test]
    async fn two_sessions_run_concurrently_without_sharing_runtime_authority() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let factory: ThreadProviderFactory = Arc::new(|_| {
            Ok(ResolvedProvider {
                provider: Arc::new(DelayedProvider::scripted([(
                    Duration::from_millis(200),
                    response(Some("done"), vec![]),
                )])),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service =
            ThreadRuntimeService::new(engine, root.path(), ThreadHistoryPolicy::default(), factory);
        let first = service.clone();
        let second = service.clone();
        let first_thread = ThreadId::from_uuid(Uuid::now_v7());
        let second_thread = ThreadId::from_uuid(Uuid::now_v7());

        let (first, second) = tokio::join!(
            first.start(first_thread, "first".into(), binding()),
            second.start(second_thread, "second".into(), binding()),
        );

        assert_eq!(first.unwrap().lifecycle, ThreadLifecycle::Ready);
        assert_eq!(second.unwrap().lifecycle, ThreadLifecycle::Ready);
    }

    #[tokio::test]
    async fn v2_provider_renewal_failure_interrupts_without_assistant_success() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&database)
            .build()
            .unwrap();
        let service = delayed_service(
            root.path(),
            engine.clone(),
            [(
                Duration::from_secs(2),
                response(Some("must not commit"), vec![]),
            )],
        )
        .with_lease_ttl_ms(300);
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .start(thread_id, "wait for fencing".into(), binding())
                .await
        });

        wait_until(
            || {
                engine
                    .thread_snapshot_v2(thread_id, None, 100)
                    .is_ok_and(|snapshot| {
                        snapshot.lifecycle == ThreadLifecycle::Running
                            && snapshot.active_run_id.is_some()
                    })
            },
            "provider turn to become active",
        )
        .await;
        let active = engine.thread_snapshot_v2(thread_id, None, 100).unwrap();
        let run_id = active.active_run_id.unwrap();
        force_lease_renewal_failure(&database);

        let error = run.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("lease heartbeat lost during provider call"),
            "unexpected provider lease-loss error: {error}"
        );
        let terminal = engine.thread_snapshot_v2(thread_id, None, 100).unwrap();
        assert_eq!(terminal.lifecycle, ThreadLifecycle::Interrupted);
        assert!(terminal.active_run_id.is_none());
        assert_eq!(
            engine.show(run_id).unwrap().status,
            latte_core::RunStatus::Interrupted
        );
        assert!(
            !terminal
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::Assistant
                    && entry.text == "must not commit"),
            "a provider response cannot be observed after lease loss"
        );
        assert!(
            terminal
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::System
                    && entry.text.contains("lease authority lost"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_started_process_heartbeat_renews_and_cancel_never_observes_success() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = delayed_service(
            root.path(),
            engine.clone(),
            [
                (
                    Duration::ZERO,
                    response(
                        Some("sleeping"),
                        vec![crate::provider::ToolCall {
                            id: "sleep-tool".into(),
                            name: "process".into(),
                            input: serde_json::json!({
                                "argv":["/bin/sleep","1"],
                                "timeout_ms":2_000,
                                "grace_ms":10,
                            }),
                        }],
                    ),
                ),
                (
                    Duration::ZERO,
                    response(Some("must not be observed"), vec![]),
                ),
            ],
        )
        // The assertion below crosses the initial lease boundary. Keep the
        // boundary comfortably above instrumented CI startup time, then wait
        // past it so the heartbeat—not scheduler speed—proves renewal.
        .with_lease_ttl_ms(300);
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let waiting = service
            .start(thread_id, "sleep".into(), binding())
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .resolve_permission(thread_id, waiting.revision, request_id, true)
                .await
        });
        let effect_id = loop {
            if let Ok(snapshot) = engine.thread_snapshot_v2(thread_id, None, 100)
                && let Some(effect_id) = snapshot.transcript.entries.iter().find_map(|entry| {
                    entry
                        .payload
                        .as_ref()?
                        .get("descriptor")?
                        .get("effect_id")?
                        .as_str()
                        .map(str::to_owned)
                })
                && engine.effect_status(&effect_id).unwrap() == latte_engine::EffectStatus::Started
            {
                break effect_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        tokio::time::sleep(Duration::from_millis(450)).await;
        let parallel = engine
            .acquire_thread_lease(ThreadId::from_uuid(Uuid::now_v7()), now_ms(), 60)
            .unwrap();
        engine.release_lease(&parallel).unwrap();
        service.cancel(thread_id);
        let terminal = run.await.unwrap().unwrap();
        assert_eq!(terminal.lifecycle, ThreadLifecycle::ReconciliationRequired);
        assert!(terminal.active_run_id.is_none());
        assert_eq!(
            engine.effect_status(&effect_id).unwrap(),
            latte_engine::EffectStatus::Unknown
        );
        assert!(
            !terminal
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::ToolResult)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn v2_started_process_renewal_failure_marks_unknown_and_reconciles() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&database)
            .build()
            .unwrap();
        let service = delayed_service(
            root.path(),
            engine.clone(),
            [
                (
                    Duration::ZERO,
                    response(
                        Some("sleeping"),
                        vec![crate::provider::ToolCall {
                            id: "forced-lease-loss-process".into(),
                            name: "process".into(),
                            input: serde_json::json!({
                                "argv":["/bin/sleep","1"],
                                "timeout_ms":2_000,
                                "grace_ms":10,
                            }),
                        }],
                    ),
                ),
                (
                    Duration::ZERO,
                    response(Some("must not be observed"), vec![]),
                ),
            ],
        )
        // Fault injection removes the durable lease after the process starts;
        // a sub-100 ms TTL only makes pre-permission setup scheduler-sensitive
        // under coverage instrumentation and is not part of this test's claim.
        .with_lease_ttl_ms(300);
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let waiting = service
            .start(thread_id, "sleep until fenced".into(), binding())
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::ThreadPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::ThreadPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .resolve_permission(thread_id, waiting.revision, request_id, true)
                .await
        });
        let effect_id = loop {
            let snapshot = engine.thread_snapshot_v2(thread_id, None, 100).unwrap();
            if let Some(effect_id) = snapshot.transcript.entries.iter().find_map(|entry| {
                entry
                    .payload
                    .as_ref()?
                    .get("descriptor")?
                    .get("effect_id")?
                    .as_str()
                    .map(str::to_owned)
            }) && engine.effect_status(&effect_id).unwrap()
                == latte_engine::EffectStatus::Started
            {
                break effect_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let before_loss = engine.thread_snapshot_v2(thread_id, None, 100).unwrap();
        let linked_run_id = before_loss.active_run_id.unwrap();
        force_lease_renewal_failure(&database);

        let error = run.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("lease heartbeat lost during started effect")
        );
        let recovered = engine.thread_snapshot_v2(thread_id, None, 100).unwrap();
        assert_eq!(recovered.lifecycle, ThreadLifecycle::ReconciliationRequired);
        assert!(recovered.active_run_id.is_none());
        assert_eq!(
            engine.effect_status(&effect_id).unwrap(),
            latte_engine::EffectStatus::Unknown
        );
        assert_eq!(
            engine.show(linked_run_id).unwrap().status,
            latte_core::RunStatus::Interrupted
        );
        assert!(
            !recovered
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::ToolResult),
            "a started process must not produce an observed result after lease loss"
        );
        let reconciled = service
            .reconcile_unknown_effect(thread_id, &effect_id)
            .unwrap();
        assert_eq!(reconciled.lifecycle, ThreadLifecycle::Failed);
        assert!(reconciled.active_run_id.is_none());
        assert_eq!(
            engine.effect_status(&effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedFailed
        );
        assert_eq!(
            engine.show(linked_run_id).unwrap().status,
            latte_core::RunStatus::Failed
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn v2_restart_recovers_started_effect_and_reconciles_exact_child() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("state.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&db)
            .build()
            .unwrap();
        std::fs::write(root.path().join("note.txt"), "recovery fixture").unwrap();
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let run_id = new_run_id();
        engine
            .create_thread_v2(thread_id, run_id, binding(), "recover", 1)
            .unwrap();
        let lease = engine
            .acquire_thread_lease(thread_id, now_ms(), 10_000)
            .unwrap();
        let running = engine
            .commit_thread_run_update(
                ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: 0,
                    expected_run_revision: 0,
                    command_id: ThreadCommandId::from_uuid(Uuid::now_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitThreadRunUpdate::Start {
                        source_key: format!("{run_id}:start"),
                    },
                },
                &lease,
                now_ms(),
            )
            .unwrap()
            .snapshot;
        let descriptor = ThreadEffectDescriptor {
            effect_id: format!("thread-effect:{run_id}:recover"),
            tool_call_id: "recover".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"note.txt"}),
            attempt: 1,
        };
        let prepared = engine
            .prepare_thread_effect(
                thread_effect_request(&running, descriptor.clone(), format!("{run_id}:prepare"))
                    .unwrap(),
                &lease,
                now_ms(),
            )
            .unwrap();
        let started = engine
            .start_thread_effect(
                thread_effect_start_request(
                    &prepared.snapshot,
                    descriptor.effect_id.clone(),
                    format!("{run_id}:start-effect"),
                )
                .unwrap(),
                prepared.operation_digest,
                &lease,
                now_ms(),
            )
            .unwrap();
        assert!(
            rusqlite::Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_checkpoints WHERE run_id=?1)",
                    [run_id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        // Model a crashed owner deterministically. Recovery depends on the
        // absence of live authority, not scheduler timing around a tiny TTL.
        force_lease_renewal_failure(&db);
        drop(started);
        drop(engine);
        let reopened = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&db)
            .build()
            .unwrap();
        let recovered = reopened.thread_snapshot_v2(thread_id, None, 100).unwrap();
        assert_eq!(recovered.lifecycle, ThreadLifecycle::ReconciliationRequired);
        assert!(recovered.active_run_id.is_none());
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            latte_engine::EffectStatus::Unknown
        );
        assert_eq!(
            reopened.show(run_id).unwrap().status,
            latte_core::RunStatus::Interrupted
        );
        assert!(
            rusqlite::Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_checkpoints WHERE run_id=?1)",
                    [run_id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert!(recovered.transcript.entries.windows(2).any(|pair| {
            pair[0].kind == TranscriptKind::System && pair[1].kind == TranscriptKind::Failure
        }));
        let service = scripted_service(root.path(), reopened.clone(), vec![]);
        let reconciled = service
            .reconcile_unknown_effect(thread_id, &descriptor.effect_id)
            .unwrap();
        assert_eq!(reconciled.lifecycle, ThreadLifecycle::Failed);
        assert!(reconciled.active_run_id.is_none());
        assert_eq!(
            reopened.show(run_id).unwrap().status,
            latte_core::RunStatus::Failed
        );
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedFailed
        );
    }

    #[tokio::test]
    async fn v2_restart_terminalizes_prepared_permission_without_unknown_effect() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("state.db");
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&db)
            .build()
            .unwrap();
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let run_id = new_run_id();
        engine
            .create_thread_v2(thread_id, run_id, binding(), "prepare", 1)
            .unwrap();
        let lease = engine
            .acquire_thread_lease(thread_id, now_ms(), 100)
            .unwrap();
        let running = engine
            .commit_thread_run_update(
                ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: 0,
                    expected_run_revision: 0,
                    command_id: ThreadCommandId::from_uuid(Uuid::now_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitThreadRunUpdate::Start {
                        source_key: format!("{run_id}:start"),
                    },
                },
                &lease,
                now_ms(),
            )
            .unwrap()
            .snapshot;
        let descriptor = ThreadEffectDescriptor {
            effect_id: format!("thread-effect:{run_id}:prepared"),
            tool_call_id: "prepared".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path":"must-not-exist.txt",
                "content":"never executed",
                "create_intent":true,
            }),
            attempt: 1,
        };
        let prepared = engine
            .prepare_thread_effect(
                thread_effect_request(&running, descriptor.clone(), format!("{run_id}:prepare"))
                    .unwrap(),
                &lease,
                now_ms(),
            )
            .unwrap();
        assert_eq!(
            prepared.snapshot.lifecycle,
            ThreadLifecycle::WaitingPermission
        );
        drop(engine);
        tokio::time::sleep(Duration::from_millis(125)).await;
        let reopened = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&db)
            .build()
            .unwrap();
        let recovered = reopened.thread_snapshot_v2(thread_id, None, 100).unwrap();
        assert_eq!(recovered.lifecycle, ThreadLifecycle::Interrupted);
        assert!(recovered.active_run_id.is_none());
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedFailed
        );
        assert_eq!(
            reopened.show(run_id).unwrap().status,
            latte_core::RunStatus::Interrupted
        );
        assert!(!root.path().join("must-not-exist.txt").exists());
    }

    #[tokio::test]
    async fn coordinator_rejects_wrong_lifecycle_and_cancels_only_registered_active_work() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = ThreadRuntimeService::new(
            engine.clone(),
            root.path(),
            ThreadHistoryPolicy::default(),
            Arc::new(|_| Err("provider must not be constructed".into())),
        );
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let run_id = new_run_id();
        let running = engine
            .create_thread_v2(thread_id, run_id, binding(), "initial", 1)
            .unwrap();

        assert!(matches!(
            service
                .follow_up(thread_id, running.revision, "too early".into())
                .await,
            Err(ThreadRuntimeError::InvalidState)
        ));
        assert!(matches!(
            service
                .provide_input(
                    thread_id,
                    running.revision,
                    "missing".into(),
                    "value".into()
                )
                .await,
            Err(ThreadRuntimeError::InvalidState)
        ));
        assert!(matches!(
            service
                .resolve_permission(thread_id, running.revision, "missing".into(), true)
                .await,
            Err(ThreadRuntimeError::InvalidState)
        ));
        assert!(matches!(
            service.reconcile_unknown_effect(thread_id, "missing"),
            Err(ThreadRuntimeError::InvalidState)
        ));

        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<dyn ThreadProgressSink> = {
            let observed = Arc::clone(&observed);
            Arc::new(move |progress| observed.lock().unwrap().push(progress))
        };
        let service = service.with_progress_sink(sink);
        let token = CancellationToken::new();
        service
            .active
            .lock()
            .unwrap()
            .insert(thread_id, token.clone());
        service.cancel(thread_id);
        assert!(token.is_cancelled());
        assert_eq!(
            service.cancel_durable(thread_id).unwrap().thread_id,
            thread_id,
            "registered in-flight work is cancelled transiently without forging a durable terminal"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    #[rustfmt::skip]
    fn verification_and_transcript_helpers_fail_closed_on_missing_authority() {
        use latte_core::{ThreadRunStatus, ThreadRunSummary, TranscriptEntry, TranscriptEntryId};

        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        let run_id = new_run_id();
        let mut snapshot = engine
            .create_thread_v2(thread_id, run_id, binding(), "initial", 1)
            .unwrap();
        let base = ThreadRuntimeService::new(
            engine,
            root.path(),
            ThreadHistoryPolicy::default(),
            Arc::new(|_| Err("not used".into())),
        );
        assert!(matches!(
            base.verification_descriptor(&snapshot, "done"),
            Err(ThreadRuntimeError::Effect(message)) if message.contains("no configured verification")
        ));
        let empty = base.clone().with_verification(VerificationPlan {
            argv: vec![],
            cwd: ".".into(),
            timeout_ms: 1,
            grace_ms: 1,
            stdout_cap: 1,
            stderr_cap: 1,
        });
        assert!(matches!(
            empty.verification_descriptor(&snapshot, "done"),
            Err(ThreadRuntimeError::Effect(message)) if message.contains("argv is empty")
        ));
        let verified = base.with_verification(VerificationPlan {
            argv: vec!["/bin/true".into()],
            cwd: ".".into(),
            timeout_ms: 1_000,
            grace_ms: 100,
            stdout_cap: 1_024,
            stderr_cap: 1_024,
        });
        let descriptor = verified
            .verification_descriptor(&snapshot, "done\napi_key=secret")
            .unwrap();
        assert_eq!(descriptor.name, "process");
        assert!(!descriptor.input.to_string().contains("api_key=secret"));

        snapshot.active_run_id = None;
        assert!(matches!(
            active_run_revision(&snapshot),
            Err(ThreadRuntimeError::InvalidState)
        ));
        assert!(
            thread_effect_request(&snapshot, descriptor.clone(), "missing-run".into()).is_err()
        );
        assert!(
            thread_effect_start_request(
                &snapshot,
                descriptor.effect_id.clone(),
                "missing-run".into()
            )
            .is_err()
        );
        let live = verified.acquire(thread_id).unwrap();
        assert!(verified.recover_lease_loss(&snapshot, &live, "test").to_string().contains("active linked run is unavailable"));

        snapshot.active_run_id = Some(run_id);
        snapshot.runs = vec![ThreadRunSummary {
            run_id: new_run_id(),
            parent_run_id: None,
            ordinal: 0,
            status: ThreadRunStatus::Running,
            run_revision: 1,
            completed_at_ms: None,
        }];
        assert!(matches!(
            active_run_revision(&snapshot),
            Err(ThreadRuntimeError::InvalidState)
        ));
        assert!(verified.recover_lease_loss(&snapshot, &live, "test").to_string().contains("linked run revision is unavailable"));
        snapshot.runs[0].run_id = run_id;
        let presentation = ThreadEffectPresentation { effect_id: descriptor.effect_id.clone(), tool_call_id: descriptor.tool_call_id.clone(), name: descriptor.name.clone(), input: descriptor.input.clone(), attempt: descriptor.attempt };
        assert!(verified.finish_verification(&snapshot, &presentation, &live).unwrap_err().to_string().contains("observation is missing"));

        let call = crate::provider::ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
        };
        snapshot.transcript.entries = vec![
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 2,
                run_id: Some(run_id),
                kind: TranscriptKind::Assistant,
                text: "call".into(),
                payload: Some(serde_json::json!({"tool_calls":[call.clone()]})),
                source_key: "assistant".into(),
                created_at_ms: 2,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 3,
                run_id: Some(run_id),
                kind: TranscriptKind::System,
                text: "ignore".into(),
                payload: None,
                source_key: "system".into(),
                created_at_ms: 3,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 4,
                run_id: Some(run_id),
                kind: TranscriptKind::ToolResult,
                text: "missing payload".into(),
                payload: None,
                source_key: "result-empty".into(),
                created_at_ms: 4,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 5,
                run_id: Some(run_id),
                kind: TranscriptKind::ToolResult,
                text: "result".into(),
                payload: Some(serde_json::json!({
                    "tool_call_id":"call-1",
                    "provider_content":"provider-safe-result"
                })),
                source_key: "result".into(),
                created_at_ms: 5,
            },
        ];
        let (sequence, calls, ordinal) = tool_round_for_call(&snapshot, "call-1").unwrap();
        assert_eq!((sequence, calls, ordinal), (2, vec![call], 0));
        assert!(tool_round_for_call(&snapshot, "missing").is_err());
        assert_eq!(
            effect_provider_result(&snapshot, "call-1").as_deref(),
            Some("provider-safe-result")
        );
        assert_eq!(effect_provider_result(&snapshot, "missing"), None);

        let mut running = verified.commit(thread_id, run_id, 0, 0, CommitThreadRunUpdate::Start { source_key: "helper:start".into() }, &live).unwrap();
        assert!(verified.recover_lease_loss(&running, &live, "test").to_string().contains("recovery failed"));
        let takeover = verified
            .engine
            .acquire_thread_lease(thread_id, live.expires_at_ms(), 60_000)
            .unwrap();
        let mut mismatched = running.clone();
        mismatched.runs[0].run_revision += 1;
        assert!(
            verified
                .recover_lease_loss(&mismatched, &live, "test")
                .to_string()
                .contains("newer owner fenced")
        );
        let live = ThreadLeaseGuard {
            engine: verified.engine.clone(),
            lease: takeover,
        };
        for index in 0..501 { running = verified.commit(thread_id, run_id, running.revision, 1, CommitThreadRunUpdate::AppendTranscript { source_key: format!("helper:page:{index}"), kind: TranscriptKind::System, text: index.to_string(), payload: None }, &live).unwrap(); }
        assert_eq!(verified.load_full(thread_id).unwrap().transcript.entries.len(), 502);
        let mut observed = running.clone();
        observed.transcript = snapshot.transcript.clone();
        let mut presentation = presentation;
        presentation.tool_call_id = "call-1".into();
        assert!(verified.finish_verification(&observed, &presentation, &live).unwrap_err().to_string().contains("not a process result"));
        let mut output = latte_engine::ProcessOutput { exit_code: Some(0), stdout: String::new(), stderr: String::new(), stdout_truncated: false, stderr_truncated: false, termination: latte_engine::ProcessTermination::Exited };
        observed.transcript.entries.last_mut().unwrap().payload.as_mut().unwrap()["provider_content"] = serde_json::Value::String(serde_json::to_string(&output).unwrap());
        presentation.effect_id = "missing-summary".into();
        presentation.input = serde_json::json!({});
        assert!(verified.finish_verification(&observed, &presentation, &live).unwrap_err().to_string().contains("completion summary is missing"));
        output.exit_code = Some(1);
        observed.transcript.entries.last_mut().unwrap().payload.as_mut().unwrap()["provider_content"] = serde_json::Value::String(serde_json::to_string(&output).unwrap());
        presentation.effect_id = "failed-verification".into();
        presentation.input = descriptor.input;
        assert_eq!(verified.finish_verification(&observed, &presentation, &live).unwrap().lifecycle, ThreadLifecycle::Failed);
        verified.engine.release_lease(&live).unwrap(); assert!(verified.recover_lease_loss(&observed, &live, "test").to_string().contains("already terminal"));
    }
}
