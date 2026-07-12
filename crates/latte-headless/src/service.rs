//! Typed command execution service shared by interactive frontends.
use crate::{
    provider::Provider,
    runtime::{AgentRuntime, RuntimeError, VerificationPlan},
};
use latte_core::{IdSource, PermissionDecision, RunId, RunState, RuntimeCommand, SystemIdSource};
use latte_engine::{CancellationToken, EngineHandle};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

/// Observable result of one typed runtime command.
#[derive(Clone, Debug)]
pub enum CommandResult {
    Run(Box<RunState>),
    Runs(Vec<RunState>),
    Accepted,
}

#[derive(Debug, Error)]
pub enum CommandError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("storage: {0}")]
    Storage(String),
    #[error("stale command: expected revision {expected}, actual {actual}")]
    Stale { expected: u64, actual: u64 },
    #[error("request identity does not match durable state")]
    RequestMismatch,
    #[error("no active runtime for run {0}")]
    NotActive(RunId),
    #[error("unsupported interactive command: {0}")]
    Unsupported(&'static str),
}

/// Runtime authority adapter. It owns provider construction and active cancellation handles.
pub struct RuntimeCommandService<F, P> {
    engine: EngineHandle,
    root: PathBuf,
    verification: VerificationPlan,
    provider: F,
    active: Arc<Mutex<HashMap<RunId, CancellationToken>>>,
    _provider: std::marker::PhantomData<fn() -> P>,
}
struct ActiveGuard {
    active: Arc<Mutex<HashMap<RunId, CancellationToken>>>,
    run_id: RunId,
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.run_id);
    }
}

impl<F: Clone, P> Clone for RuntimeCommandService<F, P> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            root: self.root.clone(),
            verification: self.verification.clone(),
            provider: self.provider.clone(),
            active: Arc::clone(&self.active),
            _provider: std::marker::PhantomData,
        }
    }
}

impl<F, P> RuntimeCommandService<F, P>
where
    F: Fn() -> Result<P, String> + Send + Sync + Clone + 'static,
    P: Provider + 'static,
{
    #[must_use]
    pub fn new(
        engine: EngineHandle,
        root: impl AsRef<Path>,
        verification: VerificationPlan,
        provider: F,
    ) -> Self {
        Self {
            engine,
            root: root.as_ref().to_owned(),
            verification,
            provider,
            active: Arc::new(Mutex::new(HashMap::new())),
            _provider: std::marker::PhantomData,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub async fn execute(&self, command: RuntimeCommand) -> Result<CommandResult, CommandError> {
        match command {
            RuntimeCommand::Run { prompt } => {
                let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
                let provider = (self.provider)()
                    .map_err(|e| CommandError::Storage(format!("provider configuration: {e}")))?;
                let cancellation = CancellationToken::new();
                self.active
                    .lock()
                    .unwrap()
                    .insert(run_id, cancellation.clone());
                let _active = ActiveGuard {
                    active: Arc::clone(&self.active),
                    run_id,
                };
                let runtime = AgentRuntime::new(
                    self.engine.clone(),
                    provider,
                    &self.root,
                    self.verification.clone(),
                )
                .with_cancellation(cancellation);
                let result = runtime.run_with_id(run_id, &prompt, None).await;
                result
                    .map(|run| CommandResult::Run(Box::new(run)))
                    .map_err(Into::into)
            }
            RuntimeCommand::ResolvePermission {
                run_id,
                request_id,
                expected_revision,
                decision,
            } => {
                let state = self.checked(run_id, expected_revision)?;
                if state
                    .pending_permission
                    .as_ref()
                    .map(|p| p.request_id.as_str())
                    != Some(request_id.as_str())
                {
                    return Err(CommandError::RequestMismatch);
                }
                if decision == PermissionDecision::Deny {
                    let now = wall_time_ms();
                    let lease = self
                        .engine
                        .acquire_lease(&format!("agent-{run_id}"), now, 60_000)
                        .map_err(|e| CommandError::Storage(e.to_string()))?;
                    let denied = self
                        .engine
                        .deny_waiting_permission(run_id, expected_revision, &lease, now)
                        .map_err(|e| CommandError::Storage(e.to_string()));
                    let _ = self.engine.release_lease(&lease);
                    return denied.map(|run| CommandResult::Run(Box::new(run)));
                }
                self.resume(run_id, true).await
            }
            RuntimeCommand::Resume {
                run_id,
                expected_revision,
            } => {
                self.checked(run_id, expected_revision)?;
                self.resume(run_id, true).await
            }
            RuntimeCommand::Cancel {
                run_id,
                expected_revision,
            } => {
                if let Some(token) = self.active.lock().unwrap().get(&run_id).cloned() {
                    token.cancel();
                    return Ok(CommandResult::Accepted);
                }
                let state = self.checked(run_id, expected_revision)?;
                if matches!(
                    state.status,
                    latte_core::RunStatus::WaitingInput
                        | latte_core::RunStatus::WaitingPermission
                        | latte_core::RunStatus::Completed
                        | latte_core::RunStatus::Failed
                        | latte_core::RunStatus::Interrupted
                ) {
                    let now = u64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX);
                    let lease = self
                        .engine
                        .acquire_lease(&format!("agent-{run_id}"), now, 60_000)
                        .map_err(|e| CommandError::Storage(e.to_string()))?;
                    self.engine
                        .cancel_waiting_run(run_id, expected_revision, &lease, now)
                        .map_err(|e| CommandError::Storage(e.to_string()))?;
                    let _ = self.engine.release_lease(&lease);
                } else {
                    return Err(CommandError::NotActive(run_id));
                }
                Ok(CommandResult::Accepted)
            }
            RuntimeCommand::Show { run_id } => self
                .engine
                .show(run_id)
                .map(|run| CommandResult::Run(Box::new(run)))
                .map_err(|e| CommandError::Storage(e.to_string())),
            RuntimeCommand::List => self
                .engine
                .list()
                .map(CommandResult::Runs)
                .map_err(|e| CommandError::Storage(e.to_string())),
            RuntimeCommand::ProvideInput {
                run_id,
                request_id,
                expected_revision,
                value,
            } => {
                let state = self.checked(run_id, expected_revision)?;
                if state.pending_input.as_ref().map(|p| p.request_id.as_str())
                    != Some(request_id.as_str())
                {
                    return Err(CommandError::RequestMismatch);
                }
                let provider = (self.provider)()
                    .map_err(|e| CommandError::Storage(format!("provider configuration: {e}")))?;
                let cancellation = CancellationToken::new();
                self.active
                    .lock()
                    .unwrap()
                    .insert(run_id, cancellation.clone());
                let _active = ActiveGuard {
                    active: Arc::clone(&self.active),
                    run_id,
                };
                AgentRuntime::new(
                    self.engine.clone(),
                    provider,
                    &self.root,
                    self.verification.clone(),
                )
                .with_cancellation(cancellation)
                .provide_input(run_id, &request_id, &value)
                .await
                .map(|run| CommandResult::Run(Box::new(run)))
                .map_err(Into::into)
            }
            RuntimeCommand::Shutdown => Ok(CommandResult::Accepted),
        }
    }

    pub fn reconcile_unknown_and_abort(
        &self,
        run_id: RunId,
        effect_id: &str,
    ) -> Result<RunState, CommandError> {
        let now = wall_time_ms();
        let state = self
            .engine
            .show(run_id)
            .map_err(|e| CommandError::Storage(e.to_string()))?;
        let lease = self
            .engine
            .acquire_lease(&format!("reconcile-{run_id}"), now, 60_000)
            .map_err(|e| CommandError::Storage(e.to_string()))?;
        let result = self
            .engine
            .resolve_unknown_effect_and_abort(run_id, effect_id, state.revision, &lease, now)
            .map_err(|e| CommandError::Storage(e.to_string()));
        let _ = self.engine.release_lease(&lease);
        result
    }

    fn checked(&self, run_id: RunId, expected: u64) -> Result<RunState, CommandError> {
        let state = self
            .engine
            .show(run_id)
            .map_err(|e| CommandError::Storage(e.to_string()))?;
        if state.revision != expected {
            return Err(CommandError::Stale {
                expected,
                actual: state.revision,
            });
        }
        Ok(state)
    }
    async fn resume(&self, run_id: RunId, allow: bool) -> Result<CommandResult, CommandError> {
        let provider = (self.provider)()
            .map_err(|e| CommandError::Storage(format!("provider configuration: {e}")))?;
        let cancellation = CancellationToken::new();
        self.active
            .lock()
            .unwrap()
            .insert(run_id, cancellation.clone());
        let _active = ActiveGuard {
            active: Arc::clone(&self.active),
            run_id,
        };
        AgentRuntime::new(
            self.engine.clone(),
            provider,
            &self.root,
            self.verification.clone(),
        )
        .with_cancellation(cancellation)
        .resume(run_id, allow)
        .await
        .map(|run| CommandResult::Run(Box::new(run)))
        .map_err(Into::into)
    }
}

fn wall_time_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

struct ActorRequest {
    command: RuntimeCommand,
    result: oneshot::Sender<Result<CommandResult, CommandError>>,
}
/// Bounded command ingress. Mutations are FIFO; cancel bypasses the queue to reach active work.
#[derive(Clone)]
pub struct RuntimeCommandActor {
    tx: mpsc::Sender<ActorRequest>,
    cancel_tx: mpsc::Sender<ActorRequest>,
}
impl RuntimeCommandActor {
    pub fn start<F, P>(service: RuntimeCommandService<F, P>, capacity: usize) -> Self
    where
        F: Fn() -> Result<P, String> + Send + Sync + Clone + 'static,
        P: Provider + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<ActorRequest>(capacity.max(1));
        let (cancel_tx, mut cancel_rx) = mpsc::channel::<ActorRequest>(capacity.max(1));
        let cancel_service = service.clone();
        tokio::spawn(async move {
            while let Some(request) = cancel_rx.recv().await {
                let result = cancel_service.execute(request.command).await;
                let _ = request.result.send(result);
            }
        });
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let result = service.execute(request.command).await;
                let _ = request.result.send(result);
            }
        });
        Self { tx, cancel_tx }
    }
    pub async fn execute(&self, command: RuntimeCommand) -> Result<CommandResult, CommandError> {
        let (tx, rx) = oneshot::channel();
        let lane = if matches!(command, RuntimeCommand::Cancel { .. }) {
            &self.cancel_tx
        } else {
            &self.tx
        };
        lane.send(ActorRequest {
            command,
            result: tx,
        })
        .await
        .map_err(|_| CommandError::Storage("command actor closed".into()))?;
        rx.await
            .map_err(|_| CommandError::Storage("command result dropped".into()))?
    }
}

#[cfg(test)]
#[allow(
    clippy::collapsible_if,
    clippy::manual_let_else,
    clippy::unnecessary_wraps
)]
mod tests {
    use super::*;
    use crate::provider::{FakeProvider, ProviderResponse};
    use crate::provider::{Message, Provider, ProviderError};
    use latte_core::{RunStatus, RuntimeCommand};
    use latte_engine::ToolDescriptor;

    #[derive(Clone)]
    struct SlowProvider;
    impl Provider for SlowProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(ProviderResponse {
                message: Some("late".into()),
                tool_calls: vec![],
                input_request: None,
            })
        }
    }

    #[derive(Clone)]
    struct InputProvider;
    impl Provider for InputProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            if messages
                .iter()
                .any(|m| m.is_role("user") && m.content() == Some("answer42"))
            {
                Ok(ProviderResponse {
                    message: Some("used answer42".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            } else {
                Ok(ProviderResponse {
                    message: Some("need input".into()),
                    tool_calls: vec![],
                    input_request: Some(crate::provider::InputRequest {
                        id: "input-1".into(),
                        prompt: "answer?".into(),
                        secret: false,
                    }),
                })
            }
        }
    }
    #[derive(Clone)]
    struct SecretProvider;
    impl Provider for SecretProvider {
        async fn complete(
            &self,
            _: &[Message],
            _: &[ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            Ok(ProviderResponse {
                message: Some("credential needed".into()),
                tool_calls: vec![],
                input_request: Some(crate::provider::InputRequest {
                    id: "secret-id".into(),
                    prompt: "secret-value".into(),
                    secret: true,
                }),
            })
        }
    }
    #[derive(Clone)]
    struct SlowInputProvider;
    impl Provider for SlowInputProvider {
        async fn complete(
            &self,
            messages: &[Message],
            _: &[ToolDescriptor],
        ) -> Result<ProviderResponse, ProviderError> {
            if messages.iter().any(|m| m.content() == Some("go")) {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(ProviderResponse {
                    message: Some("late".into()),
                    tool_calls: vec![],
                    input_request: None,
                })
            } else {
                Ok(ProviderResponse {
                    message: Some("input".into()),
                    tool_calls: vec![],
                    input_request: Some(crate::provider::InputRequest {
                        id: "slow-input".into(),
                        prompt: "go?".into(),
                        secret: false,
                    }),
                })
            }
        }
    }

    fn provider() -> Result<FakeProvider, String> {
        Ok(FakeProvider::scripted([ProviderResponse {
            message: Some("done".into()),
            tool_calls: vec![],
            input_request: None,
        }]))
    }
    fn plan() -> VerificationPlan {
        VerificationPlan {
            argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            cwd: ".".into(),
            timeout_ms: 1_000,
            grace_ms: 100,
            stdout_cap: 1024,
            stderr_cap: 1024,
        }
    }
    fn service(
        dir: &tempfile::TempDir,
    ) -> RuntimeCommandService<fn() -> Result<FakeProvider, String>, FakeProvider> {
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join("state.db"))
            .build()
            .unwrap();
        RuntimeCommandService::new(engine, dir.path(), plan(), provider)
    }

    #[tokio::test]
    async fn typed_start_permission_allow_completes() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(&dir);
        let waiting = match service
            .execute(RuntimeCommand::Run {
                prompt: "test".into(),
            })
            .await
            .unwrap_err()
        {
            CommandError::Runtime(RuntimeError::PermissionRequired { run_id }) => run_id,
            e => panic!("{e}"),
        };
        let state = match service
            .execute(RuntimeCommand::Show { run_id: waiting })
            .await
            .unwrap()
        {
            CommandResult::Run(run) => run,
            _ => panic!(),
        };
        assert_eq!(state.status, RunStatus::WaitingPermission);
        let request = state.pending_permission.as_ref().unwrap();
        let completed = service
            .execute(RuntimeCommand::ResolvePermission {
                run_id: waiting,
                request_id: request.request_id.clone(),
                expected_revision: state.revision,
                decision: PermissionDecision::Allow,
            })
            .await
            .unwrap();
        assert!(matches!(completed,CommandResult::Run(run) if run.status==RunStatus::Completed));
    }

    #[tokio::test]
    async fn explicit_deny_fails_and_stale_command_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(&dir);
        let run_id = match service
            .execute(RuntimeCommand::Run {
                prompt: "test".into(),
            })
            .await
            .unwrap_err()
        {
            CommandError::Runtime(RuntimeError::PermissionRequired { run_id }) => run_id,
            e => panic!("{e}"),
        };
        let state = match service
            .execute(RuntimeCommand::Show { run_id })
            .await
            .unwrap()
        {
            CommandResult::Run(run) => run,
            _ => panic!(),
        };
        let request = state.pending_permission.as_ref().unwrap();
        let denied = service
            .execute(RuntimeCommand::ResolvePermission {
                run_id,
                request_id: request.request_id.clone(),
                expected_revision: state.revision,
                decision: PermissionDecision::Deny,
            })
            .await
            .unwrap();
        assert!(matches!(denied,CommandResult::Run(run) if run.status==RunStatus::Failed));
        assert!(matches!(
            service
                .execute(RuntimeCommand::Resume {
                    run_id,
                    expected_revision: state.revision
                })
                .await,
            Err(CommandError::Stale { .. })
        ));
    }

    #[tokio::test]
    async fn deny_does_not_construct_provider() {
        let dir = tempfile::tempdir().unwrap();
        let initial = service(&dir);
        let run_id = match initial
            .execute(RuntimeCommand::Run {
                prompt: "test".into(),
            })
            .await
            .unwrap_err()
        {
            CommandError::Runtime(RuntimeError::PermissionRequired { run_id }) => run_id,
            error => panic!("{error}"),
        };
        let state = initial.engine.show(run_id).unwrap();
        let request_id = state
            .pending_permission
            .as_ref()
            .unwrap()
            .request_id
            .clone();
        let unavailable = RuntimeCommandService::new(
            initial.engine.clone(),
            dir.path(),
            VerificationPlan {
                argv: Vec::new(),
                cwd: ".".into(),
                timeout_ms: 0,
                grace_ms: 0,
                stdout_cap: 0,
                stderr_cap: 0,
            },
            || -> Result<FakeProvider, String> { panic!("provider must not be constructed") },
        );
        let denied = unavailable
            .execute(RuntimeCommand::ResolvePermission {
                run_id,
                request_id,
                expected_revision: state.revision,
                decision: PermissionDecision::Deny,
            })
            .await
            .unwrap();
        assert!(
            matches!(denied, CommandResult::Run(run) if run.failure.as_ref().is_some_and(|failure| failure.code == latte_core::FailureCode::PermissionDenied))
        );
    }

    #[test]
    fn reconcile_unknown_uses_engine_authority_without_provider() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join("state.db"))
            .build()
            .unwrap();
        let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        engine.create_run(run_id, 1).unwrap();
        let unavailable = RuntimeCommandService::new(
            engine,
            dir.path(),
            plan(),
            || -> Result<FakeProvider, String> { panic!("provider must not be constructed") },
        );
        assert!(matches!(
            unavailable.reconcile_unknown_and_abort(run_id, "missing-effect"),
            Err(CommandError::Storage(_))
        ));
    }

    #[tokio::test]
    async fn cancel_reaches_active_runtime_and_interrupts_durably() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(dir.path().join("state.db"))
            .build()
            .unwrap();
        let service = RuntimeCommandService::new(engine.clone(), dir.path(), plan(), || {
            Ok::<_, String>(SlowProvider)
        });
        let running_service = service.clone();
        let task = tokio::spawn(async move {
            running_service
                .execute(RuntimeCommand::Run {
                    prompt: "slow".into(),
                })
                .await
        });
        let state = loop {
            if let Some(run) = engine.list().unwrap().into_iter().next() {
                if run.status == RunStatus::Running {
                    break run;
                }
            }
            tokio::task::yield_now().await;
        };
        assert!(matches!(
            service
                .execute(RuntimeCommand::Cancel {
                    run_id: state.run_id,
                    expected_revision: state.revision
                })
                .await
                .unwrap(),
            CommandResult::Accepted
        ));
        assert!(
            matches!(task.await.unwrap().unwrap(),CommandResult::Run(run) if run.status==RunStatus::Interrupted)
        );
        assert_eq!(
            engine.show(state.run_id).unwrap().status,
            RunStatus::Interrupted
        );
    }

    #[tokio::test]
    async fn input_request_survives_reopen_and_exact_value_resumes_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let service = RuntimeCommandService::new(
            engine.clone(),
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                ..plan()
            },
            || Ok::<_, String>(InputProvider),
        );
        let run_id = match service
            .execute(RuntimeCommand::Run {
                prompt: "ask".into(),
            })
            .await
            .unwrap_err()
        {
            CommandError::Runtime(RuntimeError::InputRequired { run_id }) => run_id,
            e => panic!("{e}"),
        };
        let waiting = engine.show(run_id).unwrap();
        assert_eq!(waiting.status, RunStatus::WaitingInput);
        assert!(matches!(
            service
                .execute(RuntimeCommand::ProvideInput {
                    run_id,
                    request_id: "wrong".into(),
                    expected_revision: waiting.revision,
                    value: "answer42".into()
                })
                .await,
            Err(CommandError::RequestMismatch)
        ));
        drop(service);
        drop(engine);
        let reopened = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&db)
            .build()
            .unwrap();
        let service = RuntimeCommandService::new(
            reopened.clone(),
            dir.path(),
            VerificationPlan {
                argv: vec!["/bin/pwd".into()],
                ..plan()
            },
            || Ok::<_, String>(InputProvider),
        );
        let completed = service
            .execute(RuntimeCommand::ProvideInput {
                run_id,
                request_id: "input-1".into(),
                expected_revision: waiting.revision,
                value: "answer42".into(),
            })
            .await
            .unwrap();
        assert!(
            matches!(completed,CommandResult::Run(run) if run.status==RunStatus::Completed && run.handoff.as_ref().unwrap().summary.contains("used answer42"))
        );
        assert_eq!(reopened.show(run_id).unwrap().status, RunStatus::Completed);
    }

    #[tokio::test]
    async fn waiting_input_cancel_is_terminal_and_not_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let service = RuntimeCommandService::new(engine.clone(), dir.path(), plan(), || {
            Ok::<_, String>(InputProvider)
        });
        let run_id = match service
            .execute(RuntimeCommand::Run {
                prompt: "ask".into(),
            })
            .await
            .unwrap_err()
        {
            CommandError::Runtime(RuntimeError::InputRequired { run_id }) => run_id,
            error => panic!("{error}"),
        };
        let waiting = engine.show(run_id).unwrap();
        assert!(matches!(
            service
                .execute(RuntimeCommand::Cancel {
                    run_id,
                    expected_revision: waiting.revision
                })
                .await
                .unwrap(),
            CommandResult::Accepted
        ));
        let cancelled = engine.show(run_id).unwrap();
        assert_eq!(cancelled.status, RunStatus::Failed);
        assert_eq!(
            cancelled.failure.unwrap().code,
            latte_core::FailureCode::Cancelled
        );
        assert!(engine.runtime_checkpoint(run_id).unwrap().is_none());
        assert!(
            service
                .execute(RuntimeCommand::Resume {
                    run_id,
                    expected_revision: cancelled.revision
                })
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn secret_input_request_fails_closed_without_checkpointing_request() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let service = RuntimeCommandService::new(engine.clone(), dir.path(), plan(), || {
            Ok::<_, String>(SecretProvider)
        });
        let failed = service
            .execute(RuntimeCommand::Run {
                prompt: "ask secret".into(),
            })
            .await
            .unwrap();
        let CommandResult::Run(failed) = failed else {
            panic!()
        };
        assert_eq!(failed.status, RunStatus::Failed);
        let checkpoint = engine.runtime_checkpoint(failed.run_id).unwrap().unwrap();
        assert!(!checkpoint.contains("secret-id"));
        assert!(!checkpoint.contains("secret-value"));
    }
    #[tokio::test]
    async fn bounded_actor_keeps_results_and_cancel_interleaves_long_input_resume() {
        let dir = tempfile::tempdir().unwrap();
        let engine = latte_engine::EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        let service = RuntimeCommandService::new(engine.clone(), dir.path(), plan(), || {
            Ok::<_, String>(SlowInputProvider)
        });
        let run_id = match service
            .execute(RuntimeCommand::Run {
                prompt: "ask".into(),
            })
            .await
            .unwrap_err()
        {
            CommandError::Runtime(RuntimeError::InputRequired { run_id }) => run_id,
            e => panic!("{e}"),
        };
        let waiting = engine.show(run_id).unwrap();
        let actor = RuntimeCommandActor::start(service, 1);
        let work_actor = actor.clone();
        let work = tokio::spawn(async move {
            work_actor
                .execute(RuntimeCommand::ProvideInput {
                    run_id,
                    request_id: "slow-input".into(),
                    expected_revision: waiting.revision,
                    value: "go".into(),
                })
                .await
        });
        let running = loop {
            let state = engine.show(run_id).unwrap();
            if state.status == latte_core::RunStatus::Running {
                break state;
            }
            tokio::task::yield_now().await;
        };
        assert!(matches!(
            actor
                .execute(RuntimeCommand::Cancel {
                    run_id,
                    expected_revision: running.revision
                })
                .await
                .unwrap(),
            CommandResult::Accepted
        ));
        assert!(
            matches!(work.await.unwrap().unwrap(),CommandResult::Run(run) if run.status==latte_core::RunStatus::Interrupted)
        );
        for _ in 0..20 {
            assert!(matches!(
                actor.execute(RuntimeCommand::List).await.unwrap(),
                CommandResult::Runs(_)
            ));
        }
    }
}
