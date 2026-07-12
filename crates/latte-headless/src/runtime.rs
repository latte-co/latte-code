use crate::{
    context,
    provider::{InputRequest, Message, Provider, ProviderError, ToolCall, valid_tool_call_id},
};
use latte_core::{
    FailureCode, IdSource, PendingPermission, Retryability, RunFailure, RunId, RunState,
    RuntimeEvent, SystemIdSource, Transition, VerificationStatus,
};
use latte_engine::{
    CancellationToken, EngineHandle, Lease, ProcessDecision, ProcessError, ProcessInvocation,
    ToolError, ToolInvocation, classify,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("engine: {0}")]
    Engine(String),
    #[error("permission required for run {run_id}")]
    PermissionRequired { run_id: RunId },
    #[error("run has no resumable permission")]
    NotWaiting,
    #[error("effect {effect_id} is unknown and requires explicit reconciliation")]
    UnknownEffect { effect_id: String },
    #[error("input required for run {run_id}")]
    InputRequired { run_id: RunId },
    #[error("secret input requests are unsupported and were not persisted")]
    SecretInputUnsupported,
    #[error("checkpoint invalid: {0}")]
    CheckpointInvalid(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    messages: Vec<Message>,
    pending: Option<PendingCall>,
    #[serde(default)]
    final_message: Option<String>,
    #[serde(default)]
    baseline: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    tool_queue: Vec<ToolCall>,
    #[serde(default)]
    tool_cursor: usize,
    #[serde(default)]
    pending_input: Option<InputRequest>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingCall {
    call: ToolCall,
    effect_id: String,
    operation_digest: String,
    phase: PendingPhase,
}
#[allow(clippy::too_many_lines)]
enum CheckpointDisposition {
    Ready {
        checkpoint: Box<Checkpoint>,
        normalized: bool,
    },
    RequiresUnknown {
        effect_id: String,
    },
}
#[allow(clippy::too_many_lines)]
fn validate_and_normalize(
    mut checkpoint: Checkpoint,
    run_state: &RunState,
) -> Result<CheckpointDisposition, RuntimeError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut round: Option<(Vec<ToolCall>, usize)> = None;
    let mut last_completed = Vec::new();
    for (index, message) in checkpoint.messages.iter().enumerate() {
        match message {
            Message::Assistant { tool_calls, .. } => {
                if round.is_some() {
                    return Err(RuntimeError::CheckpointInvalid(format!(
                        "assistant before tool round resolved at message {index}"
                    )));
                }
                for call in tool_calls {
                    if !valid_tool_call_id(&call.id) || !seen.insert(call.id.clone()) {
                        return Err(RuntimeError::CheckpointInvalid(
                            "invalid or duplicate assistant tool call id".into(),
                        ));
                    }
                }
                if !tool_calls.is_empty() {
                    round = Some((tool_calls.clone(), 0));
                }
            }
            Message::Tool {
                tool_call_id, name, ..
            } => {
                let Some((calls, cursor)) = round.as_mut() else {
                    return Err(RuntimeError::CheckpointInvalid("orphan tool result".into()));
                };
                let Some(expected) = calls.get(*cursor) else {
                    return Err(RuntimeError::CheckpointInvalid("extra tool result".into()));
                };
                if tool_call_id != &expected.id
                    || name.as_ref().is_some_and(|value| value != &expected.name)
                {
                    return Err(RuntimeError::CheckpointInvalid(format!(
                        "out-of-order tool result at message {index}"
                    )));
                }
                *cursor += 1;
                if *cursor == calls.len() {
                    last_completed.clone_from(calls);
                    round = None;
                }
            }
            Message::System { .. } | Message::User { .. } if round.is_some() => {
                return Err(RuntimeError::CheckpointInvalid(format!(
                    "message interrupts tool round at index {index}"
                )));
            }
            Message::System { .. } | Message::User { .. } => {}
        }
    }
    match &round {
        Some((calls, resolved))
            if calls != &checkpoint.tool_queue || *resolved != checkpoint.tool_cursor =>
        {
            return Err(RuntimeError::CheckpointInvalid(
                "tool queue mismatch".into(),
            ));
        }
        None if !checkpoint.tool_queue.is_empty()
            && (checkpoint.pending.is_some()
                || checkpoint.tool_cursor != checkpoint.tool_queue.len()
                || checkpoint.tool_queue != last_completed) =>
        {
            return Err(RuntimeError::CheckpointInvalid(
                "invalid resolved crash queue".into(),
            ));
        }
        None if checkpoint.tool_queue.is_empty() && checkpoint.tool_cursor != 0 => {
            return Err(RuntimeError::CheckpointInvalid(
                "orphan queue cursor".into(),
            ));
        }
        Some(_) | None => {}
    }
    if let Some(pending) = &checkpoint.pending {
        if !valid_tool_call_id(&pending.call.id) {
            return Err(RuntimeError::CheckpointInvalid(
                "invalid pending call id".into(),
            ));
        }
        match pending.phase {
            PendingPhase::Tool
                if checkpoint
                    .tool_queue
                    .get(checkpoint.tool_cursor)
                    .is_none_or(|call| pending.call.id != call.id) =>
            {
                return Err(RuntimeError::CheckpointInvalid(
                    "pending queue mismatch".into(),
                ));
            }
            PendingPhase::Verification if round.is_some() || !checkpoint.tool_queue.is_empty() => {
                return Err(RuntimeError::CheckpointInvalid(
                    "premature verification".into(),
                ));
            }
            PendingPhase::Tool | PendingPhase::Verification => {}
        }
    }
    if let Some(input) = &checkpoint.pending_input
        && (round.is_some()
            || input.id.is_empty()
            || input.id.len() > 256
            || input.prompt.is_empty()
            || input.prompt.len() > 4096
            || input.secret)
    {
        return Err(RuntimeError::CheckpointInvalid(
            "invalid input request".into(),
        ));
    }
    if checkpoint.pending.is_some() && checkpoint.pending_input.is_some() {
        return Err(RuntimeError::CheckpointInvalid(
            "multiple wait payloads".into(),
        ));
    }
    if checkpoint.pending.as_ref().is_some_and(|pending| {
        matches!(
            (pending.phase, checkpoint.final_message.is_some()),
            (PendingPhase::Tool, true) | (PendingPhase::Verification, false)
        )
    }) {
        return Err(RuntimeError::CheckpointInvalid(
            "pending phase and final message mismatch".into(),
        ));
    }
    if run_state.status == latte_core::RunStatus::WaitingPermission
        || (run_state.status == latte_core::RunStatus::Interrupted
            && run_state.pending_permission.is_some())
    {
        let pending = checkpoint
            .pending
            .as_ref()
            .ok_or_else(|| RuntimeError::CheckpointInvalid("missing pending operation".into()))?;
        let permission = run_state.pending_permission.as_ref().ok_or_else(|| {
            RuntimeError::CheckpointInvalid("missing state permission binding".into())
        })?;
        if pending.effect_id != permission.request_id
            || pending.operation_digest != permission.operation_digest
        {
            return Err(RuntimeError::CheckpointInvalid(
                "pending effect binding mismatch".into(),
            ));
        }
    }
    let wait_valid = match run_state.status {
        latte_core::RunStatus::WaitingPermission => {
            checkpoint.pending.is_some() && checkpoint.pending_input.is_none()
        }
        latte_core::RunStatus::WaitingInput => {
            checkpoint.pending.is_none() && checkpoint.pending_input.is_some()
        }
        latte_core::RunStatus::Interrupted => {
            match (&checkpoint.pending, &checkpoint.pending_input) {
                (None, None) => true,
                (Some(pending), None) => match pending.phase {
                    PendingPhase::Tool => checkpoint.final_message.is_none(),
                    PendingPhase::Verification => checkpoint.final_message.is_some(),
                },
                (None, Some(input)) => {
                    checkpoint.final_message.is_none()
                        && run_state.pending_input.as_ref().is_some_and(|state_input| {
                            state_input.request_id == input.id && state_input.prompt == input.prompt
                        })
                }
                (Some(_), Some(_)) => false,
            }
        }
        latte_core::RunStatus::Queued
        | latte_core::RunStatus::Running
        | latte_core::RunStatus::Cancelling
        | latte_core::RunStatus::Completed
        | latte_core::RunStatus::Failed => {
            checkpoint.pending.is_none() && checkpoint.pending_input.is_none()
        }
    };
    if !wait_valid {
        return Err(RuntimeError::CheckpointInvalid(
            "run wait state mismatch".into(),
        ));
    }
    if checkpoint.final_message.is_some()
        && (run_state.status == latte_core::RunStatus::WaitingInput
            || (run_state.status == latte_core::RunStatus::WaitingPermission
                && checkpoint
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.phase == PendingPhase::Tool)))
    {
        return Err(RuntimeError::CheckpointInvalid(
            "final message is incompatible with wait state".into(),
        ));
    }
    let normalize = round.is_none() && !checkpoint.tool_queue.is_empty();
    if normalize {
        checkpoint.tool_queue.clear();
        checkpoint.tool_cursor = 0;
    }
    if let Some(pending) = checkpoint.pending.as_ref()
        && run_state.status == latte_core::RunStatus::Interrupted
        && run_state.pending_permission.is_none()
    {
        return Ok(CheckpointDisposition::RequiresUnknown {
            effect_id: pending.effect_id.clone(),
        });
    }
    Ok(CheckpointDisposition::Ready {
        checkpoint: Box::new(checkpoint),
        normalized: normalize,
    })
}
#[cfg(test)]
fn validate_checkpoint(checkpoint: &Checkpoint) -> Result<(), RuntimeError> {
    let status = if checkpoint.pending.is_some() {
        latte_core::RunStatus::WaitingPermission
    } else if checkpoint.pending_input.is_some() {
        latte_core::RunStatus::WaitingInput
    } else {
        latte_core::RunStatus::Running
    };
    let mut state = RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
    state.status = status;
    state.pending_permission = checkpoint
        .pending
        .as_ref()
        .map(|pending| PendingPermission {
            request_id: pending.effect_id.clone(),
            operation_digest: pending.operation_digest.clone(),
            description: "test".into(),
        });
    validate_and_normalize(checkpoint.clone(), &state).map(|_| ())
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PendingPhase {
    Tool,
    Verification,
}
pub struct AgentRuntime<P> {
    engine: EngineHandle,
    provider: P,
    root: PathBuf,
    ids: SystemIdSource,
    cancellation: CancellationToken,
    verification: VerificationPlan,
    lease_ttl_ms: u64,
}
#[derive(Clone, Debug)]
pub struct VerificationPlan {
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_ms: u64,
    pub grace_ms: u64,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
}
impl<P: Provider> AgentRuntime<P> {
    pub fn new(
        engine: EngineHandle,
        provider: P,
        root: impl AsRef<Path>,
        verification: VerificationPlan,
    ) -> Self {
        Self {
            engine,
            provider,
            root: root.as_ref().to_owned(),
            ids: SystemIdSource::default(),
            cancellation: CancellationToken::new(),
            verification,
            lease_ttl_ms: 60_000,
        }
    }
    #[must_use]
    pub fn with_verification(mut self, plan: VerificationPlan) -> Self {
        self.verification = plan;
        self
    }
    #[cfg(test)]
    fn with_lease_ttl(mut self, ttl_ms: u64) -> Self {
        self.lease_ttl_ms = ttl_ms;
        self
    }
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    /// Replaces the cancellation source so a command service can cancel an active run.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }
    /// Explicitly acknowledges an unknown external effect as failed and aborts the run.
    pub fn reconcile_unknown_and_abort(
        &self,
        run_id: RunId,
        effect_id: &str,
    ) -> Result<RunState, RuntimeError> {
        let now = now_ms();
        let state = self.engine.show(run_id).map_err(engine)?;
        let lease = self
            .engine
            .acquire_lease(&format!("reconcile-{run_id}"), now, self.authority_ttl())
            .map_err(engine)?;
        self.engine
            .resolve_unknown_effect_and_abort(run_id, effect_id, state.revision, &lease, now)
            .map_err(engine)
    }
    pub async fn run(&self, prompt: &str) -> Result<RunState, RuntimeError> {
        self.run_with_focus(prompt, None).await
    }
    pub async fn run_with_focus(
        &self,
        prompt: &str,
        focus: Option<&Path>,
    ) -> Result<RunState, RuntimeError> {
        let run_id = RunId::from_uuid(self.ids.next_uuid_v7());
        self.run_with_id(run_id, prompt, focus).await
    }

    /// Starts a run with an authority-service allocated identifier.
    pub async fn run_with_id(
        &self,
        run_id: RunId,
        prompt: &str,
        focus: Option<&Path>,
    ) -> Result<RunState, RuntimeError> {
        let now = now_ms();
        let queued = self.engine.create_run(run_id, now).map_err(engine)?;
        let lease = self
            .engine
            .acquire_lease(&format!("agent-{run_id}"), now, self.authority_ttl())
            .map_err(engine)?;
        let running = self.transition(
            &queued,
            Transition::Start,
            RuntimeEvent::StateChanged {
                status: latte_core::RunStatus::Running,
            },
            &lease,
        )?;
        let context = context::build(&self.root, focus, 64 * 1024)
            .map_err(|e| RuntimeError::Engine(e.to_string()))?;
        let checkpoint = Checkpoint {
            messages: vec![
                Message::System {
                    content: context.text,
                },
                Message::User {
                    content: prompt.into(),
                },
            ],
            pending: None,
            final_message: None,
            baseline: self.engine.workspace_manifest().map_err(engine)?,
            tool_queue: Vec::new(),
            tool_cursor: 0,
            pending_input: None,
        };
        self.drive(running, checkpoint, &lease).await
    }
    #[allow(clippy::too_many_lines)]
    pub async fn resume(&self, run_id: RunId, allow: bool) -> Result<RunState, RuntimeError> {
        let state = self.engine.show(run_id).map_err(engine)?;
        let payload = self
            .engine
            .runtime_checkpoint(run_id)
            .map_err(engine)?
            .ok_or(RuntimeError::NotWaiting)?;
        let checkpoint: Checkpoint =
            serde_json::from_str(&payload).map_err(|e| RuntimeError::Engine(e.to_string()))?;
        let (mut checkpoint, normalized) = match validate_and_normalize(checkpoint, &state)? {
            CheckpointDisposition::Ready {
                checkpoint,
                normalized,
            } => (*checkpoint, normalized),
            CheckpointDisposition::RequiresUnknown { effect_id } => {
                if self
                    .engine
                    .unknown_effects_for_run(run_id)
                    .map_err(engine)?
                    .contains(&effect_id)
                {
                    return Err(RuntimeError::UnknownEffect { effect_id });
                }
                return Err(RuntimeError::CheckpointInvalid(
                    "pending effect is not unknown".into(),
                ));
            }
        };
        if state.status == latte_core::RunStatus::Interrupted {
            if let Some(effect_id) = self
                .engine
                .unknown_effects_for_run(run_id)
                .map_err(engine)?
                .into_iter()
                .next()
            {
                return Err(RuntimeError::UnknownEffect { effect_id });
            }
            let lease = self
                .engine
                .acquire_lease(&format!("agent-{run_id}"), now_ms(), self.authority_ttl())
                .map_err(engine)?;
            if normalized {
                self.persist(run_id, &checkpoint, &lease)?;
            }
            let queued = self.transition(
                &state,
                Transition::Resume,
                RuntimeEvent::StateChanged {
                    status: latte_core::RunStatus::Queued,
                },
                &lease,
            )?;
            let running = self.transition(
                &queued,
                Transition::Start,
                RuntimeEvent::StateChanged {
                    status: latte_core::RunStatus::Running,
                },
                &lease,
            )?;
            if checkpoint.final_message.is_some()
                && checkpoint.pending.is_none()
                && checkpoint.pending_input.is_none()
            {
                let call = self.verification_call(run_id);
                checkpoint.pending = Some(PendingCall {
                    effect_id: call.id.clone(),
                    call: call.clone(),
                    operation_digest: String::new(),
                    phase: PendingPhase::Verification,
                });
                self.persist(run_id, &checkpoint, &lease)?;
                match self.verify(run_id, &running, &lease).await {
                    Ok(output) => {
                        checkpoint.pending = None;
                        self.persist(run_id, &checkpoint, &lease)?;
                        return self.finish_verified(&running, &checkpoint, &output, &lease);
                    }
                    Err(RuntimeError::Engine(message)) if message.starts_with("permission:") => {
                        let digest = message.trim_start_matches("permission:").to_owned();
                        checkpoint.pending = Some(PendingCall {
                            effect_id: call.id.clone(),
                            call: call.clone(),
                            operation_digest: digest.clone(),
                            phase: PendingPhase::Verification,
                        });
                        self.persist(run_id, &checkpoint, &lease)?;
                        let waiting = self.transition(
                            &running,
                            Transition::RequestPermission(PendingPermission {
                                request_id: call.id,
                                operation_digest: digest,
                                description: "allow verification command".into(),
                            }),
                            RuntimeEvent::StateChanged {
                                status: latte_core::RunStatus::WaitingPermission,
                            },
                            &lease,
                        )?;
                        return Err(RuntimeError::PermissionRequired {
                            run_id: waiting.run_id,
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
            return self.drive(running, checkpoint, &lease).await;
        }
        let mut pending = checkpoint.pending.take().ok_or(RuntimeError::NotWaiting)?;
        let request = state
            .pending_permission
            .as_ref()
            .ok_or(RuntimeError::NotWaiting)?;
        let now = now_ms();
        let lease = self
            .engine
            .acquire_lease(&format!("agent-{run_id}"), now, self.authority_ttl())
            .map_err(engine)?;
        if allow
            && !self
                .engine
                .permission_matches(
                    &pending.effect_id,
                    run_id,
                    state.revision.saturating_add(1),
                    &lease,
                    &pending.operation_digest,
                    now,
                )
                .map_err(engine)?
        {
            let old_id = pending.effect_id.clone();
            pending.effect_id = format!("{}-lease-{}", pending.call.id, lease.fencing_token());
            let rebound = self.transition(
                &state,
                Transition::RefreshPermission(PendingPermission {
                    request_id: pending.effect_id.clone(),
                    operation_digest: pending.operation_digest.clone(),
                    description: format!("reissue {} under fresh lease", pending.call.name),
                }),
                RuntimeEvent::StateChanged {
                    status: latte_core::RunStatus::WaitingPermission,
                },
                &lease,
            )?;
            let mut effect_call = pending.call.clone();
            effect_call.id.clone_from(&pending.effect_id);
            let digest =
                self.reissue_pending(&old_id, run_id, &rebound, &lease, &effect_call, now)?;
            let refreshed = self.transition(
                &rebound,
                Transition::RefreshPermission(PendingPermission {
                    request_id: pending.effect_id.clone(),
                    operation_digest: digest.clone(),
                    description: format!("allow {}", pending.call.name),
                }),
                RuntimeEvent::StateChanged {
                    status: latte_core::RunStatus::WaitingPermission,
                },
                &lease,
            )?;
            pending.operation_digest = digest;
            checkpoint.pending = Some(pending);
            self.persist(run_id, &checkpoint, &lease)?;
            return Err(RuntimeError::PermissionRequired {
                run_id: refreshed.run_id,
            });
        }
        let running = self.transition(
            &state,
            Transition::ResolvePermission {
                request_id: request.request_id.clone(),
                allowed: allow,
            },
            RuntimeEvent::StateChanged {
                status: if allow {
                    latte_core::RunStatus::Running
                } else {
                    latte_core::RunStatus::Failed
                },
            },
            &lease,
        )?;
        if !allow {
            return Ok(running);
        }
        let mut effect_call = pending.call.clone();
        effect_call.id.clone_from(&pending.effect_id);
        let output = if pending.phase == PendingPhase::Verification {
            self.invoke_verification(
                run_id,
                &running,
                &lease,
                &effect_call,
                Some(&pending.operation_digest),
            )
            .await?
        } else {
            self.invoke(
                run_id,
                &running,
                &lease,
                &effect_call,
                Some(&pending.operation_digest),
            )
            .await?
        };
        if pending.phase == PendingPhase::Verification {
            return self.finish_verified(&running, &checkpoint, &output, &lease);
        }
        let result_call = checkpoint
            .tool_queue
            .get(checkpoint.tool_cursor)
            .cloned()
            .unwrap_or(pending.call);
        checkpoint.messages.push(Message::Tool {
            tool_call_id: result_call.id,
            name: Some(result_call.name),
            content: output,
        });
        checkpoint.tool_cursor = checkpoint.tool_cursor.saturating_add(1);
        self.persist(run_id, &checkpoint, &lease)?;
        self.drive(running, checkpoint, &lease).await
    }

    pub async fn provide_input(
        &self,
        run_id: RunId,
        request_id: &str,
        value: &str,
    ) -> Result<RunState, RuntimeError> {
        if value.is_empty() || value.len() > 16 * 1024 {
            return Err(RuntimeError::Engine("input must be 1..=16384 bytes".into()));
        }
        let state = self.engine.show(run_id).map_err(engine)?;
        let payload = self
            .engine
            .runtime_checkpoint(run_id)
            .map_err(engine)?
            .ok_or(RuntimeError::NotWaiting)?;
        let checkpoint: Checkpoint =
            serde_json::from_str(&payload).map_err(|e| RuntimeError::Engine(e.to_string()))?;
        let (mut checkpoint, normalized) = match validate_and_normalize(checkpoint, &state)? {
            CheckpointDisposition::Ready {
                checkpoint,
                normalized,
            } => (*checkpoint, normalized),
            CheckpointDisposition::RequiresUnknown { .. } => {
                return Err(RuntimeError::CheckpointInvalid(
                    "input checkpoint requires effect reconciliation".into(),
                ));
            }
        };
        if normalized {
            return Err(RuntimeError::CheckpointInvalid(
                "input wait cannot normalize a resolved tool queue".into(),
            ));
        }
        let pending = checkpoint
            .pending_input
            .take()
            .ok_or(RuntimeError::NotWaiting)?;
        if state.pending_input.as_ref().map(|r| r.request_id.as_str()) != Some(request_id)
            || pending.id != request_id
        {
            return Err(RuntimeError::NotWaiting);
        }
        let lease = self
            .engine
            .acquire_lease(&format!("agent-{run_id}"), now_ms(), self.authority_ttl())
            .map_err(engine)?;
        let running = self.transition(
            &state,
            Transition::ProvideInput {
                request_id: request_id.into(),
            },
            RuntimeEvent::StateChanged {
                status: latte_core::RunStatus::Running,
            },
            &lease,
        )?;
        checkpoint.messages.push(Message::User {
            content: value.into(),
        });
        self.persist(run_id, &checkpoint, &lease)?;
        self.drive(running, checkpoint, &lease).await
    }
    #[allow(clippy::too_many_lines)]
    async fn drive(
        &self,
        state: RunState,
        mut checkpoint: Checkpoint,
        lease: &Lease,
    ) -> Result<RunState, RuntimeError> {
        for _ in 0..32 {
            if self.cancellation.is_cancelled() {
                let cancelling = self.transition(
                    &state,
                    Transition::Cancel,
                    RuntimeEvent::StateChanged {
                        status: latte_core::RunStatus::Cancelling,
                    },
                    lease,
                )?;
                return self.transition(
                    &cancelling,
                    Transition::Interrupt,
                    RuntimeEvent::StateChanged {
                        status: latte_core::RunStatus::Interrupted,
                    },
                    lease,
                );
            }
            if checkpoint.tool_cursor < checkpoint.tool_queue.len() {
                let call = checkpoint.tool_queue[checkpoint.tool_cursor].clone();
                checkpoint.pending = Some(PendingCall {
                    call: call.clone(),
                    effect_id: call.id.clone(),
                    operation_digest: String::new(),
                    phase: PendingPhase::Tool,
                });
                self.persist(state.run_id, &checkpoint, lease)?;
                match self.invoke(state.run_id, &state, lease, &call, None).await {
                    Ok(output) => {
                        checkpoint.pending = None;
                        checkpoint.messages.push(Message::Tool {
                            tool_call_id: call.id,
                            name: Some(call.name),
                            content: output,
                        });
                        checkpoint.tool_cursor = checkpoint.tool_cursor.saturating_add(1);
                        self.persist(state.run_id, &checkpoint, lease)?;
                        continue;
                    }
                    Err(RuntimeError::Engine(message)) if message.starts_with("permission:") => {
                        let digest = message.trim_start_matches("permission:").to_owned();
                        checkpoint.pending = Some(PendingCall {
                            call: call.clone(),
                            effect_id: call.id.clone(),
                            operation_digest: digest.clone(),
                            phase: PendingPhase::Tool,
                        });
                        self.persist(state.run_id, &checkpoint, lease)?;
                        let waiting = self.transition(
                            &state,
                            Transition::RequestPermission(PendingPermission {
                                request_id: call.id.clone(),
                                operation_digest: digest,
                                description: format!("allow {}", call.name),
                            }),
                            RuntimeEvent::StateChanged {
                                status: latte_core::RunStatus::WaitingPermission,
                            },
                            lease,
                        )?;
                        return Err(RuntimeError::PermissionRequired {
                            run_id: waiting.run_id,
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
            if !checkpoint.tool_queue.is_empty() {
                checkpoint.tool_queue.clear();
                checkpoint.tool_cursor = 0;
                self.persist(state.run_id, &checkpoint, lease)?;
            }
            self.persist(state.run_id, &checkpoint, lease)?;
            let response = {
                let tools = self.engine.tool_descriptors();
                let completion = self.provider.complete(&checkpoint.messages, &tools);
                tokio::pin!(completion);
                let heartbeat = tokio::time::sleep(std::time::Duration::from_millis(
                    (self.lease_ttl_ms / 3).max(1),
                ));
                tokio::pin!(heartbeat);
                loop {
                    tokio::select! {
                        response = &mut completion => break response,
                        () = &mut heartbeat => {
                        if self.engine.renew_lease(lease, now_ms(), self.authority_ttl()).is_err() {
                            return Err(self.recover_lease_loss(state.run_id, state.revision, lease, "provider call"));
                            }
                            heartbeat.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_millis((self.lease_ttl_ms / 3).max(1)));
                        }
                        () = self.cancellation.cancelled() => {
                            let cancelling = self.transition(&state, Transition::Cancel, RuntimeEvent::StateChanged { status: latte_core::RunStatus::Cancelling }, lease)?;
                            return self.transition(&cancelling, Transition::Interrupt, RuntimeEvent::StateChanged { status: latte_core::RunStatus::Interrupted }, lease);
                        }
                    }
                }
            };
            let response = match response {
                Ok(value) => value,
                Err(error) => return self.fail(&state, error.to_string(), lease),
            };
            let mut call_ids = std::collections::BTreeSet::new();
            if response
                .tool_calls
                .iter()
                .any(|call| !valid_tool_call_id(&call.id) || !call_ids.insert(call.id.clone()))
            {
                return self.fail(
                    &state,
                    "provider tool call ids must be nonempty and unique".into(),
                    lease,
                );
            }
            checkpoint.messages.push(Message::Assistant {
                content: response.message.clone(),
                tool_calls: response.tool_calls.clone(),
            });
            if response.tool_calls.is_empty() {
                checkpoint.tool_queue.clear();
                checkpoint.tool_cursor = 0;
            }
            if let Some(input) = response.input_request {
                if input.secret {
                    return self.fail(
                        &state,
                        RuntimeError::SecretInputUnsupported.to_string(),
                        lease,
                    );
                }
                if input.id.is_empty()
                    || input.prompt.is_empty()
                    || input.id.len() > 256
                    || input.prompt.len() > 4096
                {
                    return self.fail(&state, "invalid provider input request".into(), lease);
                }
                checkpoint.pending_input = Some(input.clone());
                self.persist(state.run_id, &checkpoint, lease)?;
                let waiting = self.transition(
                    &state,
                    Transition::RequestInput(latte_core::PendingInput {
                        request_id: input.id,
                        prompt: input.prompt,
                    }),
                    RuntimeEvent::StateChanged {
                        status: latte_core::RunStatus::WaitingInput,
                    },
                    lease,
                )?;
                return Err(RuntimeError::InputRequired {
                    run_id: waiting.run_id,
                });
            }
            if response.tool_calls.is_empty() {
                checkpoint.final_message = response.message;
                let verification_call = self.verification_call(state.run_id);
                checkpoint.pending = Some(PendingCall {
                    effect_id: verification_call.id.clone(),
                    call: verification_call,
                    operation_digest: String::new(),
                    phase: PendingPhase::Verification,
                });
                self.persist(state.run_id, &checkpoint, lease)?;
                match self.verify(state.run_id, &state, lease).await {
                    Ok(output) => {
                        checkpoint.pending = None;
                        self.persist(state.run_id, &checkpoint, lease)?;
                        return self.finish_verified(&state, &checkpoint, &output, lease);
                    }
                    Err(RuntimeError::Engine(message)) if message.starts_with("permission:") => {
                        let digest = message.trim_start_matches("permission:").to_owned();
                        let call = self.verification_call(state.run_id);
                        checkpoint.pending = Some(PendingCall {
                            effect_id: call.id.clone(),
                            call: call.clone(),
                            operation_digest: digest.clone(),
                            phase: PendingPhase::Verification,
                        });
                        self.persist(state.run_id, &checkpoint, lease)?;
                        let waiting = self.transition(
                            &state,
                            Transition::RequestPermission(PendingPermission {
                                request_id: call.id.clone(),
                                operation_digest: digest,
                                description: "allow verification command".into(),
                            }),
                            RuntimeEvent::StateChanged {
                                status: latte_core::RunStatus::WaitingPermission,
                            },
                            lease,
                        )?;
                        return Err(RuntimeError::PermissionRequired {
                            run_id: waiting.run_id,
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
            checkpoint.tool_queue = response.tool_calls;
            checkpoint.tool_cursor = 0;
            checkpoint.pending = None;
            self.persist(state.run_id, &checkpoint, lease)?;
        }
        self.fail(&state, "agent step limit exceeded".into(), lease)
    }
    async fn invoke(
        &self,
        run_id: RunId,
        state: &RunState,
        lease: &Lease,
        call: &ToolCall,
        approval: Option<&str>,
    ) -> Result<String, RuntimeError> {
        self.ensure_authority(lease)?;
        if call.name == "process" {
            return self
                .invoke_process(run_id, state, lease, call, approval)
                .await;
        }
        let precondition = call.input.get("precondition").and_then(|v| v.as_str());
        let invocation = ToolInvocation {
            name: &call.name,
            input: &call.input,
            run_revision: if approval.is_none()
                && matches!(call.name.as_str(), "edit_file" | "write_file")
            {
                state.revision.saturating_add(2)
            } else {
                state.revision
            },
            effect_id: &call.id,
            attempt: 1,
            precondition,
            timeout_ms: 30_000,
            output_cap: 64 * 1024,
            approval_digest: approval,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        match self
            .engine
            .execute_tool(run_id, lease, now_ms(), &invocation)
        {
            Ok(output) => Ok(output.value.to_string()),
            Err(ToolError::PermissionRequired { digest, .. }) => {
                Err(RuntimeError::Engine(format!("permission:{digest}")))
            }
            Err(error) => Err(RuntimeError::Engine(error.to_string())),
        }
    }
    async fn invoke_process(
        &self,
        run_id: RunId,
        state: &RunState,
        lease: &Lease,
        call: &ToolCall,
        approval: Option<&str>,
    ) -> Result<String, RuntimeError> {
        self.ensure_authority(lease)?;
        let argv = call
            .input
            .get("argv")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let shell = call.input.get("shell").and_then(|v| v.as_str());
        let cwd = call
            .input
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let env = call
            .input
            .get("env")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| RuntimeError::Engine(e.to_string()))?
            .unwrap_or_default();
        let mut invocation = ProcessInvocation {
            argv: &argv,
            shell,
            cwd,
            env: &env,
            timeout_ms: call
                .input
                .get("timeout_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(30_000),
            grace_ms: call
                .input
                .get("grace_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(250),
            stdout_cap: call
                .input
                .get("stdout_cap")
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(64 * 1024),
            stderr_cap: call
                .input
                .get("stderr_cap")
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(64 * 1024),
            run_revision: state.revision,
            effect_id: &call.id,
            attempt: 1,
            approval_digest: approval,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        if approval.is_none() && classify(&invocation) == ProcessDecision::Ask {
            invocation.run_revision = state.revision.saturating_add(2)
        }
        let execution =
            self.engine
                .execute_process(run_id, lease, now_ms(), &invocation, &self.cancellation);
        tokio::pin!(execution);
        let heartbeat = tokio::time::sleep(std::time::Duration::from_millis(
            (self.lease_ttl_ms / 3).max(1),
        ));
        tokio::pin!(heartbeat);
        let result = loop {
            tokio::select! {
                result = &mut execution => break result,
                () = &mut heartbeat => {
                if self.engine.renew_lease(lease, now_ms(), self.authority_ttl()).is_err() {
                    self.cancellation.cancel();
                    let _ = execution.await;
                    return Err(self.recover_lease_loss(run_id, state.revision, lease, "process"));
                    }
                    heartbeat.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_millis((self.lease_ttl_ms / 3).max(1)));
                }
            }
        };
        match result {
            Ok(output) => {
                serde_json::to_string(&output).map_err(|e| RuntimeError::Engine(e.to_string()))
            }
            Err(ProcessError::PermissionRequired { digest }) => {
                Err(RuntimeError::Engine(format!("permission:{digest}")))
            }
            Err(error) => Err(RuntimeError::Engine(error.to_string())),
        }
    }
    fn verification_call(&self, run_id: RunId) -> ToolCall {
        ToolCall {
            id: format!("verify-{run_id}"),
            name: "process".into(),
            input: serde_json::json!({
                "argv": self.verification.argv,
                "cwd": self.verification.cwd,
                "timeout_ms": self.verification.timeout_ms,
                "grace_ms": self.verification.grace_ms,
                "stdout_cap": self.verification.stdout_cap,
                "stderr_cap": self.verification.stderr_cap,
                "env": {},
            }),
        }
    }
    fn recover_lease_loss(
        &self,
        run_id: RunId,
        revision: u64,
        lease: &Lease,
        phase: &str,
    ) -> RuntimeError {
        match self
            .engine
            .interrupt_after_lease_loss(run_id, lease, revision, now_ms())
        {
            Ok(latte_engine::LeaseLossRecovery::Interrupted(_)) => RuntimeError::Engine(format!(
                "lease heartbeat lost during {phase}; run interrupted"
            )),
            Ok(latte_engine::LeaseLossRecovery::FencedNoop) => RuntimeError::Engine(format!(
                "lease heartbeat lost during {phase}; newer owner fenced stale recovery"
            )),
            Ok(latte_engine::LeaseLossRecovery::AlreadyTerminal(_)) => RuntimeError::Engine(
                format!("lease heartbeat lost during {phase}; run already terminal"),
            ),
            Err(error) => RuntimeError::Engine(format!(
                "lease heartbeat lost during {phase}; reconciliation required because recovery failed: {error}"
            )),
        }
    }
    const fn authority_ttl(&self) -> u64 {
        if self.lease_ttl_ms < 50 {
            50
        } else {
            self.lease_ttl_ms
        }
    }
    fn ensure_authority(&self, lease: &Lease) -> Result<(), RuntimeError> {
        self.engine
            .renew_lease(lease, now_ms(), self.authority_ttl())
            .map(|_| ())
            .map_err(engine)
    }
    fn reissue_pending(
        &self,
        old_effect_id: &str,
        run_id: RunId,
        state: &RunState,
        lease: &Lease,
        call: &ToolCall,
        now: u64,
    ) -> Result<String, RuntimeError> {
        let run_revision = state.revision.saturating_add(2);
        if call.name == "process" {
            let argv = call
                .input
                .get("argv")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let env = call
                .input
                .get("env")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(engine)?
                .unwrap_or_default();
            let invocation = ProcessInvocation {
                argv: &argv,
                shell: call.input.get("shell").and_then(|v| v.as_str()),
                cwd: call
                    .input
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or("."),
                env: &env,
                timeout_ms: call
                    .input
                    .get("timeout_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(30_000),
                grace_ms: call
                    .input
                    .get("grace_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(250),
                stdout_cap: call
                    .input
                    .get("stdout_cap")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(64 * 1024),
                stderr_cap: call
                    .input
                    .get("stderr_cap")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(64 * 1024),
                run_revision,
                effect_id: &call.id,
                attempt: 1,
                approval_digest: None,
                lease_owner: lease.owner(),
                lease_token: lease.fencing_token(),
            };
            return self
                .engine
                .reissue_process_permission(old_effect_id, run_id, lease, now, &invocation)
                .map_err(engine);
        }
        let invocation = ToolInvocation {
            name: &call.name,
            input: &call.input,
            run_revision,
            effect_id: &call.id,
            attempt: 1,
            precondition: call.input.get("precondition").and_then(|v| v.as_str()),
            timeout_ms: 30_000,
            output_cap: 64 * 1024,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        self.engine
            .reissue_tool_permission(old_effect_id, run_id, lease, now, &invocation)
            .map_err(engine)
    }
    async fn verify(
        &self,
        run_id: RunId,
        state: &RunState,
        lease: &Lease,
    ) -> Result<String, RuntimeError> {
        self.invoke_verification(run_id, state, lease, &self.verification_call(run_id), None)
            .await
    }
    async fn invoke_verification(
        &self,
        run_id: RunId,
        state: &RunState,
        lease: &Lease,
        call: &ToolCall,
        approval: Option<&str>,
    ) -> Result<String, RuntimeError> {
        self.ensure_authority(lease)?;
        let env = std::collections::BTreeMap::new();
        let mut invocation = ProcessInvocation {
            argv: &self.verification.argv,
            shell: None,
            cwd: &self.verification.cwd,
            env: &env,
            timeout_ms: self.verification.timeout_ms,
            grace_ms: self.verification.grace_ms,
            stdout_cap: self.verification.stdout_cap,
            stderr_cap: self.verification.stderr_cap,
            run_revision: state.revision,
            effect_id: &call.id,
            attempt: 1,
            approval_digest: approval,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        if approval.is_none() && classify(&invocation) == ProcessDecision::Ask {
            invocation.run_revision = state.revision.saturating_add(2);
        }
        match self
            .engine
            .execute_verification(
                run_id,
                invocation.run_revision,
                lease,
                now_ms(),
                &invocation,
                &self.cancellation,
            )
            .await
        {
            Ok(output) => serde_json::to_string(&output).map_err(engine),
            Err(ProcessError::PermissionRequired { digest }) => {
                Err(RuntimeError::Engine(format!("permission:{digest}")))
            }
            Err(error) => Err(engine(error)),
        }
    }
    fn finish_verified(
        &self,
        state: &RunState,
        checkpoint: &Checkpoint,
        output_json: &str,
        lease: &Lease,
    ) -> Result<RunState, RuntimeError> {
        self.ensure_authority(lease)?;
        let output: latte_engine::ProcessOutput = serde_json::from_str(output_json)
            .map_err(|e| RuntimeError::Engine(format!("invalid verification result: {e}")))?;
        let status = if output.command_succeeded() {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        };
        if status != VerificationStatus::Passed {
            return self
                .fail(state, "verification failed".into(), lease)
                .and_then(|_| Err(RuntimeError::Engine("verification failed".into())));
        }
        let summary = checkpoint.final_message.clone().ok_or_else(|| {
            RuntimeError::Engine("provider final response content was null".into())
        })?;
        self.engine
            .complete_verified_run(state.run_id, state.revision, lease, summary, now_ms())
            .map_err(engine)
    }
    fn fail(
        &self,
        state: &RunState,
        message: String,
        lease: &Lease,
    ) -> Result<RunState, RuntimeError> {
        let failure = RunFailure {
            code: FailureCode::RuntimeFailed,
            message,
            retryability: Retryability::Terminal,
        };
        self.transition(
            state,
            Transition::Fail(failure),
            RuntimeEvent::StateChanged {
                status: latte_core::RunStatus::Failed,
            },
            lease,
        )
    }
    #[allow(clippy::needless_pass_by_value)]
    fn transition(
        &self,
        state: &RunState,
        transition: Transition,
        _event: RuntimeEvent,
        lease: &Lease,
    ) -> Result<RunState, RuntimeError> {
        self.ensure_authority(lease)?;
        self.engine
            .apply_transition(state.run_id, state.revision, transition, now_ms(), lease)
            .map_err(engine)
    }
    fn persist(
        &self,
        run: RunId,
        checkpoint: &Checkpoint,
        lease: &Lease,
    ) -> Result<(), RuntimeError> {
        self.ensure_authority(lease)?;
        let expected_revision = self.engine.show(run).map_err(engine)?.revision;
        self.engine
            .persist_runtime_checkpoint(
                run,
                expected_revision,
                lease,
                &serde_json::to_string(checkpoint).unwrap(),
                now_ms(),
            )
            .map_err(engine)
    }
}
fn engine(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Engine(error.to_string())
}
fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderResponse;
    use serde_json::json;

    #[test]
    #[allow(
        clippy::too_many_lines,
        clippy::default_trait_access,
        clippy::manual_string_new
    )]
    fn checkpoint_validation_rejects_corrupt_relationships_before_execution() {
        let call = ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            input: json!({"path":"a"}),
        };
        let base = Checkpoint {
            messages: vec![Message::Assistant {
                content: None,
                tool_calls: vec![call.clone()],
            }],
            pending: Some(PendingCall {
                call: call.clone(),
                effect_id: call.id.clone(),
                operation_digest: String::new(),
                phase: PendingPhase::Tool,
            }),
            final_message: None,
            baseline: Default::default(),
            tool_queue: vec![call.clone()],
            tool_cursor: 0,
            pending_input: None,
        };
        assert!(validate_checkpoint(&base).is_ok());
        for interrupt in [
            Message::User {
                content: "interrupt".into(),
            },
            Message::System {
                content: "interrupt".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![],
            },
        ] {
            let mut corrupt = base.clone();
            corrupt.messages.push(interrupt);
            assert!(validate_checkpoint(&corrupt).is_err());
        }
        let mut wrong_order = base.clone();
        wrong_order.messages.push(Message::Tool {
            tool_call_id: "wrong".into(),
            name: Some("read_file".into()),
            content: "x".into(),
        });
        assert!(validate_checkpoint(&wrong_order).is_err());
        let mut wrong_name = base.clone();
        wrong_name.messages.push(Message::Tool {
            tool_call_id: "call-1".into(),
            name: Some("other".into()),
            content: "x".into(),
        });
        assert!(validate_checkpoint(&wrong_name).is_err());
        let mut bad_assistant = base.clone();
        bad_assistant.messages[0] = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: String::new(),
                name: "read_file".into(),
                input: json!({}),
            }],
        };
        assert!(validate_checkpoint(&bad_assistant).is_err());
        let resolved_crash = Checkpoint {
            messages: vec![
                Message::Assistant {
                    content: None,
                    tool_calls: vec![call.clone()],
                },
                Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: Some(call.name.clone()),
                    content: "ok".into(),
                },
            ],
            pending: None,
            final_message: None,
            baseline: Default::default(),
            tool_queue: vec![call.clone()],
            tool_cursor: 1,
            pending_input: None,
        };
        let mut interrupted =
            RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        interrupted.status = latte_core::RunStatus::Interrupted;
        let CheckpointDisposition::Ready {
            checkpoint: normalized,
            normalized: changed,
        } = validate_and_normalize(resolved_crash, &interrupted).unwrap()
        else {
            panic!()
        };
        assert!(changed);
        assert!(normalized.tool_queue.is_empty());
        assert_eq!(normalized.tool_cursor, 0);
        let mut bad_suffix = base.clone();
        bad_suffix.pending.as_mut().unwrap().effect_id = "internal-effect-1".into();
        assert!(validate_checkpoint(&bad_suffix).is_ok());
        let mut waiting =
            RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        waiting.status = latte_core::RunStatus::WaitingPermission;
        assert!(validate_and_normalize(bad_suffix.clone(), &waiting).is_err());
        waiting.pending_permission = Some(PendingPermission {
            request_id: "wrong".into(),
            operation_digest: "wrong".into(),
            description: "test".into(),
        });
        assert!(validate_and_normalize(bad_suffix.clone(), &waiting).is_err());
        let pending = bad_suffix.pending.as_ref().unwrap();
        waiting.pending_permission = Some(PendingPermission {
            request_id: pending.effect_id.clone(),
            operation_digest: "wrong".into(),
            description: "test".into(),
        });
        assert!(validate_and_normalize(bad_suffix.clone(), &waiting).is_err());
        waiting.pending_permission = Some(PendingPermission {
            request_id: pending.effect_id.clone(),
            operation_digest: pending.operation_digest.clone(),
            description: "test".into(),
        });
        assert!(validate_and_normalize(bad_suffix.clone(), &waiting).is_ok());
        waiting.status = latte_core::RunStatus::Interrupted;
        assert!(validate_and_normalize(bad_suffix.clone(), &waiting).is_ok());
        waiting.pending_permission = None;
        assert!(matches!(
            validate_and_normalize(bad_suffix.clone(), &waiting).unwrap(),
            CheckpointDisposition::RequiresUnknown { .. }
        ));
        let final_interrupt = Checkpoint {
            messages: vec![],
            pending: None,
            final_message: Some("done".into()),
            baseline: Default::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: None,
        };
        assert!(matches!(
            validate_and_normalize(final_interrupt, &waiting).unwrap(),
            CheckpointDisposition::Ready { .. }
        ));
        let interrupted_input = Checkpoint {
            messages: vec![],
            pending: None,
            final_message: None,
            baseline: Default::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: Some(InputRequest {
                id: "i".into(),
                prompt: "p".into(),
                secret: false,
            }),
        };
        waiting.pending_input = Some(latte_core::PendingInput {
            request_id: "i".into(),
            prompt: "p".into(),
        });
        assert!(matches!(
            validate_and_normalize(interrupted_input, &waiting).unwrap(),
            CheckpointDisposition::Ready { .. }
        ));
        let interrupted_verification = Checkpoint {
            messages: vec![],
            pending: Some(PendingCall {
                effect_id: "verify-effect".into(),
                call: ToolCall {
                    id: "verify".into(),
                    name: "process".into(),
                    input: json!({}),
                },
                operation_digest: "digest".into(),
                phase: PendingPhase::Verification,
            }),
            final_message: Some("done".into()),
            baseline: Default::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: None,
        };
        waiting.pending_input = None;
        assert!(matches!(
            validate_and_normalize(interrupted_verification, &waiting).unwrap(),
            CheckpointDisposition::RequiresUnknown { .. }
        ));
        waiting.status = latte_core::RunStatus::Running;
        waiting.pending_permission = None;
        assert!(validate_and_normalize(bad_suffix.clone(), &waiting).is_err());
        let mut incompatible = bad_suffix.clone();
        incompatible.final_message = Some("not tool wait".into());
        waiting.status = latte_core::RunStatus::WaitingPermission;
        assert!(validate_and_normalize(incompatible, &waiting).is_err());
        let mut mixed = bad_suffix.clone();
        mixed.pending_input = Some(InputRequest {
            id: "i".into(),
            prompt: "p".into(),
            secret: false,
        });
        assert!(validate_and_normalize(mixed, &waiting).is_err());
        let input_final = Checkpoint {
            messages: vec![],
            pending: None,
            final_message: Some("bad".into()),
            baseline: Default::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: Some(InputRequest {
                id: "i".into(),
                prompt: "p".into(),
                secret: false,
            }),
        };
        let mut input_state =
            RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
        input_state.status = latte_core::RunStatus::WaitingInput;
        assert!(validate_and_normalize(input_final, &input_state).is_err());
        let empty = Checkpoint {
            messages: vec![],
            pending: None,
            final_message: None,
            baseline: Default::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: None,
        };
        for status in [
            latte_core::RunStatus::Queued,
            latte_core::RunStatus::Cancelling,
            latte_core::RunStatus::Completed,
            latte_core::RunStatus::Failed,
        ] {
            let mut state =
                RunState::queued(RunId::from_uuid(SystemIdSource::default().next_uuid_v7()));
            state.status = status;
            assert!(validate_and_normalize(empty.clone(), &state).is_ok());
        }
        let mut corrupt = base.clone();
        corrupt.messages.push(Message::Tool {
            tool_call_id: "orphan".into(),
            name: None,
            content: "x".into(),
        });
        assert!(matches!(
            validate_checkpoint(&corrupt),
            Err(RuntimeError::CheckpointInvalid(_))
        ));
        let mut corrupt = base.clone();
        corrupt.tool_queue[0].id = "bad\n".into();
        assert!(validate_checkpoint(&corrupt).is_err());
        let mut corrupt = base.clone();
        corrupt.pending.as_mut().unwrap().call.id = "mismatch".into();
        assert!(validate_checkpoint(&corrupt).is_err());
        let mut corrupt = base.clone();
        corrupt.messages.push(Message::Assistant {
            content: None,
            tool_calls: vec![call.clone()],
        });
        assert!(validate_checkpoint(&corrupt).is_err());
        let mut corrupt = base;
        corrupt.pending_input = Some(InputRequest {
            id: "".into(),
            prompt: "".into(),
            secret: true,
        });
        assert!(validate_checkpoint(&corrupt).is_err());
        let mut verification = corrupt.clone();
        verification.pending_input = None;
        verification.messages.push(Message::Tool {
            tool_call_id: "call-1".into(),
            name: Some("read_file".into()),
            content: "ok".into(),
        });
        verification.tool_queue.clear();
        verification.tool_cursor = 0;
        verification.pending = Some(PendingCall {
            effect_id: "verify-ok".into(),
            call: ToolCall {
                id: "verify-ok".into(),
                name: "process".into(),
                input: json!({}),
            },
            operation_digest: String::new(),
            phase: PendingPhase::Verification,
        });
        verification.final_message = Some("done".into());
        assert!(validate_checkpoint(&verification).is_ok());
        verification.tool_queue.push(call.clone());
        assert!(validate_checkpoint(&verification).is_err());
        let mut duplicate_result = corrupt;
        duplicate_result.pending_input = None;
        duplicate_result.pending = None;
        duplicate_result.tool_cursor = 1;
        duplicate_result.messages.push(Message::Tool {
            tool_call_id: "call-1".into(),
            name: None,
            content: "one".into(),
        });
        duplicate_result.messages.push(Message::Tool {
            tool_call_id: "call-1".into(),
            name: None,
            content: "two".into(),
        });
        assert!(validate_checkpoint(&duplicate_result).is_err());
        let complete = Checkpoint {
            messages: vec![
                Message::System {
                    content: "s".into(),
                },
                Message::User {
                    content: "u".into(),
                },
                Message::Assistant {
                    content: None,
                    tool_calls: vec![call.clone()],
                },
                Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: Some(call.name.clone()),
                    content: "ok".into(),
                },
            ],
            pending: None,
            final_message: None,
            baseline: Default::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: Some(InputRequest {
                id: "input-1".into(),
                prompt: "answer".into(),
                secret: false,
            }),
        };
        assert!(validate_checkpoint(&complete).is_ok());
        let mut bad_cursor = complete;
        bad_cursor.tool_cursor = 1;
        assert!(validate_checkpoint(&bad_cursor).is_err());
        for input in [
            InputRequest {
                id: "x".repeat(257),
                prompt: "ok".into(),
                secret: false,
            },
            InputRequest {
                id: "ok".into(),
                prompt: String::new(),
                secret: false,
            },
            InputRequest {
                id: "ok".into(),
                prompt: "x".repeat(4097),
                secret: false,
            },
            InputRequest {
                id: "ok".into(),
                prompt: "ok".into(),
                secret: true,
            },
        ] {
            let mut invalid = bad_cursor.clone();
            invalid.tool_cursor = 0;
            invalid.pending_input = Some(input);
            assert!(validate_checkpoint(&invalid).is_err());
        }
        let mut invalid_pending = bad_cursor;
        invalid_pending.tool_cursor = 0;
        invalid_pending.pending = Some(PendingCall {
            effect_id: String::new(),
            call: ToolCall {
                id: String::new(),
                name: "process".into(),
                input: json!({}),
            },
            operation_digest: String::new(),
            phase: PendingPhase::Verification,
        });
        assert!(validate_checkpoint(&invalid_pending).is_err());
    }

    #[tokio::test]
    async fn final_only_interrupted_resumes_verification_without_provider() {
        struct PanicProvider;
        impl Provider for PanicProvider {
            async fn complete(
                &self,
                _: &[Message],
                _: &[latte_engine::ToolDescriptor],
            ) -> Result<crate::provider::ProviderResponse, ProviderError> {
                panic!("provider must not be called for final-only interrupted recovery")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("setup", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 3, &lease)
            .unwrap();
        let cancelling = engine
            .apply_transition(run, running.revision, Transition::Cancel, 4, &lease)
            .unwrap();
        let interrupted = engine
            .apply_transition(run, cancelling.revision, Transition::Interrupt, 5, &lease)
            .unwrap();
        let checkpoint = Checkpoint {
            messages: vec![],
            pending: None,
            final_message: Some("done".into()),
            baseline: std::collections::BTreeMap::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: None,
        };
        engine
            .persist_runtime_checkpoint(
                run,
                interrupted.revision,
                &lease,
                &serde_json::to_string(&checkpoint).unwrap(),
                6,
            )
            .unwrap();
        engine.release_lease(&lease).unwrap();
        let runtime = AgentRuntime::new(
            engine,
            PanicProvider,
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                cwd: ".".into(),
                timeout_ms: 1000,
                grace_ms: 10,
                stdout_cap: 1024,
                stderr_cap: 1024,
            },
        );
        let completed = runtime.resume(run, true).await.unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(completed.handoff.unwrap().summary, "done");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines, clippy::items_after_statements)]
    async fn final_only_interrupted_verifier_ask_becomes_verification_permission() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("setup", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 3, &lease)
            .unwrap();
        let cancelling = engine
            .apply_transition(run, running.revision, Transition::Cancel, 4, &lease)
            .unwrap();
        let interrupted = engine
            .apply_transition(run, cancelling.revision, Transition::Interrupt, 5, &lease)
            .unwrap();
        let checkpoint = Checkpoint {
            messages: vec![],
            pending: None,
            final_message: Some("done".into()),
            baseline: std::collections::BTreeMap::default(),
            tool_queue: vec![],
            tool_cursor: 0,
            pending_input: None,
        };
        engine
            .persist_runtime_checkpoint(
                run,
                interrupted.revision,
                &lease,
                &serde_json::to_string(&checkpoint).unwrap(),
                6,
            )
            .unwrap();
        engine.release_lease(&lease).unwrap();
        let runtime = AgentRuntime::new(
            engine,
            crate::provider::FakeProvider::scripted([]),
            dir.path(),
            VerificationPlan {
                argv: vec!["/usr/bin/env".into()],
                cwd: ".".into(),
                timeout_ms: 1000,
                grace_ms: 10,
                stdout_cap: 1024,
                stderr_cap: 1024,
            },
        );
        assert!(matches!(
            runtime.resume(run, true).await,
            Err(RuntimeError::PermissionRequired { .. })
        ));
        let waiting = runtime.engine.show(run).unwrap();
        assert_eq!(waiting.status, latte_core::RunStatus::WaitingPermission);
        assert!(
            waiting
                .pending_permission
                .unwrap()
                .description
                .contains("verification")
        );
        let payload = runtime.engine.runtime_checkpoint(run).unwrap().unwrap();
        let mut corrupt: Checkpoint = serde_json::from_str(&payload).unwrap();
        corrupt.final_message = None;
        drop(runtime);
        let connection = rusqlite::Connection::open(&db).unwrap();
        let before_effects: i64 = connection
            .query_row("SELECT COUNT(*) FROM effects", [], |row| row.get(0))
            .unwrap();
        connection
            .execute(
                "UPDATE runtime_checkpoints SET payload_json=?1 WHERE run_id=?2",
                rusqlite::params![serde_json::to_string(&corrupt).unwrap(), run.to_string()],
            )
            .unwrap();
        connection.execute("DELETE FROM runtime_lease", []).unwrap();
        drop(connection);
        struct PanicProvider;
        impl Provider for PanicProvider {
            async fn complete(
                &self,
                _: &[Message],
                _: &[latte_engine::ToolDescriptor],
            ) -> Result<crate::provider::ProviderResponse, ProviderError> {
                panic!("provider called for corrupt checkpoint")
            }
        }
        let reopened = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let runtime = AgentRuntime::new(
            reopened,
            PanicProvider,
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                cwd: ".".into(),
                timeout_ms: 1000,
                grace_ms: 10,
                stdout_cap: 1024,
                stderr_cap: 1024,
            },
        );
        assert!(matches!(
            runtime.resume(run, true).await,
            Err(RuntimeError::CheckpointInvalid(_))
        ));
        let connection = rusqlite::Connection::open(&db).unwrap();
        let after_effects: i64 = connection
            .query_row("SELECT COUNT(*) FROM effects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before_effects, after_effects);
    }

    #[tokio::test]
    async fn interrupted_resolved_crash_queue_normalizes_before_single_provider_turn() {
        struct OneProvider(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Provider for OneProvider {
            async fn complete(
                &self,
                _: &[Message],
                _: &[latte_engine::ToolDescriptor],
            ) -> Result<crate::provider::ProviderResponse, ProviderError> {
                assert_eq!(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
                Ok(crate::provider::ProviderResponse {
                    message: Some("done".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run, 1).unwrap();
        std::fs::write(dir.path().join("prior.txt"), "once").unwrap();
        let lease = engine.acquire_lease("setup", 2, 100).unwrap();
        let running = engine
            .apply_transition(run, 0, Transition::Start, 3, &lease)
            .unwrap();
        let cancelling = engine
            .apply_transition(run, running.revision, Transition::Cancel, 4, &lease)
            .unwrap();
        let interrupted = engine
            .apply_transition(run, cancelling.revision, Transition::Interrupt, 5, &lease)
            .unwrap();
        let call = ToolCall {
            id: "prior-call".into(),
            name: "write_file".into(),
            input: json!({}),
        };
        let checkpoint = Checkpoint {
            messages: vec![
                Message::Assistant {
                    content: None,
                    tool_calls: vec![call.clone()],
                },
                Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: Some(call.name.clone()),
                    content: "ok".into(),
                },
            ],
            pending: None,
            final_message: None,
            baseline: std::collections::BTreeMap::default(),
            tool_queue: vec![call],
            tool_cursor: 1,
            pending_input: None,
        };
        engine
            .persist_runtime_checkpoint(
                run,
                interrupted.revision,
                &lease,
                &serde_json::to_string(&checkpoint).unwrap(),
                6,
            )
            .unwrap();
        engine.release_lease(&lease).unwrap();
        drop(engine);
        let reopened = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runtime = AgentRuntime::new(
            reopened.clone(),
            OneProvider(std::sync::Arc::clone(&calls)),
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                cwd: ".".into(),
                timeout_ms: 1000,
                grace_ms: 10,
                stdout_cap: 1024,
                stderr_cap: 1024,
            },
        );
        let completed = runtime.resume(run, true).await.unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prior.txt")).unwrap(),
            "once"
        );
        let stored: Checkpoint =
            serde_json::from_str(&reopened.runtime_checkpoint(run).unwrap().unwrap()).unwrap();
        assert!(stored.tool_queue.is_empty());
        assert_eq!(stored.tool_cursor, 0);
    }
    use std::io::{Read, Write};
    use std::sync::Mutex;
    #[derive(Default)]
    struct EditingProvider {
        step: Mutex<u8>,
    }
    #[derive(Default)]
    struct StatelessProvider;
    impl Provider for StatelessProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            if messages.iter().any(|message| message.is_role("tool")) {
                Ok(ProviderResponse {
                    message: Some("created fresh.txt".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            } else {
                Ok(ProviderResponse {
                    message: Some("create".into()),
                    tool_calls: vec![ToolCall {
                        id: "create-1".into(),
                        name: "write_file".into(),
                        input: json!({"path":"fresh.txt","content":"durable","create_intent":true}),
                    }],
                    input_request: None,
                })
            }
        }
    }
    struct FinalProvider;
    impl Provider for FinalProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            Ok(ProviderResponse {
                message: Some("ready to verify".into()),
                tool_calls: vec![],
                input_request: None,
            })
        }
    }
    struct EnvProcessProvider;
    impl Provider for EnvProcessProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            if messages.iter().any(|message| message.is_role("tool")) {
                Ok(ProviderResponse {
                    message: Some("env checked".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            } else {
                Ok(ProviderResponse {
                    message: Some("check env".into()),
                    tool_calls: vec![ToolCall {
                        id: "env-process".into(),
                        name: "process".into(),
                        input: json!({"argv":["/usr/bin/env"],"cwd":".","env":{"LATTE_EXACT":"bound"},"timeout_ms":1234,"grace_ms":111,"stdout_cap":4096,"stderr_cap":2048}),
                    }],
                    input_request: None,
                })
            }
        }
    }
    struct BatchProvider;
    impl Provider for BatchProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            if messages
                .iter()
                .filter(|message| message.is_role("tool"))
                .count()
                == 3
            {
                Ok(ProviderResponse {
                    message: Some("batch complete".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            } else {
                Ok(ProviderResponse {
                    message: Some("three calls".into()),
                    tool_calls: vec![
                        ToolCall {
                            id: "batch-read".into(),
                            name: "read_file".into(),
                            input: json!({"path":"seed.txt"}),
                        },
                        ToolCall {
                            id: "batch-write".into(),
                            name: "write_file".into(),
                            input: json!({"path":"batch.txt","content":"once","create_intent":true}),
                        },
                        ToolCall {
                            id: "batch-list".into(),
                            name: "list_directory".into(),
                            input: json!({"path":"."}),
                        },
                    ],
                    input_request: None,
                })
            }
        }
    }
    struct SlowFinalProvider;
    impl Provider for SlowFinalProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            tokio::time::sleep(std::time::Duration::from_millis(220)).await;
            Ok(ProviderResponse {
                message: Some("slow done".into()),
                tool_calls: vec![],
                input_request: None,
            })
        }
    }
    struct SlowProcessProvider;
    impl Provider for SlowProcessProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            if messages.iter().any(|message| message.is_role("tool")) {
                Ok(ProviderResponse {
                    message: Some("slow process done".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            } else {
                Ok(ProviderResponse {
                    message: Some("sleep".into()),
                    tool_calls: vec![ToolCall {
                        id: "slow-process".into(),
                        name: "process".into(),
                        input: json!({"argv":["/bin/sleep","0.22"],"cwd":".","env":{},"timeout_ms":1000,"grace_ms":100,"stdout_cap":1024,"stderr_cap":1024}),
                    }],
                    input_request: None,
                })
            }
        }
    }
    impl Provider for EditingProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[latte_engine::ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            let mut step = self.step.lock().unwrap();
            let response = match *step {
                0 => ProviderResponse {
                    message: Some("read".into()),
                    tool_calls: vec![ToolCall {
                        id: "read-1".into(),
                        name: "read_file".into(),
                        input: json!({"path":"a.txt"}),
                    }],
                    input_request: None,
                },
                1 => {
                    let value: serde_json::Value =
                        serde_json::from_str(messages.last().unwrap().content().unwrap()).unwrap();
                    ProviderResponse {
                        message: Some("edit".into()),
                        tool_calls: vec![ToolCall {
                            id: "edit-1".into(),
                            name: "edit_file".into(),
                            input: json!({"path":"a.txt","anchor":"old","after":"new","precondition":value["sha256"]}),
                        }],
                        input_request: None,
                    }
                }
                _ => ProviderResponse {
                    message: Some("changed a.txt".into()),
                    tool_calls: vec![],
                    input_request: None,
                },
            };
            *step += 1;
            Ok(response)
        }
    }
    #[tokio::test]
    async fn fake_provider_read_edit_permission_resume_and_handoff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["add", "a.txt"],
            vec!["commit", "-qm", "base"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let db = dir.path().join(".latte/db.sqlite");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        assert!(
            !std::process::Command::new("/usr/bin/grep")
                .args(["-q", "new", "a.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let runtime = AgentRuntime::new(
            engine.clone(),
            EditingProvider::default(),
            dir.path(),
            VerificationPlan {
                argv: vec![
                    "/usr/bin/grep".into(),
                    "-q".into(),
                    "new".into(),
                    "a.txt".into(),
                ],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        );
        let run = match runtime.run("change").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            other => panic!("{other}"),
        };
        assert_eq!(
            engine.show(run).unwrap().status,
            latte_core::RunStatus::WaitingPermission
        );
        let state = runtime.resume(run, true).await.unwrap();
        assert_eq!(state.status, latte_core::RunStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new"
        );
        assert!(state.handoff.is_some());
    }

    #[tokio::test]
    async fn git_handoff_reports_created_untracked_file_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "--allow-empty", "-qm", "base"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let db = dir.path().join(".latte/db.sqlite");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let runtime = AgentRuntime::new(
            engine.clone(),
            StatelessProvider,
            dir.path(),
            VerificationPlan {
                argv: vec![
                    "/usr/bin/grep".into(),
                    "-q".into(),
                    "durable".into(),
                    "fresh.txt".into(),
                ],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        )
        .with_lease_ttl(20);
        let run_id = match runtime.run("create").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("{error}"),
        };
        drop(runtime);
        drop(engine);
        tokio::time::sleep(std::time::Duration::from_millis(70)).await;
        let reopened = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let runtime = AgentRuntime::new(
            reopened,
            StatelessProvider,
            dir.path(),
            VerificationPlan {
                argv: vec![
                    "/usr/bin/grep".into(),
                    "-q".into(),
                    "durable".into(),
                    "fresh.txt".into(),
                ],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        );
        assert!(matches!(
            runtime.resume(run_id, true).await,
            Err(RuntimeError::PermissionRequired { .. })
        ));
        let completed = runtime.resume(run_id, true).await.unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(completed.handoff.unwrap().files_changed, vec!["fresh.txt"]);
    }

    #[tokio::test]
    async fn expired_process_permission_reissues_exact_nonempty_env_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join(".latte/state.db"))
            .build()
            .unwrap();
        let plan = VerificationPlan {
            argv: vec!["/bin/pwd".into()],
            cwd: ".".into(),
            timeout_ms: 999,
            grace_ms: 77,
            stdout_cap: 1024,
            stderr_cap: 1024,
        };
        let runtime =
            AgentRuntime::new(engine, EnvProcessProvider, dir.path(), plan).with_lease_ttl(1_000);
        let run_id = match runtime.run("env").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("{error}"),
        };
        tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
        assert!(matches!(
            runtime.resume(run_id, true).await,
            Err(RuntimeError::PermissionRequired { .. })
        ));
        assert_eq!(
            runtime.resume(run_id, true).await.unwrap().status,
            latte_core::RunStatus::Completed
        );
    }

    #[tokio::test]
    async fn persisted_tool_batch_resumes_middle_permission_and_executes_each_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("seed.txt"), "seed").unwrap();
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let db = dir.path().join(".latte/state.db");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let plan = VerificationPlan {
            argv: vec![
                "/usr/bin/grep".into(),
                "-q".into(),
                "once".into(),
                "batch.txt".into(),
            ],
            cwd: ".".into(),
            timeout_ms: 2_000,
            grace_ms: 250,
            stdout_cap: 1024,
            stderr_cap: 1024,
        };
        let runtime = AgentRuntime::new(engine.clone(), BatchProvider, dir.path(), plan.clone());
        let run_id = match runtime.run("batch").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("{error}"),
        };
        let saved: Checkpoint =
            serde_json::from_str(&engine.runtime_checkpoint(run_id).unwrap().unwrap()).unwrap();
        assert_eq!(saved.tool_queue.len(), 3);
        assert_eq!(saved.tool_cursor, 1);
        drop(runtime);
        drop(engine);
        let reopened = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let completed = AgentRuntime::new(reopened.clone(), BatchProvider, dir.path(), plan)
            .resume(run_id, true)
            .await
            .unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("batch.txt")).unwrap(),
            "once"
        );
        let saved: Checkpoint =
            serde_json::from_str(&reopened.runtime_checkpoint(run_id).unwrap().unwrap()).unwrap();
        assert_eq!(
            saved
                .messages
                .iter()
                .filter(|message| message.is_role("tool"))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn lease_heartbeat_keeps_long_provider_alive_past_initial_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let plan = VerificationPlan {
            argv: vec!["/bin/pwd".into()],
            cwd: ".".into(),
            timeout_ms: 1_000,
            grace_ms: 250,
            stdout_cap: 1024,
            stderr_cap: 1024,
        };
        let completed = AgentRuntime::new(engine, SlowFinalProvider, dir.path(), plan)
            .with_lease_ttl(60)
            .run("slow")
            .await
            .unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(completed.handoff.unwrap().evidence.len(), 1);
    }

    #[tokio::test]
    async fn lease_heartbeat_keeps_started_process_alive_to_one_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let plan = VerificationPlan {
            argv: vec!["/bin/pwd".into()],
            cwd: ".".into(),
            timeout_ms: 1_000,
            grace_ms: 250,
            stdout_cap: 1024,
            stderr_cap: 1024,
        };
        let runtime = AgentRuntime::new(engine.clone(), SlowProcessProvider, dir.path(), plan)
            .with_lease_ttl(60);
        let run_id = match runtime.run("slow process").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("{error}"),
        };
        let completed = runtime.resume(run_id, true).await.unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(
            engine.effect_status("slow-process").unwrap(),
            latte_engine::EffectStatus::ObservedSuccess
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn openai_http_e2e_sends_exact_tools_and_resumes_permission() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for body in [
                r#"{"choices":[{"message":{"content":"create","tool_calls":[{"id":"http-create","function":{"name":"write_file","arguments":"{\"path\":\"http.txt\",\"content\":\"ok\",\"create_intent\":true}"}}]}}]}"#,
                r#"{"choices":[{"message":{"content":"done","tool_calls":[]}}]}"#,
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 8192];
                loop {
                    let count = socket.read(&mut chunk).unwrap();
                    bytes.extend_from_slice(&chunk[..count]);
                    let Some(split) = bytes.windows(4).position(|v| v == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&bytes[..split]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap();
                    if bytes.len() >= split + 4 + length {
                        break;
                    }
                }
                let split = bytes.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
                tx.send(serde_json::from_slice::<serde_json::Value>(&bytes[split + 4..]).unwrap())
                    .unwrap();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join(".latte/state.db"))
            .build()
            .unwrap();
        let provider = crate::provider::OpenAiProvider::new(
            endpoint,
            "model",
            "secret",
            std::time::Duration::from_secs(2),
        )
        .unwrap();
        let runtime = AgentRuntime::new(
            engine,
            provider,
            dir.path(),
            VerificationPlan {
                argv: vec![
                    "/usr/bin/grep".into(),
                    "-q".into(),
                    "ok".into(),
                    "http.txt".into(),
                ],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        );
        let run_id = match runtime.run("create").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("{error}"),
        };
        let completed = runtime.resume(run_id, true).await.unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        let first = rx.recv().unwrap();
        let names = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            [
                "edit_file",
                "git_diff",
                "list_directory",
                "process",
                "read_file",
                "read_project_manifest",
                "search",
                "write_file"
            ]
            .into_iter()
            .collect()
        );
        for tool in first["tools"].as_array().unwrap() {
            assert_eq!(tool["function"]["parameters"]["type"], "object");
        }
        let second = rx.recv().unwrap();
        assert!(
            second["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool")
        );
    }

    #[tokio::test]
    async fn verifier_ask_waits_and_records_evidence_only_after_resume() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join(".latte/state.db"))
            .build()
            .unwrap();
        let runtime = AgentRuntime::new(
            engine.clone(),
            FinalProvider,
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/echo".into(), "ok".into()],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        );
        let run_id = match runtime.run("verify").await.unwrap_err() {
            RuntimeError::PermissionRequired { run_id } => run_id,
            error => panic!("{error}"),
        };
        assert_eq!(
            engine.show(run_id).unwrap().status,
            latte_core::RunStatus::WaitingPermission
        );
        let completed = runtime.resume(run_id, true).await.unwrap();
        assert_eq!(completed.status, latte_core::RunStatus::Completed);
        assert_eq!(completed.handoff.unwrap().evidence.len(), 1);
    }

    #[tokio::test]
    async fn provider_error_and_cancellation_have_durable_terminal_states() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".latte")).unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join(".latte/state.db"))
            .build()
            .unwrap();
        let failing = crate::provider::FakeProvider::default();
        failing.push_error("offline");
        let runtime = AgentRuntime::new(
            engine.clone(),
            failing,
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        );
        assert_eq!(
            runtime.run("fail").await.unwrap().status,
            latte_core::RunStatus::Failed
        );
        let cancel_dir = tempfile::tempdir().unwrap();
        let cancelled_engine = latte_engine::EngineBuilder::new()
            .workspace_root(cancel_dir.path())
            .build()
            .unwrap();
        let cancelled = AgentRuntime::new(
            cancelled_engine,
            FinalProvider,
            cancel_dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                cwd: ".".into(),
                timeout_ms: 1_000,
                grace_ms: 250,
                stdout_cap: 16 * 1024,
                stderr_cap: 16 * 1024,
            },
        );
        cancelled.cancel();
        assert_eq!(
            cancelled.run("cancel").await.unwrap().status,
            latte_core::RunStatus::Interrupted
        );
    }
}
