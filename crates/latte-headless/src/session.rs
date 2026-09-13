//! Session v2 composition for the transcript-first clients.
//!
//! The service is deliberately a coordinator, not an effect authority: every
//! durable change goes through `EngineHandle::commit_session_turn_update` and
//! provider calls receive no direct repository capability.

use crate::{
    context,
    profile::{ProfileCatalog, ResolvedProfile},
    provider::{
        Message, Provider, ProviderContext, ProviderError, ProviderEvent, ProviderEventSink,
        ProviderRequest, valid_tool_call_id,
    },
    registry::ResolvedProvider,
    runtime::VerificationPlan,
};
use latte_core::{
    ContextPolicy, FailureCode, Retryability, SessionCommandId, SessionId, SessionLifecycle,
    SessionProviderBinding, SessionSnapshot, SessionTransientProgress, TranscriptKind, TurnFailure,
    TurnId, redact_session_text, valid_openai_chat_input_request_id, wall_time_ms as now_ms,
};
use latte_engine::{
    CancellationToken, CommitSessionTurnUpdate, EngineHandle, Lease, SessionCommitRequest,
    SessionEffectDescriptor, SessionEffectExecutionError, SessionEffectPresentation,
    SessionEffectRequest, SessionEffectStartRequest, SessionEffectStarted,
    SessionLeaseLossRecovery, StorageError,
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::oneshot;
use uuid::Uuid;

const SESSION_VERIFICATION_EFFECT_PREFIX: &str = "session-verification:";
/// Pre-rename (schema <13) prefix minted into durable verification effect ids.
/// A verification effect left `waiting_permission` by an old binary carries this
/// id; after upgrade its approval must still be routed as verification rather
/// than a normal provider tool continuation.
const LEGACY_THREAD_VERIFICATION_EFFECT_PREFIX: &str = "thread-verification:";
const SESSION_MAILBOX_CAPACITY: usize = 8;
/// Poll interval while waiting out a dying runner's mailbox teardown residue
/// (issue #21).
const RESIDUE_SETTLE_INTERVAL_MS: u64 = 5;
/// Total budget for that wait. The teardown is one mutex acquisition on the
/// dying runner's side, so this covers extreme scheduling delays many times
/// over while keeping a misbehaving-residue worst case bounded.
const RESIDUE_SETTLE_BUDGET_MS: u64 = 100;

/// Identifies a verification effect whether it was minted by a current binary
/// (`session-verification:`) or persisted before the Thread→Session rename
/// (`thread-verification:`).
fn is_verification_effect_id(request_id: &str) -> bool {
    request_id.starts_with(SESSION_VERIFICATION_EFFECT_PREFIX)
        || request_id.starts_with(LEGACY_THREAD_VERIFICATION_EFFECT_PREFIX)
}

/// Whether some assistant message in this segment declared `tool_call_id`.
///
/// The Chat Completions grammar admits a `tool` message only as the answer to
/// a call the assistant made. Effects the engine starts on its own — the
/// configured verification run — are durable tool results too, but carry an id
/// we minted rather than one the model emitted, so they are transcript content
/// and not provider history.
fn declared_tool_call(segment: &[Message], tool_call_id: &str) -> bool {
    segment.iter().any(|message| match message {
        Message::Assistant { tool_calls, .. } => {
            tool_calls.iter().any(|call| call.id == tool_call_id)
        }
        _ => false,
    })
}

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
pub struct SessionHistoryPolicy {
    pub max_request_bytes: usize,
    pub max_input_bytes: usize,
    pub reserved_output_bytes: usize,
    pub context_cap_bytes: usize,
    /// Optional hard bound on how many tool batches one turn may open.
    ///
    /// The agent loop is a recursion with no natural fixed point: the model
    /// decides whether to call another tool. The default is deliberately
    /// **unlimited** (`None`): a round count cannot by itself prove that a
    /// model is failing to converge, and forcing a stop could interrupt a
    /// legitimate long read-edit-verify task just before its final answer.
    /// Operators who want a hard safety bound set an explicit positive limit;
    /// the turn then ends retryably when a provider response tries to open a
    /// further tool batch at or beyond the bound. A response that stops
    /// calling tools is never blocked, so the turn can still consume its last
    /// tool results and finish normally once the bound is reached. Single
    /// request timeouts, cancellation, the I/O cap, and process cleanup
    /// remain in force independently of this setting.
    pub max_tool_rounds: Option<u32>,
    /// Wall-clock budget for one provider request. The provider transport also
    /// carries its own `timeout_ms` (per provider config, default 60s); the
    /// request deadline is the minimum of the two.
    pub provider_timeout_ms: u64,
}

impl Default for SessionHistoryPolicy {
    fn default() -> Self {
        Self {
            max_request_bytes: 512 * 1024,
            max_input_bytes: 384 * 1024,
            reserved_output_bytes: 128 * 1024,
            context_cap_bytes: 64 * 1024,
            // Unlimited by default. Wall-clock protection comes from the
            // per-request provider timeout and cancellation instead.
            max_tool_rounds: None,
            provider_timeout_ms: 60_000,
        }
    }
}

impl SessionHistoryPolicy {
    /// Validates the exact input/output budget without constructing a request.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_request_bytes == 0
            || self.max_input_bytes == 0
            || self.reserved_output_bytes >= self.max_input_bytes
            || self.context_cap_bytes == 0
        {
            return Err("max_request_bytes/context cap must be nonzero and reserved output must be smaller than input budget".into());
        }
        if self.max_tool_rounds.is_some_and(|max| max == 0) {
            return Err("max_tool_rounds must be omitted (unlimited) or at least 1".into());
        }
        if self.provider_timeout_ms == 0 {
            return Err("provider_timeout_ms must be nonzero".into());
        }
        Ok(())
    }
    fn budget(&self) -> Result<usize, SessionRuntimeError> {
        self.validate().map_err(SessionRuntimeError::History)?;
        Ok(self
            .max_request_bytes
            .min(self.max_input_bytes - self.reserved_output_bytes))
    }
}

/// A secret-resolving provider constructor. Callers must validate the supplied
/// binding first; the registry's `resolve_session_bound` does exactly that.
pub type SessionProviderFactory =
    Arc<dyn Fn(&SessionProviderBinding) -> Result<ResolvedProvider, String> + Send + Sync>;

/// Non-durable provider progress bridge. It is intentionally separate from
/// transcript persistence and is cleared by the TUI on a gap or reconnect.
pub trait SessionProgressSink: Send + Sync {
    fn observe(&self, session_id: SessionId, progress: SessionTransientProgress);
}
impl<F: Fn(SessionId, SessionTransientProgress) + Send + Sync> SessionProgressSink for F {
    fn observe(&self, session_id: SessionId, progress: SessionTransientProgress) {
        self(session_id, progress);
    }
}

/// Result of executing the effects in one persisted assistant tool batch.
enum ToolBatchOutcome {
    /// Every call finished; the turn may issue its next provider request.
    Completed {
        snapshot: SessionSnapshot,
        messages: Vec<Message>,
    },
    /// The turn parked at an Ask permission gate, failed, interrupted, or
    /// otherwise left the running lifecycle. The snapshot is terminal for
    /// this entry into the turn loop.
    Parked(SessionSnapshot),
}

/// Control flow after one provider request inside the iterative turn loop.
enum RoundFlow {
    /// The tool batch finished and another provider request is allowed.
    Continue {
        snapshot: SessionSnapshot,
        messages: Vec<Message>,
    },
    /// The turn completed, parked, failed, or was interrupted.
    Done(SessionSnapshot),
}

#[derive(Debug, Error)]
pub enum SessionRuntimeError {
    #[error("session storage: {0}")]
    Storage(#[from] StorageError),
    #[error("session provider configuration: {0}")]
    ProviderConfiguration(String),
    #[error("session provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("session history: {0}")]
    History(String),
    #[error("session is not in the requested active state")]
    InvalidState,
    #[error("session input mailbox is full")]
    MailboxFull,
    #[error("session effect: {0}")]
    Effect(String),
}

/// Transcript-first provider coordinator. The active cancellation map is
/// process-local only; durable recovery remains in the engine.
#[derive(Clone)]
pub struct SessionRuntimeService {
    engine: EngineHandle,
    root: PathBuf,
    profiles: Arc<ProfileCatalog>,
    provider: SessionProviderFactory,
    active: Arc<Mutex<HashMap<SessionId, CancellationToken>>>,
    mailboxes: Arc<Mutex<HashMap<SessionId, VecDeque<String>>>>,
    progress: Option<Arc<dyn SessionProgressSink>>,
    verification: Option<VerificationPlan>,
    lease_ttl_ms: u64,
}

struct SessionLeaseGuard {
    engine: EngineHandle,
    lease: Lease,
}

struct SessionRunnerGuard {
    session_id: SessionId,
    mailboxes: Arc<Mutex<HashMap<SessionId, VecDeque<String>>>>,
    closed: bool,
}

/// One user-segment of the provider history: the messages the request
/// replays, the bounded plain text a compaction summary is generated from,
/// and the highest durable sequence folded into the segment. The sequence is
/// `None` for the synthetic current-prompt segment, which has no durable
/// card of its own.
struct HistorySegment {
    messages: Vec<Message>,
    text: String,
    max_sequence: Option<u64>,
}

impl HistorySegment {
    fn new(max_sequence: Option<u64>, messages: Vec<Message>, text: String) -> Self {
        Self {
            messages,
            text,
            max_sequence,
        }
    }

    fn push_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    fn push_text(&mut self, text: &str, sequence: u64) {
        self.text.push_str(text);
        self.max_sequence = self.max_sequence.max(Some(sequence));
    }
}

/// The history a request window had to discard: the plain text a compaction
/// summary is generated from (newest-first, byte-bounded by the profile's
/// `max_summary_source_bytes`) and the newest durable sequence the summary
/// supersedes.
#[derive(Clone)]
struct SupersededHistory {
    text: String,
    through_sequence: u64,
}

/// A selected request window: the system message, the kept segment messages,
/// the resolved history policy, and — when the window dropped older history —
/// what was dropped. `assemble` produces the final provider messages,
/// optionally prefixing a compaction summary ahead of the kept segments.
struct HistoryWindow {
    system: Message,
    kept: Vec<Message>,
    policy: SessionHistoryPolicy,
    superseded: Option<SupersededHistory>,
}

impl HistoryWindow {
    fn assemble(&self, summary: Option<&str>) -> Vec<Message> {
        let mut messages = vec![self.system.clone()];
        if let Some(summary) = summary {
            messages.push(Message::User {
                content: format!(
                    "Earlier conversation, automatically compacted to this summary:\n\n{summary}"
                ),
            });
        }
        messages.extend(self.kept.iter().cloned());
        messages
    }
}

/// The outcome of history preparation for one new child.
enum PreparedHistory {
    /// Nothing was discarded, or compaction is disabled — the historical
    /// silent-discard behavior.
    Complete(Vec<Message>),
    /// Older history was summarized; the caller persists the durable
    /// `CompactSummary` card and runs the turn with these messages.
    Summarized {
        messages: Vec<Message>,
        superseded: SupersededHistory,
        summary: String,
    },
    /// Compaction is enabled but the summary request failed (or overflowed
    /// the budget): degrade to the silent-discard behavior, with a durable
    /// failure audit card.
    Degraded(Vec<Message>),
}

impl SessionRunnerGuard {
    fn mark_closed(&mut self) {
        self.closed = true;
    }
}

impl Drop for SessionRunnerGuard {
    fn drop(&mut self) {
        if !self.closed {
            self.mailboxes
                .lock()
                .expect("mailbox mutex poisoned")
                .remove(&self.session_id);
        }
    }
}

impl std::ops::Deref for SessionLeaseGuard {
    type Target = Lease;

    fn deref(&self) -> &Self::Target {
        &self.lease
    }
}

impl Drop for SessionLeaseGuard {
    fn drop(&mut self) {
        let _ = self.engine.release_lease(&self.lease);
    }
}

impl SessionRuntimeService {
    #[must_use]
    pub fn new(
        engine: EngineHandle,
        root: impl AsRef<Path>,
        policy: SessionHistoryPolicy,
        provider: SessionProviderFactory,
    ) -> Self {
        Self {
            engine,
            root: root.as_ref().to_owned(),
            profiles: Arc::new(ProfileCatalog::without_registry(ContextPolicy::from(
                policy,
            ))),
            provider,
            active: Arc::new(Mutex::new(HashMap::new())),
            mailboxes: Arc::new(Mutex::new(HashMap::new())),
            progress: None,
            verification: None,
            lease_ttl_ms: 60_000,
        }
    }

    /// Attaches a registry-backed harness profile catalog. Without one the
    /// service resolves the built-in profile over the constructor's base
    /// policy and per-model `context_window` tightening is unavailable;
    /// production composition roots pass the catalog built from the same
    /// registry that resolves providers.
    #[must_use]
    pub fn with_profile_catalog(mut self, catalog: Arc<ProfileCatalog>) -> Self {
        self.profiles = catalog;
        self
    }

    /// Resolves the harness profile for one binding. Fail-closed: an
    /// unresolvable provider type is a typed error, never a silent
    /// fallback to defaults.
    fn resolved_profile(
        &self,
        binding: &SessionProviderBinding,
    ) -> Result<ResolvedProfile, SessionRuntimeError> {
        self.profiles
            .resolve(binding)
            .map_err(|error| SessionRuntimeError::ProviderConfiguration(error.to_string()))
    }

    /// Connects typed transient provider progress to an interactive frontend.
    #[must_use]
    pub fn with_progress_sink(mut self, sink: Arc<dyn SessionProgressSink>) -> Self {
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
        session_id: SessionId,
        prompt: String,
        binding: SessionProviderBinding,
        focus: Option<&Path>,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let runner = self.begin_runner(session_id)?;
        let snapshot = match self
            .start_one(None, session_id, prompt, binding, focus, None)
            .await?
        {
            latte_core::CreateOutcome::Created(snapshot)
            | latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
        };
        self.drain_mailbox(snapshot, runner).await
    }

    /// Like [`Self::start`], but signals `accept` at the durable-acceptance
    /// boundary (once the user submission is persisted) before running the
    /// provider turn. Lets a server return 202 only after the command is
    /// durable, while the turn continues under the same supervised execution.
    /// The signal carries the accepted snapshot wrapped in
    /// [`latte_core::CreateOutcome`]: `Created` (202) or `Replayed` (200,
    /// crash-safe idempotent replay).
    pub async fn start_accepted(
        &self,
        session_id: SessionId,
        command_id: SessionCommandId,
        prompt: String,
        binding: SessionProviderBinding,
        focus: Option<&Path>,
        accept: oneshot::Sender<
            Result<latte_core::CreateOutcome<SessionSnapshot>, latte_core::CreateAcceptError>,
        >,
    ) -> Result<latte_core::CreateOutcome<SessionSnapshot>, SessionRuntimeError> {
        let runner = match self.begin_runner(session_id) {
            Ok(runner) => runner,
            Err(error) => {
                let _ = accept.send(Err(latte_core::CreateAcceptError::Failed(
                    error.to_string(),
                )));
                return Err(error);
            }
        };
        let outcome = self
            .start_one(
                Some(command_id),
                session_id,
                prompt,
                binding,
                focus,
                Some(accept),
            )
            .await?;
        match outcome {
            latte_core::CreateOutcome::Created(snapshot) => {
                let drained = self.drain_mailbox(snapshot, runner).await?;
                Ok(latte_core::CreateOutcome::Created(drained))
            }
            // A replay owns no runner: the accepted submission is already
            // durable and the pre-crash provider task is gone, so there is
            // nothing to drain. Recovery of the expired lease is the
            // sweeper's job.
            latte_core::CreateOutcome::Replayed(snapshot) => {
                Ok(latte_core::CreateOutcome::Replayed(snapshot))
            }
        }
    }

    /// Looks up a durable crash-safe replay for a create command before any
    /// lease is acquired. `Ok(Some)` means the command was already durably
    /// accepted (replay the snapshot, start no runner); `Ok(None)` means it is
    /// a fresh create; `Err` is a command-identity conflict or storage error.
    fn durable_replay(
        &self,
        command_id: &SessionCommandId,
        session_id: SessionId,
        prompt: &str,
        binding: &SessionProviderBinding,
        focus: Option<&Path>,
    ) -> Result<Option<SessionSnapshot>, SessionRuntimeError> {
        self.engine
            .lookup_create_replay(
                command_id,
                session_id,
                prompt,
                binding,
                focus.and_then(|focus| focus.to_str()),
            )
            .map_err(SessionRuntimeError::from)
    }

    async fn start_one(
        &self,
        command_id: Option<SessionCommandId>,
        session_id: SessionId,
        prompt: String,
        binding: SessionProviderBinding,
        focus: Option<&Path>,
        accept: Option<
            oneshot::Sender<
                Result<latte_core::CreateOutcome<SessionSnapshot>, latte_core::CreateAcceptError>,
            >,
        >,
    ) -> Result<latte_core::CreateOutcome<SessionSnapshot>, SessionRuntimeError> {
        let messages = match self.preflight_messages(&binding, &prompt, focus) {
            Ok(messages) => messages,
            Err(error) => {
                signal_accept(
                    accept,
                    Err(latte_core::CreateAcceptError::Failed(error.to_string())),
                );
                return Err(error);
            }
        };
        // Crash-safe idempotency: look up the durable dedup record BEFORE
        // acquiring the lease. A retry after a crash hits the record and
        // replays without blocking on the dead owner's still-unexpired lease.
        if let Some(command_id) = &command_id {
            match self.durable_replay(command_id, session_id, &prompt, &binding, focus) {
                Ok(Some(snapshot)) => {
                    signal_accept(
                        accept,
                        Ok(latte_core::CreateOutcome::Replayed(snapshot.clone())),
                    );
                    return Ok(latte_core::CreateOutcome::Replayed(snapshot));
                }
                Ok(None) => {}
                Err(error) => {
                    signal_accept(accept, Err(classify_create_error(&error)));
                    return Err(error);
                }
            }
        }
        let turn_id = new_turn_id();
        let now = now_ms();
        let lease = match self.acquire(session_id) {
            Ok(lease) => lease,
            Err(error) => {
                signal_accept(
                    accept,
                    Err(latte_core::CreateAcceptError::Failed(error.to_string())),
                );
                return Err(error);
            }
        };
        // The legacy in-process path (`start`) passes no command id; mint a
        // fresh one so the durable dedup record is still written (it will
        // never replay, but the create contract is uniform).
        let create_command_id =
            command_id.unwrap_or_else(|| SessionCommandId::from_uuid(Uuid::now_v7()));
        let started = match self.engine.create_started_session_v2(
            &create_command_id,
            session_id,
            turn_id,
            binding,
            &prompt,
            &lease,
            now,
            focus.and_then(|f| f.to_str()),
        ) {
            Ok(latte_core::CreateOutcome::Created(snapshot)) => snapshot,
            Ok(latte_core::CreateOutcome::Replayed(snapshot)) => {
                // The in-transaction recheck caught a concurrent create (or a
                // retry that raced the pre-acquire lookup). Don't restart the
                // provider; return the existing snapshot.
                signal_accept(
                    accept,
                    Ok(latte_core::CreateOutcome::Replayed(snapshot.clone())),
                );
                return Ok(latte_core::CreateOutcome::Replayed(snapshot));
            }
            Err(error) => {
                let error = SessionRuntimeError::from(error);
                signal_accept(accept, Err(classify_create_error(&error)));
                return Err(error);
            }
        };
        // The user submission is now durable — acknowledge acceptance.
        signal_accept(
            accept,
            Ok(latte_core::CreateOutcome::Created(started.clone())),
        );
        // Provider construction is runtime work, not submission validation.
        // Once the user card is durable, a missing credential or invalid model
        // becomes a visible retryable child failure instead of restoring the
        // composer and making the accepted prompt appear to vanish.
        let Ok(provider) = (self.provider)(&started.binding) else {
            return self
                .fail_retryable(
                    session_id,
                    turn_id,
                    started.revision,
                    active_turn_revision(&started)?,
                    provider_configuration_failure_message(),
                    &lease,
                )
                .map(latte_core::CreateOutcome::Created);
        };
        self.run_provider_turn(started, messages, provider.provider, lease)
            .await
            .map(latte_core::CreateOutcome::Created)
    }

    /// Creates a child only after the complete history fits exactly. The
    /// completed parent remains untouched in both v1 and v2 tables.
    pub async fn follow_up(
        &self,
        session_id: SessionId,
        expected_session_revision: u64,
        prompt: String,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let runner = self.begin_runner_when_settled(session_id).await?;
        // The legacy in-process path mints a fresh command id so the durable
        // dedup record is still written (it will never replay, but the
        // follow-up contract is uniform).
        let command_id = SessionCommandId::from_uuid(Uuid::now_v7());
        let snapshot = match self
            .follow_up_one(
                session_id,
                expected_session_revision,
                prompt,
                Some(command_id),
                None,
            )
            .await?
        {
            latte_core::CreateOutcome::Created(snapshot)
            | latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
        };
        self.drain_mailbox(snapshot, runner).await
    }

    /// Like [`Self::follow_up`], but signals `accept` once the follow-up
    /// submission is durably persisted, before the provider turn runs. The
    /// signal carries the accepted snapshot wrapped in
    /// [`latte_core::CreateOutcome`]: `Created` (202) or `Replayed` (200,
    /// crash-safe idempotent replay).
    pub async fn follow_up_accepted(
        &self,
        session_id: SessionId,
        command_id: SessionCommandId,
        expected_session_revision: u64,
        prompt: String,
        accept: oneshot::Sender<
            Result<latte_core::CreateOutcome<SessionSnapshot>, latte_core::CreateAcceptError>,
        >,
    ) -> Result<latte_core::CreateOutcome<SessionSnapshot>, SessionRuntimeError> {
        let runner = match self.begin_runner_when_settled(session_id).await {
            Ok(runner) => runner,
            Err(error) => {
                let _ = accept.send(Err(classify_create_error(&error)));
                return Err(error);
            }
        };
        let outcome = self
            .follow_up_one(
                session_id,
                expected_session_revision,
                prompt,
                Some(command_id),
                Some(accept),
            )
            .await?;
        match outcome {
            latte_core::CreateOutcome::Created(snapshot) => {
                let drained = self.drain_mailbox(snapshot, runner).await?;
                Ok(latte_core::CreateOutcome::Created(drained))
            }
            // A replay owns no runner: the accepted submission is already
            // durable and the pre-crash provider task is gone, so there is
            // nothing to drain. Recovery of the expired lease is the
            // sweeper's job.
            latte_core::CreateOutcome::Replayed(snapshot) => {
                Ok(latte_core::CreateOutcome::Replayed(snapshot))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn follow_up_one(
        &self,
        session_id: SessionId,
        expected_session_revision: u64,
        prompt: String,
        command_id: Option<SessionCommandId>,
        accept: Option<
            oneshot::Sender<
                Result<latte_core::CreateOutcome<SessionSnapshot>, latte_core::CreateAcceptError>,
            >,
        >,
    ) -> Result<latte_core::CreateOutcome<SessionSnapshot>, SessionRuntimeError> {
        let snapshot = match self.load_full(session_id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                signal_accept(accept, Err(classify_create_error(&error)));
                return Err(error);
            }
        };
        // Crash-safe idempotency: look up the durable dedup record BEFORE the
        // revision check and lease acquisition. A retry after a crash hits the
        // record and replays without blocking on the dead owner's
        // still-unexpired lease, and without failing the revision fence (the
        // client's expected revision predates the accepted follow-up).
        if let Some(command_id) = &command_id {
            match self.engine.lookup_follow_up_replay(
                command_id,
                session_id,
                expected_session_revision,
                &prompt,
            ) {
                Ok(Some(snapshot)) => {
                    signal_accept(
                        accept,
                        Ok(latte_core::CreateOutcome::Replayed(snapshot.clone())),
                    );
                    return Ok(latte_core::CreateOutcome::Replayed(snapshot));
                }
                Ok(None) => {}
                Err(error) => {
                    let error = SessionRuntimeError::from(error);
                    signal_accept(accept, Err(classify_create_error(&error)));
                    return Err(error);
                }
            }
        }
        if snapshot.revision != expected_session_revision || !snapshot.lifecycle.accepts_follow_up()
        {
            signal_accept(
                accept,
                Err(latte_core::CreateAcceptError::Conflict(
                    SessionRuntimeError::InvalidState.to_string(),
                )),
            );
            return Err(SessionRuntimeError::InvalidState);
        }
        let prepared = match self.prepare_history(&snapshot, &prompt).await {
            Ok(prepared) => prepared,
            Err(error) => {
                signal_accept(accept, Err(classify_create_error(&error)));
                return Err(error);
            }
        };
        let messages = match &prepared {
            PreparedHistory::Complete(messages)
            | PreparedHistory::Degraded(messages)
            | PreparedHistory::Summarized { messages, .. } => messages.clone(),
        };
        let turn_id = new_turn_id();
        let lease = match self.acquire(session_id) {
            Ok(lease) => lease,
            Err(error) => {
                signal_accept(accept, Err(classify_create_error(&error)));
                return Err(error);
            }
        };
        let started = match self.engine.create_started_session_follow_up_v2(
            command_id.as_ref(),
            session_id,
            turn_id,
            expected_session_revision,
            &prompt,
            &lease,
            now_ms(),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let error = SessionRuntimeError::from(error);
                signal_accept(accept, Err(classify_create_error(&error)));
                return Err(error);
            }
        };
        let started = match started {
            latte_core::CreateOutcome::Created(snapshot) => snapshot,
            latte_core::CreateOutcome::Replayed(snapshot) => {
                // The in-transaction recheck caught a concurrent follow-up (or
                // a retry that raced the pre-acquire lookup). Don't restart
                // the provider; return the existing snapshot.
                signal_accept(
                    accept,
                    Ok(latte_core::CreateOutcome::Replayed(snapshot.clone())),
                );
                return Ok(latte_core::CreateOutcome::Replayed(snapshot));
            }
        };
        // The follow-up submission is now durable — acknowledge acceptance.
        signal_accept(
            accept,
            Ok(latte_core::CreateOutcome::Created(started.clone())),
        );
        // Persist the compaction record before the provider turn runs so a
        // restart sees the summary (or its failure audit) the in-flight
        // request was built from. The commit returns the post-append
        // snapshot and the turn continues from it — the append bumps the
        // session revision and every later commit CASes on it. A storage
        // failure here is not fatal: the transcript remains authoritative
        // and the next turn regenerates the summary deterministically from
        // it.
        let started = match &prepared {
            PreparedHistory::Summarized {
                superseded,
                summary,
                ..
            } => self
                .commit(
                    session_id,
                    turn_id,
                    started.revision,
                    active_turn_revision(&started)?,
                    CommitSessionTurnUpdate::AppendTranscript {
                        source_key: format!("{turn_id}:compact-summary"),
                        kind: TranscriptKind::CompactSummary,
                        text: summary.clone(),
                        payload: Some(serde_json::json!({
                            "superseded_through_sequence": superseded.through_sequence,
                        })),
                    },
                    &lease,
                )
                .unwrap_or(started),
            PreparedHistory::Degraded(_) => self
                .commit(
                    session_id,
                    turn_id,
                    started.revision,
                    active_turn_revision(&started)?,
                    CommitSessionTurnUpdate::AppendTranscript {
                        source_key: format!("{turn_id}:compact-summary-failed"),
                        kind: TranscriptKind::System,
                        text: "context compaction failed; continuing without a summary".to_owned(),
                        payload: None,
                    },
                    &lease,
                )
                .unwrap_or(started),
            PreparedHistory::Complete(_) => started,
        };
        let Ok(provider) = (self.provider)(&started.binding) else {
            return self
                .fail_retryable(
                    session_id,
                    turn_id,
                    started.revision,
                    active_turn_revision(&started)?,
                    provider_configuration_failure_message(),
                    &lease,
                )
                .map(latte_core::CreateOutcome::Created);
        };
        self.run_provider_turn(started, messages, provider.provider, lease)
            .await
            .map(latte_core::CreateOutcome::Created)
    }

    /// Enqueues a user turn behind the active turn for this session. The
    /// mailbox is intentionally process-local: accepted prompts are persisted
    /// only when the single session runner reaches the next safe child
    /// boundary. A restart therefore cannot mistake queued work for durable
    /// transcript history.
    pub fn queue_follow_up(
        &self,
        session_id: SessionId,
        prompt: String,
    ) -> Result<usize, SessionRuntimeError> {
        // Eager pre-validation against the base policy only; the
        // authoritative binding-aware budget check happens when the runner
        // reaches the next child boundary.
        let profile = self
            .profiles
            .resolve_base()
            .map_err(|error| SessionRuntimeError::ProviderConfiguration(error.to_string()))?;
        let _ = self.initial_messages(&profile, &prompt, None)?;
        let mut mailboxes = self.mailboxes.lock().expect("mailbox mutex poisoned");
        let mailbox = mailboxes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::InvalidState)?;
        if mailbox.len() >= SESSION_MAILBOX_CAPACITY {
            return Err(SessionRuntimeError::MailboxFull);
        }
        mailbox.push_back(prompt);
        Ok(mailbox.len())
    }

    fn begin_runner(
        &self,
        session_id: SessionId,
    ) -> Result<SessionRunnerGuard, SessionRuntimeError> {
        let mut mailboxes = self.mailboxes.lock().expect("mailbox mutex poisoned");
        if mailboxes.contains_key(&session_id) {
            return Err(SessionRuntimeError::InvalidState);
        }
        mailboxes.insert(session_id, VecDeque::new());
        Ok(SessionRunnerGuard {
            session_id,
            mailboxes: Arc::clone(&self.mailboxes),
            closed: false,
        })
    }

    /// Like [`Self::begin_runner`], but tolerates the teardown residue of a
    /// just-finished turn. The lifecycle `Ready` commit happens inside
    /// `run_provider_turn`, and the runner's `drain_mailbox` removes its
    /// process-local mailbox entry only afterwards — so a follow-up that
    /// observes `Ready` through a snapshot can legitimately race that window
    /// and hit the strict `contains_key` rejection (issue #21). When the
    /// durable snapshot shows an idle `Ready` session, the residue belongs to
    /// a runner that is about to remove it: wait out the teardown instead of
    /// failing the request. Every other state rejects immediately — a live
    /// runner must keep excluding concurrent follow-ups, a non-idle
    /// lifecycle must keep its current error, and residue never belongs to
    /// us, so it is waited out, never taken over.
    async fn begin_runner_when_settled(
        &self,
        session_id: SessionId,
    ) -> Result<SessionRunnerGuard, SessionRuntimeError> {
        let mut waited_ms = 0_u64;
        loop {
            match self.begin_runner(session_id) {
                Ok(runner) => return Ok(runner),
                Err(SessionRuntimeError::InvalidState) if waited_ms < RESIDUE_SETTLE_BUDGET_MS => {
                    // Only `lifecycle` and `active_turn_id` are needed here;
                    // read them through the tail-bounded snapshot instead of
                    // paging the whole transcript on every poll.
                    let Ok(snapshot) = self.engine.session_snapshot_tail_v2(session_id, 1) else {
                        return Err(SessionRuntimeError::InvalidState);
                    };
                    if snapshot.lifecycle != SessionLifecycle::Ready
                        || snapshot.active_turn_id.is_some()
                    {
                        return Err(SessionRuntimeError::InvalidState);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        RESIDUE_SETTLE_INTERVAL_MS,
                    ))
                    .await;
                    waited_ms += RESIDUE_SETTLE_INTERVAL_MS;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn drain_mailbox(
        &self,
        mut snapshot: SessionSnapshot,
        mut runner: SessionRunnerGuard,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        loop {
            let prompt = {
                let mut mailboxes = self.mailboxes.lock().expect("mailbox mutex poisoned");
                let mailbox = mailboxes
                    .get_mut(&snapshot.session_id)
                    .ok_or(SessionRuntimeError::InvalidState)?;
                let prompt = snapshot
                    .lifecycle
                    .accepts_follow_up()
                    .then(|| mailbox.pop_front())
                    .flatten();
                if prompt.is_none() {
                    mailboxes.remove(&snapshot.session_id);
                    runner.mark_closed();
                }
                prompt
            };
            let Some(prompt) = prompt else {
                return Ok(snapshot);
            };
            // Queued mailbox turns are process-local by design; mint a fresh
            // command id so the durable dedup record is still written.
            let command_id = SessionCommandId::from_uuid(Uuid::now_v7());
            snapshot = match self
                .follow_up_one(
                    snapshot.session_id,
                    snapshot.revision,
                    prompt,
                    Some(command_id),
                    None,
                )
                .await?
            {
                latte_core::CreateOutcome::Created(snapshot)
                | latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
            };
        }
    }

    /// Persists an explicit provider/model selection for subsequent children.
    /// Credential resolution remains deferred until the next accepted prompt.
    pub fn switch_model(
        &self,
        session_id: SessionId,
        expected_session_revision: u64,
        binding: &SessionProviderBinding,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        binding
            .validate()
            .map_err(SessionRuntimeError::ProviderConfiguration)?;
        let snapshot = self.load_full(session_id)?;
        if snapshot.revision != expected_session_revision
            || !snapshot.lifecycle.accepts_follow_up()
            || snapshot.active_turn_id.is_some()
        {
            return Err(SessionRuntimeError::InvalidState);
        }
        if snapshot.binding == *binding {
            return Ok(snapshot);
        }
        let lease = self.acquire(session_id)?;
        self.engine
            .switch_session_binding_v2(
                session_id,
                expected_session_revision,
                binding,
                &lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    /// Provides a non-secret request value and continues the same child.
    pub async fn provide_input(
        &self,
        session_id: SessionId,
        expected_session_revision: u64,
        expected_turn_revision: u64,
        request_id: String,
        value: String,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let snapshot = self.load_full(session_id)?;
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let turn = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == turn_id)
            .ok_or(SessionRuntimeError::InvalidState)?;
        if snapshot.lifecycle != SessionLifecycle::WaitingInput
            || snapshot.revision != expected_session_revision
            || turn.turn_revision != expected_turn_revision
        {
            return Err(SessionRuntimeError::InvalidState);
        }
        let prepared = self.prepare_history(&snapshot, &value).await?;
        let messages = match &prepared {
            PreparedHistory::Complete(messages)
            | PreparedHistory::Degraded(messages)
            | PreparedHistory::Summarized { messages, .. } => messages.clone(),
        };
        let provider = (self.provider)(&snapshot.binding)
            .map_err(SessionRuntimeError::ProviderConfiguration)?;
        let lease = self.acquire(session_id)?;
        let running = self.commit(
            session_id,
            turn_id,
            snapshot.revision,
            turn.turn_revision,
            CommitSessionTurnUpdate::ProvideInput {
                source_key: format!("{turn_id}:input:{request_id}"),
                request_id,
                value,
            },
            &lease,
        )?;
        // Same compaction contract as a new child: persist the summary (or
        // its failure audit) before the provider sees the request. The
        // ProvideInput commit advanced both the session revision and the
        // turn revision, so the append CASes on `running`'s fresh values and
        // the turn continues from the commit's returned snapshot — same
        // mechanism as the tool-round appends. A storage failure here is
        // not fatal: the transcript remains authoritative and the next turn
        // regenerates the summary deterministically from it.
        let running = match &prepared {
            PreparedHistory::Summarized {
                superseded,
                summary,
                ..
            } => self
                .commit(
                    session_id,
                    turn_id,
                    running.revision,
                    active_turn_revision(&running)?,
                    CommitSessionTurnUpdate::AppendTranscript {
                        // Distinct from the new-child key: one turn can
                        // compact twice (its own start, then an input
                        // answer), and source keys dedup within a turn.
                        source_key: format!("{turn_id}:compact-summary:input"),
                        kind: TranscriptKind::CompactSummary,
                        text: summary.clone(),
                        payload: Some(serde_json::json!({
                            "superseded_through_sequence": superseded.through_sequence,
                        })),
                    },
                    &lease,
                )
                .unwrap_or(running),
            PreparedHistory::Degraded(_) => self
                .commit(
                    session_id,
                    turn_id,
                    running.revision,
                    active_turn_revision(&running)?,
                    CommitSessionTurnUpdate::AppendTranscript {
                        source_key: format!("{turn_id}:compact-summary-failed:input"),
                        kind: TranscriptKind::System,
                        text: "context compaction failed; continuing without a summary".to_owned(),
                        payload: None,
                    },
                    &lease,
                )
                .unwrap_or(running),
            PreparedHistory::Complete(_) => running,
        };
        self.run_provider_turn(running, messages, provider.provider, lease)
            .await
    }

    /// Resolves a durable v2 effect permission. Allow consumes the exact
    /// prepared approval through the engine Started transaction before any
    /// external operation is invoked; denial terminalizes the prepared effect
    /// without executing it.
    pub async fn resolve_permission(
        &self,
        session_id: SessionId,
        expected_session_revision: u64,
        expected_turn_revision: u64,
        request_id: String,
        allow: bool,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let snapshot = self.load_full(session_id)?;
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let turn_revision = active_turn_revision(&snapshot)?;
        if snapshot.lifecycle != SessionLifecycle::WaitingPermission
            || snapshot.revision != expected_session_revision
            || turn_revision != expected_turn_revision
            || snapshot.pending.as_ref().and_then(|pending| match pending {
                latte_core::SessionPendingRequest::Permission { request_id, .. } => {
                    Some(request_id.as_str())
                }
                latte_core::SessionPendingRequest::Input { .. } => None,
            }) != Some(request_id.as_str())
        {
            return Err(SessionRuntimeError::InvalidState);
        }
        let verification = is_verification_effect_id(&request_id);
        // Validate the immutable Provider binding before approval can start an
        // external effect. A configuration/model mismatch is not authority to
        // consume permission or execute the tool; the Session remains at the
        // same durable waiting boundary so the user can choose another path.
        let provider = if allow && !verification {
            Some(
                (self.provider)(&snapshot.binding)
                    .map_err(SessionRuntimeError::ProviderConfiguration)?,
            )
        } else {
            None
        };
        let lease = self.acquire(session_id)?;
        let resolved = self.engine.resolve_session_effect_permission(
            session_id,
            turn_id,
            snapshot.revision,
            turn_revision,
            request_id.clone(),
            format!(
                "{turn_id}:permission:{request_id}:{}",
                if allow { "allow" } else { "deny" }
            ),
            allow,
            SessionCommandId::from_uuid(Uuid::now_v7()),
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
        let started = self.engine.start_session_effect(
            session_effect_start_request(
                &resolved,
                request_id.clone(),
                format!("{turn_id}:effect:{request_id}:start"),
            )?,
            self.engine.session_effect_digest(&request_id)?,
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
        if after_effect.lifecycle != SessionLifecycle::Running {
            return Ok(after_effect);
        }
        if verification {
            return self.finish_verification(&after_effect, &presentation, &lease);
        }
        let (round_sequence, calls, ordinal) = continuation.ok_or_else(|| {
            SessionRuntimeError::Effect("provider tool continuation is missing".into())
        })?;
        let provider = provider.ok_or_else(|| {
            SessionRuntimeError::ProviderConfiguration(
                "provider was not resolved before effect approval".into(),
            )
        })?;
        let messages = self.history_from_snapshot(&after_effect)?;
        // Finish the remaining calls of this approved batch, then re-enter the
        // iterative turn loop. The loop re-reads the persisted round counter
        // itself, so approval/restart resumptions never reset the budget.
        let outcome = self
            .execute_tool_batch(
                after_effect,
                messages,
                calls,
                ordinal.saturating_add(1),
                round_sequence,
                &lease,
            )
            .await?;
        match outcome {
            ToolBatchOutcome::Parked(parked) => Ok(parked),
            ToolBatchOutcome::Completed { snapshot, messages } => {
                self.run_provider_turn(snapshot, messages, provider.provider, lease)
                    .await
            }
        }
    }

    /// Explicitly resolves an Unknown v2 effect through the v2 commit path.
    pub fn reconcile_unknown_effect(
        &self,
        session_id: SessionId,
        effect_id: &str,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let snapshot = self.load_full(session_id)?;
        let turn_id = snapshot
            .latest_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        if snapshot.lifecycle != SessionLifecycle::ReconciliationRequired {
            return Err(SessionRuntimeError::InvalidState);
        }
        let turn_revision = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == turn_id)
            .map(|turn| turn.turn_revision)
            .ok_or(SessionRuntimeError::InvalidState)?;
        // A reconcile acquires a lease and immediately commits under it with no
        // heartbeat renewal, so it uses the management TTL (floored well above a
        // sub-second turn TTL) to avoid a spurious `LeaseLost` if the acquire →
        // in-transaction recheck window stalls under load.
        let lease = self.acquire_with_ttl(session_id, self.management_ttl())?;
        self.engine
            .reconcile_session_effect_unknown(
                session_id,
                turn_id,
                snapshot.revision,
                turn_revision,
                effect_id.to_owned(),
                format!("{turn_id}:effect:{effect_id}:reconcile"),
                SessionCommandId::from_uuid(Uuid::now_v7()),
                &lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    /// Cancellation is explicit. No composer input has a turn ID before start,
    /// so canceling it is necessarily local and never reaches this method.
    pub fn cancel(&self, session_id: SessionId) {
        if let Some(token) = self
            .active
            .lock()
            .expect("active mutex poisoned")
            .get(&session_id)
        {
            token.cancel();
        }
    }

    /// Cancels a durable waiting/idle active child. An in-flight provider call
    /// is signalled first and commits its own interruption without a partial
    /// assistant card; a waiting request is terminally cancelled immediately.
    ///
    /// The caller's `expected_session_revision`/`expected_turn_revision` are
    /// validated against the authoritative snapshot before any interruption, so
    /// a stale client cannot cancel a newer turn.
    pub fn cancel_durable(
        &self,
        session_id: SessionId,
        expected_session_revision: u64,
        expected_turn_revision: u64,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let snapshot = self.load_full(session_id)?;
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let turn_revision = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == turn_id)
            .ok_or(SessionRuntimeError::InvalidState)?
            .turn_revision;
        // Fence the caller's expectation against the authoritative snapshot
        // before signalling or committing any cancellation.
        if snapshot.revision != expected_session_revision || turn_revision != expected_turn_revision
        {
            return Err(SessionRuntimeError::InvalidState);
        }
        // Hold the active-map lock across the fence recheck and signal so no
        // concurrent state advance can slip between validation and
        // cancellation.
        {
            let active = self.active.lock().expect("active mutex poisoned");
            if let Some(token) = active.get(&session_id) {
                token.cancel();
                drop(active);
                return self.load_full(session_id);
            }
        }
        let lease = self.acquire(session_id)?;
        self.commit(
            session_id,
            turn_id,
            snapshot.revision,
            turn_revision,
            CommitSessionTurnUpdate::Interrupt {
                source_key: format!("{turn_id}:cancel"),
                reconciliation_effect_id: None,
            },
            &lease,
        )
    }

    /// Validates the binding, resolves its harness profile, and builds the
    /// first request messages. Shared preflight for session creation; any
    /// failure is durable-safe (nothing has been persisted yet).
    fn preflight_messages(
        &self,
        binding: &SessionProviderBinding,
        prompt: &str,
        focus: Option<&Path>,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        binding
            .validate()
            .map_err(SessionRuntimeError::ProviderConfiguration)?;
        let profile = self.resolved_profile(binding)?;
        self.initial_messages(&profile, prompt, focus)
    }

    fn initial_messages(
        &self,
        profile: &ResolvedProfile,
        prompt: &str,
        focus: Option<&Path>,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        let policy = profile.history_policy();
        let context = context::build(&self.root, focus, policy.context_cap_bytes)
            .map_err(|error| SessionRuntimeError::History(error.to_string()))?;
        let system = profile
            .system_prompt(&context.text)
            .map_err(|error| SessionRuntimeError::ProviderConfiguration(error.to_string()))?;
        Self::enforce_budget(
            vec![
                Message::System {
                    content: redact_session_text(&system),
                },
                Message::User {
                    content: redact_session_text(prompt),
                },
            ],
            &policy,
        )
    }

    fn history_with_prompt(
        &self,
        snapshot: &SessionSnapshot,
        prompt: &str,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        let profile = self.resolved_profile(&snapshot.binding)?;
        let segments = Self::scan_history_segments(snapshot);
        let window = self.select_history_window(&profile, snapshot, segments, prompt)?;
        Ok(window.assemble(None))
    }

    /// Builds the next child's provider history with context compaction:
    /// when the newest-first window would discard older history and the
    /// profile enables compaction, the discarded range is summarized by a
    /// dedicated bounded provider request and travels as one summary user
    /// message instead of being silently dropped.
    ///
    /// The returned [`PreparedHistory`] tells the caller which durable
    /// compaction record to append once the turn exists: a
    /// [`TranscriptKind::CompactSummary`] card on success, a failure audit
    /// card on degradation. Degradation never blocks the turn — it falls
    /// back to the exact pre-compaction behavior.
    async fn prepare_history(
        &self,
        snapshot: &SessionSnapshot,
        prompt: &str,
    ) -> Result<PreparedHistory, SessionRuntimeError> {
        let profile = self.resolved_profile(&snapshot.binding)?;
        let segments = Self::scan_history_segments(snapshot);
        let window = self.select_history_window(&profile, snapshot, segments, prompt)?;
        let Some(superseded) = window.superseded.as_ref() else {
            return Ok(PreparedHistory::Complete(window.assemble(None)));
        };
        if profile.compaction().strategy == latte_core::CompactionStrategy::Off {
            // Pre-compaction behavior: silent discard.
            return Ok(PreparedHistory::Complete(window.assemble(None)));
        }
        // The durable card is appended after this turn's user entry, so by
        // position it supersedes the prompt too (context-design §3.2): the
        // summary source must therefore include it, or the prompt's exact
        // words would drop out of every later window.
        let mut source = format!(
            "{}\n[user]\n{}\n",
            superseded.text,
            redact_session_text(prompt)
        );
        let source_bound = profile.compaction().max_summary_source_bytes;
        if source.len() > source_bound {
            let mut cut = source_bound;
            while !source.is_char_boundary(cut) {
                cut -= 1;
            }
            source.truncate(cut);
        }
        let Some(summary) = self.summarize_history(snapshot, &source).await else {
            return Ok(PreparedHistory::Degraded(window.assemble(None)));
        };
        let messages = window.assemble(Some(&summary));
        // A large summary can overflow the exact budget; compaction must
        // never make a request less sendable than the plain window, so an
        // overflowing summary degrades exactly like a failed one.
        if Self::enforce_budget(messages.clone(), &window.policy).is_err() {
            return Ok(PreparedHistory::Degraded(window.assemble(None)));
        }
        Ok(PreparedHistory::Summarized {
            messages,
            superseded: superseded.clone(),
            summary,
        })
    }

    /// Runs the dedicated bounded summary request for context compaction:
    /// the profile's `agent.summarize` system prompt plus the byte-bounded
    /// plain text of the discarded history. Any failure — provider error,
    /// timeout, empty reply — yields `None` and the caller degrades.
    async fn summarize_history(
        &self,
        snapshot: &SessionSnapshot,
        source_text: &str,
    ) -> Option<String> {
        let profile = self.resolved_profile(&snapshot.binding).ok()?;
        let instructions = profile.summarize_prompt().ok()?;
        let Ok(provider) = (self.provider)(&snapshot.binding) else {
            return None;
        };
        let cancellation = CancellationToken::new();
        // Reuses the per-session active-cancellation slot: preparation is
        // strictly serial (prepare_history completes before any main request
        // of the same session starts), so the insert/remove cannot race a
        // main request's token. If preparation ever becomes concurrent with
        // a main request, this must move to a distinct key.
        self.active
            .lock()
            .expect("active mutex poisoned")
            .insert(snapshot.session_id, cancellation.clone());
        let completed = provider
            .provider
            .complete(
                ProviderRequest {
                    messages: vec![
                        Message::System {
                            content: instructions,
                        },
                        Message::User {
                            content: source_text.to_owned(),
                        },
                    ],
                    tools: Vec::new(),
                    session_ref: crate::provider::session_ref_for(snapshot.session_id),
                },
                ProviderContext {
                    deadline: Instant::now()
                        + Duration::from_millis(profile.history_policy().provider_timeout_ms),
                    cancellation,
                    events: None,
                },
            )
            .await;
        self.active
            .lock()
            .expect("active mutex poisoned")
            .remove(&snapshot.session_id);
        let message = completed.ok()?.message?;
        let trimmed = message.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(redact_session_text(trimmed))
        }
    }

    fn scan_history_segments(snapshot: &SessionSnapshot) -> Vec<HistorySegment> {
        let mut segments: Vec<HistorySegment> = Vec::new();
        for entry in &snapshot.transcript.entries {
            match entry.kind {
                TranscriptKind::User => segments.push(HistorySegment::new(
                    Some(entry.sequence),
                    vec![Message::User {
                        content: entry.text.clone(),
                    }],
                    format!("[user]\n{}\n", entry.text),
                )),
                TranscriptKind::CompactSummary => {
                    // The summary supersedes every older entry: window
                    // construction never re-enters them. The summary itself
                    // travels as an ordinary user-segment message.
                    segments.clear();
                    segments.push(HistorySegment::new(
                        Some(entry.sequence),
                        vec![Message::User {
                            content: entry.text.clone(),
                        }],
                        format!("[compacted summary]\n{}\n", entry.text),
                    ));
                }
                TranscriptKind::Assistant => {
                    if let Some(segment) = segments.last_mut() {
                        let tool_calls = entry
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("tool_calls"))
                            .and_then(|calls| serde_json::from_value(calls.clone()).ok())
                            .unwrap_or_default();
                        segment.push_message(Message::Assistant {
                            content: Some(entry.text.clone()),
                            tool_calls,
                        });
                        segment
                            .push_text(&format!("[assistant]\n{}\n", entry.text), entry.sequence);
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
                        // A `tool` message is only legal as the answer to a
                        // call the assistant actually made. Engine-initiated
                        // effects — verification above all — are recorded as
                        // tool results with an id we minted, which the model
                        // never declared; replaying one makes every later turn
                        // of the Session a protocol violation the Provider
                        // rejects outright.
                        && declared_tool_call(&segment.messages, tool_call_id)
                    {
                        segment.push_message(Message::Tool {
                            tool_call_id: tool_call_id.into(),
                            name: payload
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned),
                            content: content.into(),
                        });
                        segment.push_text(&format!("[tool]\n{content}\n"), entry.sequence);
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
                        append_denied_tool_results(&mut segment.messages);
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
        segments
    }

    fn select_history_window(
        &self,
        profile: &ResolvedProfile,
        snapshot: &SessionSnapshot,
        mut segments: Vec<HistorySegment>,
        prompt: &str,
    ) -> Result<HistoryWindow, SessionRuntimeError> {
        let policy = profile.history_policy();
        let focus = snapshot.focus.as_deref().map(Path::new);
        let context = context::build(&self.root, focus, policy.context_cap_bytes)
            .map_err(|error| SessionRuntimeError::History(error.to_string()))?;
        let system =
            Message::System {
                content: redact_session_text(&profile.system_prompt(&context.text).map_err(
                    |error| SessionRuntimeError::ProviderConfiguration(error.to_string()),
                )?),
            };
        // The current prompt has no durable card yet — hence no sequence.
        // It is never part of a discarded range: a window that cannot fit
        // it fails with the hard budget error instead.
        segments.push(HistorySegment::new(
            None,
            vec![Message::User {
                content: redact_session_text(prompt),
            }],
            format!("[user]\n{}\n", redact_session_text(prompt)),
        ));
        let budget = policy.budget()?;
        let mut selected: Vec<Message> = Vec::new();
        let mut kept = 0usize;
        for segment in segments.iter().rev() {
            let mut candidate = Vec::with_capacity(selected.len() + segment.messages.len() + 1);
            candidate.push(system.clone());
            candidate.extend(segment.messages.iter().cloned());
            candidate.extend(selected.iter().cloned());
            if wire_bytes(&candidate)? > budget {
                if selected.is_empty() {
                    return Err(SessionRuntimeError::History(
                        "newest complete user segment exceeds the exact request budget".into(),
                    ));
                }
                break;
            }
            let mut next = segment.messages.clone();
            next.extend(selected);
            selected = next;
            kept += 1;
        }
        let superseded = (kept < segments.len()).then(|| {
            // The discarded range is the contiguous older prefix of the
            // chronological segments (the loop drops from the oldest side).
            let discarded = &segments[..segments.len() - kept];
            let through_sequence = discarded
                .iter()
                .filter_map(|segment| segment.max_sequence)
                .max()
                .unwrap_or_default();
            let mut bound = profile.compaction().max_summary_source_bytes;
            let mut parts: Vec<&str> = Vec::new();
            for segment in discarded.iter().rev() {
                if bound == 0 {
                    break;
                }
                let take = segment.text.len().min(bound);
                let mut text = segment.text.as_str();
                if take < text.len() {
                    let mut cut = take;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text = &text[..cut];
                }
                bound -= text.len();
                parts.push(text);
            }
            SupersededHistory {
                text: parts.concat(),
                through_sequence,
            }
        });
        Ok(HistoryWindow {
            system,
            kept: selected,
            policy,
            superseded,
        })
    }

    fn enforce_budget(
        messages: Vec<Message>,
        policy: &SessionHistoryPolicy,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        let bytes = wire_bytes(&messages)?;
        if bytes > policy.budget()? {
            return Err(SessionRuntimeError::History(format!(
                "request is {bytes} bytes and exceeds the exact budget"
            )));
        }
        Ok(messages)
    }

    fn history_from_snapshot(
        &self,
        snapshot: &SessionSnapshot,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        let mut messages = self.history_with_prompt(snapshot, "")?;
        if matches!(messages.last(), Some(Message::User { content }) if content.is_empty()) {
            let _ = messages.pop();
        }
        Ok(messages)
    }

    fn verification_descriptor(
        &self,
        snapshot: &SessionSnapshot,
        summary: &str,
    ) -> Result<SessionEffectDescriptor, SessionRuntimeError> {
        let plan = self.verification.as_ref().ok_or_else(|| {
            SessionRuntimeError::Effect(
                "workspace changed but no configured verification plan is available".into(),
            )
        })?;
        if plan.argv.is_empty() {
            return Err(SessionRuntimeError::Effect(
                "configured verification argv is empty".into(),
            ));
        }
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        Ok(SessionEffectDescriptor {
            effect_id: format!("{SESSION_VERIFICATION_EFFECT_PREFIX}{turn_id}"),
            tool_call_id: format!("verification-{turn_id}"),
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
                "completion_summary": redact_session_text(summary),
            }),
            attempt: 1,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn begin_verification(
        &self,
        snapshot: SessionSnapshot,
        summary: String,
        lease: &SessionLeaseGuard,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let descriptor = self.verification_descriptor(&snapshot, &summary)?;
        let prepared = self.engine.prepare_session_effect(
            session_effect_request(
                &snapshot,
                descriptor.clone(),
                format!("{turn_id}:verification:prepare"),
            )?,
            lease,
            now_ms(),
        )?;
        if prepared.policy == latte_engine::SessionEffectPolicy::Ask {
            return Ok(prepared.snapshot);
        }
        let started = self.engine.start_session_effect(
            session_effect_start_request(
                &prepared.snapshot,
                descriptor.effect_id.clone(),
                format!("{turn_id}:verification:start"),
            )?,
            prepared.operation_digest,
            lease,
            now_ms(),
        )?;
        let presentation = started.presentation.clone();
        let observed = self.execute_and_observe_effect(started, lease).await?;
        if observed.lifecycle != SessionLifecycle::Running {
            return Ok(observed);
        }
        self.finish_verification(&observed, &presentation, lease)
    }

    fn finish_verification(
        &self,
        snapshot: &SessionSnapshot,
        descriptor: &SessionEffectPresentation,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let turn_revision = active_turn_revision(snapshot)?;
        let raw_output =
            effect_provider_result(snapshot, &descriptor.tool_call_id).ok_or_else(|| {
                SessionRuntimeError::Effect("verification observation is missing".into())
            })?;
        let output: latte_engine::ProcessOutput =
            serde_json::from_str(&raw_output).map_err(|_| {
                SessionRuntimeError::Effect(
                    "verification observation is not a process result".into(),
                )
            })?;
        self.engine.record_session_verification(
            turn_id,
            turn_revision,
            &descriptor.effect_id,
            &output,
            lease,
            now_ms(),
        )?;
        if !output.command_succeeded() {
            return self.fail(
                snapshot.session_id,
                turn_id,
                snapshot.revision,
                turn_revision,
                "configured verification failed; evidence was recorded".into(),
                lease,
            );
        }
        let summary = descriptor
            .input
            .get("completion_summary")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                SessionRuntimeError::Effect("verification completion summary is missing".into())
            })?
            .to_owned();
        self.engine
            .complete_session_verified(
                snapshot,
                summary,
                descriptor.effect_id.clone(),
                lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    /// Persists the assistant tool-round card for a provider response and
    /// executes the whole batch. Returns [`ToolBatchOutcome`] so the iterative
    /// turn loop decides whether another provider request is allowed.
    async fn handle_provider_tool_round(
        &self,
        snapshot: SessionSnapshot,
        mut messages: Vec<Message>,
        response: crate::provider::ProviderResponse,
        lease: &SessionLeaseGuard,
    ) -> Result<ToolBatchOutcome, SessionRuntimeError> {
        let session_id = snapshot.session_id;
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let turn_revision = active_turn_revision(&snapshot)?;
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
            let failed = self.fail(
                session_id,
                turn_id,
                snapshot.revision,
                turn_revision,
                "provider tool call ids must match [A-Za-z0-9_-]{1,256}, be unique, known, and object-shaped".into(),
                lease,
            )?;
            return Ok(ToolBatchOutcome::Parked(failed));
        }
        let assistant_text = response.message.clone().unwrap_or_default();
        let first_tool_call_id = response.tool_calls[0].id.clone();
        let tool_calls = response.tool_calls;
        let current = self.commit(
            session_id,
            turn_id,
            snapshot.revision,
            turn_revision,
            CommitSessionTurnUpdate::AppendTranscript {
                source_key: format!("{turn_id}:assistant-tool-round:{first_tool_call_id}"),
                kind: TranscriptKind::Assistant,
                text: assistant_text.clone(),
                // This is intentionally more than display data: it is the
                // durable ordered continuation queue.  A restart while an
                // Ask call waits for approval reloads this exact assistant
                // envelope and completes the remaining calls before another
                // provider request can be made.
                payload: Some(serde_json::json!({"tool_calls":tool_calls.clone()})),
            },
            lease,
        )?;
        messages.push(Message::Assistant {
            content: response.message,
            tool_calls: tool_calls.clone(),
        });
        let round_sequence = current.sequence;
        self.execute_tool_batch(current, messages, tool_calls, 0, round_sequence, lease)
            .await
    }

    /// Executes the remaining calls of one persisted assistant tool batch.
    /// Returns [`ToolBatchOutcome::Completed`] when every call finished and
    /// the turn may issue its next provider request, or
    /// [`ToolBatchOutcome::Parked`] when the turn is parked at a permission
    /// gate or left the running lifecycle (failure, interruption, unknown
    /// reconciliation).
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_batch(
        &self,
        mut current: SessionSnapshot,
        mut messages: Vec<Message>,
        calls: Vec<crate::provider::ToolCall>,
        start_ordinal: usize,
        round_sequence: u64,
        lease: &SessionLeaseGuard,
    ) -> Result<ToolBatchOutcome, SessionRuntimeError> {
        let turn_id = current
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        for (ordinal, call) in calls.into_iter().enumerate().skip(start_ordinal) {
            let descriptor = SessionEffectDescriptor {
                // Provider IDs only need to be unique within one response.
                // Include the durable assistant sequence to prevent a later
                // response reusing an ID from colliding with this effect.
                effect_id: format!(
                    "session-effect:{turn_id}:{round_sequence}:{ordinal}:{}",
                    call.id
                ),
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
                attempt: 1,
            };
            let prepared = self.engine.prepare_session_effect(
                session_effect_request(
                    &current,
                    descriptor.clone(),
                    format!("{turn_id}:effect:{}:{ordinal}:prepare", call.id),
                )?,
                lease,
                now_ms(),
            )?;
            current = prepared.snapshot;
            if prepared.policy == latte_engine::SessionEffectPolicy::Ask {
                return Ok(ToolBatchOutcome::Parked(current));
            }
            let started = self.engine.start_session_effect(
                session_effect_start_request(
                    &current,
                    descriptor.effect_id,
                    format!("{turn_id}:effect:{}:{ordinal}:start", call.id),
                )?,
                prepared.operation_digest,
                lease,
                now_ms(),
            )?;
            current = self.execute_and_observe_effect(started, lease).await?;
            if current.lifecycle != SessionLifecycle::Running {
                return Ok(ToolBatchOutcome::Parked(current));
            }
            let result = effect_provider_result(&current, &call.id).ok_or_else(|| {
                SessionRuntimeError::Effect("missing observed tool result".into())
            })?;
            messages.push(Message::Tool {
                tool_call_id: call.id,
                name: Some(call.name),
                content: result,
            });
        }
        Ok(ToolBatchOutcome::Completed {
            snapshot: current,
            messages,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_and_observe_effect(
        &self,
        started: SessionEffectStarted,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let session_id = started.snapshot.session_id;
        let cancellation = CancellationToken::new();
        self.active
            .lock()
            .expect("active mutex poisoned")
            .insert(session_id, cancellation.clone());
        let execution = self
            .engine
            .execute_started_session_effect(&started, lease, &cancellation);
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
                        self.active.lock().expect("active mutex poisoned").remove(&session_id);
                        return Err(self.recover_lease_loss(&started.snapshot, lease, "started effect"));
                    }
                    heartbeat
                        .as_mut()
                        .reset(tokio::time::Instant::now() + self.heartbeat_interval());
                }
                () = cancellation.cancelled() => {
                    let _ = execution.await;
                    self.active.lock().expect("active mutex poisoned").remove(&session_id);
                    return self.mark_cancelled_started_effect_unknown(&started, lease);
                }
            }
        };
        let cancelled = cancellation.is_cancelled();
        self.active
            .lock()
            .expect("active mutex poisoned")
            .remove(&session_id);
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
                    .observe_session_effect(
                        &started,
                        format!(
                            "{}:effect:{}:observe",
                            started
                                .snapshot
                                .active_turn_id
                                .ok_or(SessionRuntimeError::InvalidState)?,
                            started.presentation.effect_id
                        ),
                        SessionCommandId::from_uuid(Uuid::now_v7()),
                        value,
                        lease,
                        now_ms(),
                    )
                    .map(|observed| observed.snapshot)
                    .map_err(Into::into)
            }
            Err(SessionEffectExecutionError::Certified(error)) => self
                .engine
                .observe_session_effect(
                    &started,
                    format!(
                        "{}:effect:{}:observe-failed",
                        started
                            .snapshot
                            .active_turn_id
                            .ok_or(SessionRuntimeError::InvalidState)?,
                        started.presentation.effect_id
                    ),
                    SessionCommandId::from_uuid(Uuid::now_v7()),
                    latte_engine::SessionEffectObservedValue {
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
            Err(SessionEffectExecutionError::Uncertain(_error)) => self
                .engine
                .mark_session_effect_unknown(
                    &started,
                    format!(
                        "{}:effect:{}:unknown",
                        started
                            .snapshot
                            .active_turn_id
                            .ok_or(SessionRuntimeError::InvalidState)?,
                        started.presentation.effect_id
                    ),
                    SessionCommandId::from_uuid(Uuid::now_v7()),
                    lease,
                    now_ms(),
                )
                .map_err(Into::into),
        }
    }

    /// Reads how many tool rounds one turn has already taken from the engine's
    /// authoritative persisted counter. This must never be reconstructed from
    /// the snapshot passed into a turn: commit responses carry only the
    /// tail-500 transcript page, and once the conversation outbox drains into
    /// the JSONL log the durable cards are gone from the database. Counting
    /// either view undercounts long turns (a 47-round × 6-call probe showed 38
    /// from the tail page) and silently refills the budget across an input
    /// answer, an approval, or a restart. The counter increments in the same
    /// transaction as each assistant tool-round card.
    fn persisted_tool_rounds(&self, turn_id: TurnId) -> Result<u32, SessionRuntimeError> {
        self.engine
            .session_turn_tool_round_count(turn_id)
            .map_err(Into::into)
    }

    async fn run_provider_turn(
        &self,
        mut snapshot: SessionSnapshot,
        mut messages: Vec<Message>,
        provider: Arc<dyn Provider>,
        lease: SessionLeaseGuard,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        // The tool-batch loop is iterative on purpose: each iteration issues
        // one provider request. A recursive tail grew the future stack by one
        // frame per batch, so an unlimited turn on a long real task could
        // overflow the worker thread (observed past ~40 batches in debug).
        loop {
            // The persisted counter is authoritative for the optional round
            // bound: it counts every committed tool batch for this turn and
            // survives input answers, approvals, restarts, the tail-500
            // projection, and outbox draining.
            let turn_id = snapshot
                .active_turn_id
                .ok_or(SessionRuntimeError::InvalidState)?;
            let round = self.persisted_tool_rounds(turn_id)?;
            match self
                .run_provider_step(snapshot, &messages, provider.clone(), &lease, round)
                .await?
            {
                RoundFlow::Done(done) => return Ok(done),
                RoundFlow::Continue {
                    snapshot: next,
                    messages: next_messages,
                } => {
                    snapshot = next;
                    messages = next_messages;
                }
            }
        }
    }

    /// Issues a single provider request and either finishes the turn
    /// ([`RoundFlow::Done`]) or, after the response's tool batch fully
    /// executed, prepares the next request ([`RoundFlow::Continue`]).
    ///
    /// The optional round bound is enforced *after* the model responds, not
    /// before the request: at the bound the model must still be allowed to
    /// read its last tool results and return a final answer; only an attempt
    /// to open another tool batch stops the turn.
    #[allow(clippy::too_many_lines)]
    async fn run_provider_step(
        &self,
        snapshot: SessionSnapshot,
        messages: &[Message],
        provider: Arc<dyn Provider>,
        lease: &SessionLeaseGuard,
        round: u32,
    ) -> Result<RoundFlow, SessionRuntimeError> {
        let mut snapshot = snapshot;
        let session_id = snapshot.session_id;
        let turn_id = snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?;
        let turn_revision = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == turn_id)
            .ok_or(SessionRuntimeError::InvalidState)?
            .turn_revision;
        let policy = self.resolved_profile(&snapshot.binding)?.history_policy();
        let cancellation = CancellationToken::new();
        self.active
            .lock()
            .expect("active mutex poisoned")
            .insert(session_id, cancellation.clone());
        let output = {
            let output = provider.complete(
                ProviderRequest {
                    messages: messages.to_vec(),
                    // Declarations are data only. The provider receives no
                    // capability: every returned call still crosses the
                    // engine-owned prepare/start/observe lifecycle below.
                    tools: self.engine.tool_descriptors(),
                    // Derived, not the Session id itself: stable for every turn
                    // of this Session so a Provider can group them, opaque
                    // enough that it learns no internal identifier.
                    session_ref: crate::provider::session_ref_for(session_id),
                },
                ProviderContext {
                    deadline: Instant::now() + Duration::from_millis(policy.provider_timeout_ms),
                    cancellation: cancellation.clone(),
                    events: self.progress.as_ref().map(|sink| {
                        Arc::new(ProviderProgress {
                            session_id,
                            turn_id,
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
                        if self.engine.renew_lease(lease, now_ms(), self.authority_ttl()).is_err() {
                            cancellation.cancel();
                            let _ = output.await;
                            self.active.lock().expect("active mutex poisoned").remove(&session_id);
                            return Err(self.recover_lease_loss(&snapshot, lease, "provider call"));
                        }
                        heartbeat
                            .as_mut()
                            .reset(tokio::time::Instant::now() + self.heartbeat_interval());
                    }
                    () = cancellation.cancelled() => {
                        // Do not drop an in-flight provider future and race it
                        // against a terminal write. Providers receive the same
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
            .remove(&session_id);
        let finished = match output {
            Err(ProviderError::Cancelled) => self.commit(
                session_id,
                turn_id,
                snapshot.revision,
                turn_revision,
                CommitSessionTurnUpdate::Interrupt {
                    source_key: format!("{turn_id}:provider-cancel"),
                    reconciliation_effect_id: None,
                },
                lease,
            )?,
            Err(error) => self.fail_retryable(
                session_id,
                turn_id,
                snapshot.revision,
                turn_revision,
                format!("provider: {error}"),
                lease,
            )?,
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
                    self.fail(
                        session_id,
                        turn_id,
                        snapshot.revision,
                        turn_revision,
                        "provider requested unsupported secret or invalid input".into(),
                        lease,
                    )?
                } else {
                    self.commit(
                        session_id,
                        turn_id,
                        snapshot.revision,
                        turn_revision,
                        CommitSessionTurnUpdate::RequestInput {
                            source_key: format!("{turn_id}:input-request:{}", input.id),
                            request: latte_core::PendingInput {
                                request_id: input.id,
                                prompt: input.prompt,
                            },
                        },
                        lease,
                    )?
                }
            }
            Ok(response) if !response.tool_calls.is_empty() => {
                // Enforce the optional round bound here, at the decision to
                // open another tool batch — never before the provider
                // request. At the bound the model has already been allowed to
                // read the last batch's results; only continuing to call
                // tools is stopped, so a model that converges on its
                // bound-reaching round finishes normally.
                if policy.max_tool_rounds.is_some_and(|max| round >= max) {
                    let stopped = self.fail_retryable(
                        session_id,
                        turn_id,
                        snapshot.revision,
                        turn_revision,
                        format!(
                            "turn stopped after {} tool rounds without completing;                              raise or remove session.max_tool_rounds, or narrow the task",
                            policy.max_tool_rounds.expect("checked some")
                        ),
                        lease,
                    )?;
                    return Ok(RoundFlow::Done(stopped));
                }
                let next_messages = messages.to_vec();
                match self
                    .handle_provider_tool_round(snapshot, next_messages, response, lease)
                    .await?
                {
                    ToolBatchOutcome::Completed {
                        snapshot: next,
                        messages: batch_messages,
                    } => {
                        return Ok(RoundFlow::Continue {
                            snapshot: next,
                            messages: batch_messages,
                        });
                    }
                    ToolBatchOutcome::Parked(parked) => return Ok(RoundFlow::Done(parked)),
                }
            }
            Ok(response) => {
                let truncated = matches!(
                    response.finish_reason,
                    Some(crate::provider::FinishReason::Length)
                );
                let Some(message) = response.message.filter(|value| !value.trim().is_empty())
                else {
                    let failed = self.fail(
                        session_id,
                        turn_id,
                        snapshot.revision,
                        turn_revision,
                        "provider returned an empty assistant outcome".into(),
                        lease,
                    )?;
                    return Ok(RoundFlow::Done(failed));
                };
                let appended = self.commit(
                    session_id,
                    turn_id,
                    snapshot.revision,
                    turn_revision,
                    CommitSessionTurnUpdate::AppendTranscript {
                        source_key: format!("{turn_id}:assistant-final"),
                        kind: TranscriptKind::Assistant,
                        text: message.clone(),
                        payload: truncated.then(|| serde_json::json!({"truncated":"length"})),
                    },
                    lease,
                )?;
                // A `length` finish means the model stopped at its output cap
                // mid-answer. Persist what arrived, then fail retryably: the
                // partial text is not a completed turn, and treating it as one
                // would record an unfinished answer as success. Retryable keeps
                // the session usable so a follow-up can continue the work.
                if truncated {
                    let stopped = self.fail_retryable(
                        session_id,
                        turn_id,
                        appended.revision,
                        turn_revision,
                        "provider stopped at its output limit before completing the response"
                            .into(),
                        lease,
                    )?;
                    return Ok(RoundFlow::Done(stopped));
                }
                let changed = self.engine.session_turn_changed_files(turn_id)?;
                if !changed.is_empty() {
                    if self.verification.is_none() {
                        let failed = self.fail(
                            session_id,
                            turn_id,
                            appended.revision,
                            turn_revision,
                            "workspace changed but no configured verification plan is available"
                                .into(),
                            lease,
                        )?;
                        return Ok(RoundFlow::Done(failed));
                    }
                    let verified = self.begin_verification(appended, message, lease).await?;
                    return Ok(RoundFlow::Done(verified));
                }
                snapshot = self.commit(
                    session_id,
                    turn_id,
                    appended.revision,
                    turn_revision,
                    CommitSessionTurnUpdate::Complete {
                        source_key: format!("{turn_id}:complete"),
                        handoff: latte_core::Handoff {
                            summary: message,
                            files_changed: Vec::new(),
                            evidence: Vec::new(),
                        },
                    },
                    lease,
                )?;
                return Ok(RoundFlow::Done(snapshot));
            }
        };
        Ok(RoundFlow::Done(finished))
    }

    #[allow(clippy::needless_pass_by_value)]
    fn fail(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        session_revision: u64,
        turn_revision: u64,
        message: String,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        self.fail_with_retryability(
            session_id,
            turn_id,
            session_revision,
            turn_revision,
            &message,
            Retryability::Terminal,
            lease,
        )
    }

    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    fn fail_retryable(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        session_revision: u64,
        turn_revision: u64,
        message: String,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        self.fail_with_retryability(
            session_id,
            turn_id,
            session_revision,
            turn_revision,
            &message,
            Retryability::Retryable,
            lease,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_with_retryability(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        session_revision: u64,
        turn_revision: u64,
        message: &str,
        retryability: Retryability,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        self.commit(
            session_id,
            turn_id,
            session_revision,
            turn_revision,
            CommitSessionTurnUpdate::Fail {
                source_key: format!("{turn_id}:failure"),
                failure: TurnFailure {
                    code: FailureCode::RuntimeFailed,
                    message: redact_session_text(message),
                    retryability,
                },
            },
            lease,
        )
    }

    fn commit(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        expected_session_revision: u64,
        expected_turn_revision: u64,
        update: CommitSessionTurnUpdate,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        let command_id = SessionCommandId::from_uuid(Uuid::now_v7());
        self.engine
            .commit_session_turn_update(
                SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision,
                    expected_turn_revision,
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

    fn acquire(&self, session_id: SessionId) -> Result<SessionLeaseGuard, SessionRuntimeError> {
        self.acquire_with_ttl(session_id, self.authority_ttl())
    }

    fn acquire_with_ttl(
        &self,
        session_id: SessionId,
        ttl_ms: u64,
    ) -> Result<SessionLeaseGuard, SessionRuntimeError> {
        self.engine
            .acquire_session_lease(session_id, now_ms(), ttl_ms)
            .map(|lease| SessionLeaseGuard {
                engine: self.engine.clone(),
                lease,
            })
            .map_err(Into::into)
    }

    fn recover_lease_loss(
        &self,
        snapshot: &SessionSnapshot,
        lease: &Lease,
        phase: &str,
    ) -> SessionRuntimeError {
        let Some(turn_id) = snapshot.active_turn_id else {
            return SessionRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; active linked turn is unavailable"
            ));
        };
        let Some(revision) = snapshot
            .turns
            .iter()
            .find(|turn| turn.turn_id == turn_id)
            .map(|turn| turn.turn_revision)
        else {
            return SessionRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; active linked turn revision is unavailable"
            ));
        };
        match self.engine.recover_session_after_lease_loss(
            snapshot.session_id,
            turn_id,
            lease,
            revision,
            now_ms(),
        ) {
            Ok(SessionLeaseLossRecovery::Recovered(_)) => SessionRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; linked turn requires reconciliation"
            )),
            Ok(SessionLeaseLossRecovery::FencedNoop) => SessionRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; newer owner fenced stale recovery"
            )),
            Ok(SessionLeaseLossRecovery::AlreadyTerminal(_)) => SessionRuntimeError::Effect(
                format!("lease heartbeat lost during {phase}; linked turn already terminal"),
            ),
            Err(error) => SessionRuntimeError::Effect(format!(
                "lease heartbeat lost during {phase}; recovery failed: {error}"
            )),
        }
    }

    fn mark_cancelled_started_effect_unknown(
        &self,
        started: &SessionEffectStarted,
        lease: &Lease,
    ) -> Result<SessionSnapshot, SessionRuntimeError> {
        self.engine
            .mark_session_effect_unknown(
                started,
                format!(
                    "{}:effect:{}:cancelled-after-start",
                    started
                        .snapshot
                        .active_turn_id
                        .ok_or(SessionRuntimeError::InvalidState)?,
                    started.presentation.effect_id
                ),
                SessionCommandId::from_uuid(Uuid::now_v7()),
                lease,
                now_ms(),
            )
            .map_err(Into::into)
    }

    const fn authority_ttl(&self) -> u64 {
        self.lease_ttl_ms
    }

    /// TTL for a one-shot synchronous recovery/management acquisition
    /// (`reconcile_unknown_effect`). Unlike a running turn — which renews on a
    /// heartbeat and so can safely use a short `authority_ttl` — a reconcile
    /// acquires a lease and immediately commits under it with no renewal. A
    /// sub-second turn TTL (used by crash-recovery tests, and clamped as low as
    /// 10ms) could otherwise expire between the acquire and the in-transaction
    /// `expires_at_ms > now` recheck under a scheduler/coverage stall, spuriously
    /// failing the commit with `LeaseLost`. Flooring at 5s removes that race
    /// without ever shortening the production 60s default.
    const fn management_ttl(&self) -> u64 {
        let floor = 5_000;
        if self.lease_ttl_ms > floor {
            self.lease_ttl_ms
        } else {
            floor
        }
    }

    fn heartbeat_interval(&self) -> Duration {
        Duration::from_millis((self.lease_ttl_ms / 3).max(1))
    }

    fn load_full(&self, session_id: SessionId) -> Result<SessionSnapshot, SessionRuntimeError> {
        let mut snapshot = self.engine.session_snapshot_v2(session_id, None, 500)?;
        while snapshot.transcript.has_more {
            let after = snapshot.transcript.next_after;
            let next = self.engine.session_snapshot_v2(session_id, after, 500)?;
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
    session_id: SessionId,
    turn_id: TurnId,
    sink: Arc<dyn SessionProgressSink>,
}
impl ProviderEventSink for ProviderProgress {
    fn observe(&self, event: ProviderEvent) {
        match event {
            ProviderEvent::Attempt { number } => {
                self.sink.observe(
                    self.session_id,
                    SessionTransientProgress::ProviderAttempt {
                        turn_id: self.turn_id,
                        number,
                    },
                );
            }
            ProviderEvent::AssistantDelta { text } => {
                self.sink.observe(
                    self.session_id,
                    SessionTransientProgress::AssistantDelta {
                        turn_id: self.turn_id,
                        text: redact_session_text(&text),
                    },
                );
            }
        }
    }
}

fn wire_bytes(messages: &[Message]) -> Result<usize, SessionRuntimeError> {
    serde_json::to_vec(messages)
        .map(|bytes| bytes.len())
        .map_err(|error| SessionRuntimeError::History(error.to_string()))
}

fn new_turn_id() -> TurnId {
    TurnId::from_uuid(Uuid::now_v7())
}

/// Sends a durable-acceptance signal if a receiver is present, ignoring a
/// dropped receiver (the caller may have stopped awaiting acceptance).
fn signal_accept<T, E>(accept: Option<oneshot::Sender<Result<T, E>>>, result: Result<T, E>) {
    if let Some(sender) = accept {
        let _ = sender.send(result);
    }
}

/// Classifies a create/follow-up acceptance error for the HTTP layer: a
/// durable command-id reuse with a different payload is a 422 idempotency
/// mismatch; a non-replay create for an existing session, or a stale revision
/// / invalid state, is a 409 conflict; everything else is a 500.
fn classify_create_error(error: &SessionRuntimeError) -> latte_core::CreateAcceptError {
    match error {
        SessionRuntimeError::Storage(latte_engine::StorageError::SessionCommandReplayMismatch) => {
            latte_core::CreateAcceptError::IdempotencyMismatch(error.to_string())
        }
        SessionRuntimeError::Storage(latte_engine::StorageError::SessionAlreadyExists(_))
        | SessionRuntimeError::InvalidState => {
            latte_core::CreateAcceptError::Conflict(error.to_string())
        }
        _ => latte_core::CreateAcceptError::Failed(error.to_string()),
    }
}

fn active_turn_revision(snapshot: &SessionSnapshot) -> Result<u64, SessionRuntimeError> {
    let turn_id = snapshot
        .active_turn_id
        .ok_or(SessionRuntimeError::InvalidState)?;
    snapshot
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)
        .map(|turn| turn.turn_revision)
        .ok_or(SessionRuntimeError::InvalidState)
}

fn session_effect_request(
    snapshot: &SessionSnapshot,
    descriptor: SessionEffectDescriptor,
    source_key: String,
) -> Result<SessionEffectRequest, SessionRuntimeError> {
    Ok(SessionEffectRequest {
        session_id: snapshot.session_id,
        turn_id: snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?,
        expected_session_revision: snapshot.revision,
        expected_turn_revision: active_turn_revision(snapshot)?,
        command_id: SessionCommandId::from_uuid(Uuid::now_v7()),
        source_key,
        descriptor,
    })
}

fn session_effect_start_request(
    snapshot: &SessionSnapshot,
    effect_id: String,
    source_key: String,
) -> Result<SessionEffectStartRequest, SessionRuntimeError> {
    Ok(SessionEffectStartRequest {
        session_id: snapshot.session_id,
        turn_id: snapshot
            .active_turn_id
            .ok_or(SessionRuntimeError::InvalidState)?,
        expected_session_revision: snapshot.revision,
        expected_turn_revision: active_turn_revision(snapshot)?,
        command_id: SessionCommandId::from_uuid(Uuid::now_v7()),
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
    snapshot: &SessionSnapshot,
    tool_call_id: &str,
) -> Result<(u64, Vec<crate::provider::ToolCall>, usize), SessionRuntimeError> {
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
            SessionRuntimeError::Effect(
                "prepared tool call has no durable provider round continuation".into(),
            )
        })
}

fn effect_provider_result(snapshot: &SessionSnapshot, tool_call_id: &str) -> Option<String> {
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
        /// When set, the Nth request (zero-based) fails with a transport
        /// error instead of consuming a scripted response — used to force
        /// summary-request failures in the compaction tests.
        fail_request_index: Mutex<Option<usize>>,
    }

    impl RecordingProvider {
        fn scripted(values: impl IntoIterator<Item = ProviderResponse>) -> Self {
            Self {
                responses: Mutex::new(values.into_iter().collect()),
                requests: Arc::new(Mutex::new(Vec::new())),
                fail_request_index: Mutex::new(None),
            }
        }

        fn fail_request(&self, index: usize) {
            *self.fail_request_index.lock().unwrap() = Some(index);
        }
    }

    impl Provider for RecordingProvider {
        fn complete(
            &self,
            request: ProviderRequest,
            _: ProviderContext,
        ) -> crate::provider::ProviderFuture<'_> {
            let index = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request.messages);
                requests.len() - 1
            };
            let forced_failure = *self.fail_request_index.lock().unwrap() == Some(index);
            let response = if forced_failure {
                None
            } else {
                self.responses.lock().unwrap().pop_front()
            };
            Box::pin(async move {
                response
                    .ok_or_else(|| ProviderError::Malformed("recording provider exhausted".into()))
            })
        }
    }

    fn test_turn_revision(snapshot: &SessionSnapshot) -> u64 {
        snapshot
            .active_turn_id
            .and_then(|turn_id| {
                snapshot
                    .turns
                    .iter()
                    .find(|r| r.turn_id == turn_id)
                    .map(|r| r.turn_revision)
            })
            .unwrap_or(0)
    }

    fn binding() -> SessionProviderBinding {
        SessionProviderBinding {
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

    fn simple_response(message: &str) -> ProviderResponse {
        ProviderResponse {
            message: Some(message.into()),
            tool_calls: Vec::new(),
            input_request: None,
            usage: crate::provider::ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        }
    }

    #[tokio::test]
    async fn session_runner_drains_bounded_mailbox_in_fifo_order() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(DelayedProvider::scripted(
            std::iter::once((Duration::from_millis(100), simple_response("answer-0"))).chain(
                (1..=SESSION_MAILBOX_CAPACITY)
                    .map(|index| (Duration::ZERO, simple_response(&format!("answer-{index}")))),
            ),
        ));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let task_service = service.clone();
        let task = tokio::spawn(async move {
            task_service
                .start(session_id, "prompt-0".into(), binding(), None)
                .await
        });
        tokio::task::yield_now().await;

        for index in 1..=SESSION_MAILBOX_CAPACITY {
            assert_eq!(
                service
                    .queue_follow_up(session_id, format!("prompt-{index}"))
                    .unwrap(),
                index
            );
        }
        assert!(matches!(
            service.queue_follow_up(session_id, "overflow".into()),
            Err(SessionRuntimeError::MailboxFull)
        ));
        assert!(matches!(
            service
                .follow_up(session_id, 0, "parallel runner".into())
                .await,
            Err(SessionRuntimeError::InvalidState)
        ));

        let completed = task.await.unwrap().unwrap();
        assert_eq!(completed.turns.len(), SESSION_MAILBOX_CAPACITY + 1);
        let users = completed
            .transcript
            .entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::User)
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            users,
            (0..=SESSION_MAILBOX_CAPACITY)
                .map(|index| format!("prompt-{index}"))
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            service.queue_follow_up(session_id, "too late".into()),
            Err(SessionRuntimeError::InvalidState)
        ));
    }

    #[tokio::test]
    async fn runners_for_different_sessions_make_progress_concurrently() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(DelayedProvider::scripted([
            (Duration::from_millis(150), simple_response("one")),
            (Duration::from_millis(150), simple_response("two")),
            (Duration::ZERO, simple_response("three")),
            (Duration::ZERO, simple_response("four")),
        ]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let first = service.clone();
        let second = service.clone();
        let left_id = SessionId::from_uuid(Uuid::now_v7());
        let right_id = SessionId::from_uuid(Uuid::now_v7());
        let left =
            tokio::spawn(async move { first.start(left_id, "left".into(), binding(), None).await });
        let right = tokio::spawn(async move {
            second
                .start(right_id, "right".into(), binding(), None)
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(
            service
                .queue_follow_up(left_id, "left queued".into())
                .unwrap(),
            1
        );
        assert_eq!(
            service
                .queue_follow_up(right_id, "right queued".into())
                .unwrap(),
            1
        );
        let (left, right) = tokio::join!(left, right);
        assert!(left.unwrap().is_ok() && right.unwrap().is_ok());
    }

    #[tokio::test]
    async fn accepted_start_and_follow_up_reject_while_runner_is_active() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        // A delayed provider keeps the runner active while the turn is
        // running, so a concurrent start/follow_up must fail with
        // InvalidState (the mailbox is held by the active runner).
        let service = delayed_service(
            root.path(),
            engine,
            [(Duration::from_millis(300), simple_response("done"))],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let first = service.clone();
        let running = tokio::spawn(async move {
            first
                .start(session_id, "slow turn".into(), binding(), None)
                .await
        });
        // Let the first turn enter the provider call so its runner is active.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // While the runner is active, start_accepted must reject with
        // InvalidState and signal the accept channel.
        let (accept_tx, accept_rx) = tokio::sync::oneshot::channel();
        let err = service
            .start_accepted(
                session_id,
                SessionCommandId::from_uuid(Uuid::now_v7()),
                "second start".into(),
                binding(),
                None,
                accept_tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::InvalidState));
        accept_rx
            .await
            .expect("accept channel must be signalled")
            .expect_err("accept must carry the error");

        // follow_up_accepted must also reject while the runner is active.
        let (accept_tx, accept_rx) = tokio::sync::oneshot::channel();
        let err = service
            .follow_up_accepted(
                session_id,
                SessionCommandId::from_uuid(Uuid::now_v7()),
                0,
                "second follow-up".into(),
                accept_tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::InvalidState));
        accept_rx
            .await
            .expect("accept channel must be signalled")
            .expect_err("accept must carry the error");

        // The original turn completes successfully.
        let completed = running.await.unwrap().unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
    }

    #[tokio::test]
    async fn start_provider_configuration_failure_is_durable_and_retryable() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let factory: SessionProviderFactory =
            Arc::new(|_| Err("missing environment variable PROVIDER_SECRET_NAME".into()));
        let service = SessionRuntimeService::new(
            engine.clone(),
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());

        let failed = service
            .start(session_id, "durable prompt".into(), binding(), None)
            .await
            .unwrap();

        assert_eq!(failed.lifecycle, SessionLifecycle::Ready);
        assert!(failed.active_turn_id.is_none());
        assert_eq!(failed.turns.len(), 1);
        assert_eq!(
            failed.turns[0].status,
            latte_core::SessionTurnStatus::Failed
        );
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
            engine.show(failed.turns[0].turn_id).unwrap().failure,
            Some(TurnFailure {
                code: FailureCode::RuntimeFailed,
                message: expected_failure,
                retryability: Retryability::Retryable,
            })
        );
        assert_eq!(engine.list_sessions().unwrap(), [failed]);
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
        let factory: SessionProviderFactory = Arc::new(move |_| {
            if factory_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Err("secret reference PROVIDER_SECRET_NAME is unavailable".into())
            } else {
                Ok(ResolvedProvider {
                    provider: provider.clone(),
                    binding: crate::registry::ProviderBinding::direct(&[]),
                })
            }
        });
        let service = SessionRuntimeService::new(
            engine.clone(),
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let complete = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();

        let failed = service
            .follow_up(session_id, complete.revision, "durable follow-up".into())
            .await
            .unwrap();

        assert_eq!(failed.lifecycle, SessionLifecycle::Ready);
        assert_eq!(failed.turns.len(), 2);
        assert_eq!(
            failed.turns[0].status,
            latte_core::SessionTurnStatus::Completed
        );
        assert_eq!(
            failed.turns[1].status,
            latte_core::SessionTurnStatus::Failed
        );
        assert!(failed.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::User && entry.text == "durable follow-up"
        }));
        assert!(failed.transcript.entries.iter().any(|entry| {
            entry.kind == TranscriptKind::Failure
                && entry.text == provider_configuration_failure_message()
        }));

        let retried = service
            .follow_up(session_id, failed.revision, "retry after config fix".into())
            .await
            .unwrap();
        assert_eq!(retried.lifecycle, SessionLifecycle::Ready);
        assert_eq!(retried.turns.len(), 3);
        assert_eq!(
            retried.turns[2].status,
            latte_core::SessionTurnStatus::Completed
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
        let factory: SessionProviderFactory = Arc::new(|_| {
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
        let service = SessionRuntimeService::new(
            engine.clone(),
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let complete = service
            .start(session_id, "one".into(), binding(), None)
            .await
            .unwrap();
        let parent = complete.latest_turn_id.unwrap();
        let child = service
            .follow_up(session_id, complete.revision, "two".into())
            .await
            .unwrap();
        assert_eq!(child.lifecycle, SessionLifecycle::Ready);
        assert_eq!(
            engine.show(parent).unwrap().status,
            latte_core::TurnStatus::Completed
        );
        assert_eq!(child.turns.len(), 2);
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
    ) -> SessionRuntimeService {
        scripted_service_with_policy(root, engine, responses, SessionHistoryPolicy::default())
    }

    fn scripted_service_with_policy(
        root: &std::path::Path,
        engine: EngineHandle,
        responses: Vec<ProviderResponse>,
        policy: SessionHistoryPolicy,
    ) -> SessionRuntimeService {
        let provider = Arc::new(FakeProvider::scripted(responses));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        SessionRuntimeService::new(engine, root, policy, factory)
    }

    /// A `length` finish is a mid-answer stop at the model's output cap. The
    /// partial text must survive for the user to read, but the turn must not
    /// be recorded as completed, or an unfinished answer counts as success.
    #[tokio::test]
    async fn length_finish_persists_the_partial_answer_and_fails_retryably() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let mut cut = response(Some("half of an ans"), vec![]);
        cut.finish_reason = Some(crate::provider::FinishReason::Length);
        let service = scripted_service(root.path(), engine, vec![cut]);
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let snapshot = service
            .start(session_id, "write something long".into(), binding(), None)
            .await
            .unwrap();

        // Retryable, not terminal: the session still accepts a follow-up that
        // continues the work.
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Ready);
        assert!(snapshot.lifecycle.accepts_follow_up());
        let latest = snapshot.turns.last().expect("one run");
        assert_eq!(latest.status, latte_core::SessionTurnStatus::Failed);

        let assistant = snapshot
            .transcript
            .entries
            .iter()
            .find(|entry| entry.kind == TranscriptKind::Assistant)
            .expect("the partial answer is persisted");
        assert_eq!(assistant.text, "half of an ans");
        assert_eq!(
            assistant.payload.as_ref().and_then(|p| p.get("truncated")),
            Some(&serde_json::json!("length")),
            "the card must say why it stopped"
        );
        assert!(
            snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::Failure
                    && entry.text.contains("output limit")),
            "the user must see the reason, not a silently short answer"
        );
    }

    /// Every other finish reason completes normally; only `length` is a stop.
    #[tokio::test]
    async fn non_length_finish_reasons_complete_the_turn() {
        for reason in [
            None,
            Some(crate::provider::FinishReason::Stop),
            Some(crate::provider::FinishReason::Other("eos".into())),
        ] {
            let root = tempfile::tempdir().unwrap();
            let engine = EngineBuilder::new()
                .workspace_root(root.path())
                .build()
                .unwrap();
            let mut done = response(Some("a complete answer"), vec![]);
            done.finish_reason = reason.clone();
            let service = scripted_service(root.path(), engine, vec![done]);
            let session_id = SessionId::from_uuid(Uuid::now_v7());
            let snapshot = service
                .start(session_id, "ask".into(), binding(), None)
                .await
                .unwrap();
            let latest = snapshot.turns.last().expect("one run");
            assert_eq!(
                latest.status,
                latte_core::SessionTurnStatus::Completed,
                "finish_reason {reason:?} must not block completion"
            );
            let assistant = snapshot
                .transcript
                .entries
                .iter()
                .find(|entry| entry.kind == TranscriptKind::Assistant)
                .expect("assistant card");
            assert!(
                assistant.payload.is_none(),
                "an untruncated answer carries no truncation marker"
            );
        }
    }

    fn delayed_service(
        root: &std::path::Path,
        engine: EngineHandle,
        responses: impl IntoIterator<Item = (Duration, ProviderResponse)>,
    ) -> SessionRuntimeService {
        let provider = Arc::new(DelayedProvider::scripted(responses));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        SessionRuntimeService::new(engine, root, SessionHistoryPolicy::default(), factory)
    }

    fn recording_service(
        root: &std::path::Path,
        engine: EngineHandle,
        provider: Arc<RecordingProvider>,
    ) -> SessionRuntimeService {
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        SessionRuntimeService::new(engine, root, SessionHistoryPolicy::default(), factory)
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
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "initial".into(), binding(), None)
            .await
            .unwrap();

        assert_eq!(
            service
                .switch_model(session_id, ready.revision, &ready.binding)
                .unwrap(),
            ready
        );
        let mut invalid = binding();
        invalid.provider_name.clear();
        assert!(matches!(
            service.switch_model(session_id, ready.revision, &invalid),
            Err(SessionRuntimeError::ProviderConfiguration(_))
        ));
        let mut next = binding();
        next.provider_name = "other".into();
        next.model = "reasoning".into();
        next.config_fingerprint = "other-config".into();
        assert!(matches!(
            service.switch_model(session_id, ready.revision + 1, &next),
            Err(SessionRuntimeError::InvalidState)
        ));

        let switched = service
            .switch_model(session_id, ready.revision, &next)
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
                SessionId::from_uuid(Uuid::now_v7()),
                "read it".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Ready);
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
                SessionId::from_uuid(Uuid::now_v7()),
                "run a failed process".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let terminal = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(terminal.lifecycle, SessionLifecycle::ReconciliationRequired);
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
                .reconcile_unknown_effect(terminal.session_id, effect_id)
                .unwrap()
                .lifecycle,
            SessionLifecycle::Failed
        );
    }

    #[test]
    fn management_ttl_floors_sub_second_turn_ttls_for_reconcile() {
        // Deterministic guard for the reconcile `LeaseLost` fix. A running turn
        // renews on a heartbeat, so it may use a short `authority_ttl`; a
        // reconcile acquires a lease and immediately commits under it with no
        // renewal, so it must use a `management_ttl` floored well above a
        // sub-second turn TTL. Otherwise the just-acquired lease can read as
        // expired at the in-transaction `expires_at_ms > now` recheck under a
        // scheduler/coverage stall and spuriously fail with `LeaseLost`.
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();

        // Production default (60s) is unchanged for both paths.
        let default = scripted_service(root.path(), engine.clone(), vec![]);
        assert_eq!(default.authority_ttl(), 60_000);
        assert_eq!(default.management_ttl(), 60_000);

        // A 300ms turn TTL (the value crash-recovery tests use) keeps the turn
        // heartbeat tight but floors the no-renewal reconcile to 5s.
        let short = scripted_service(root.path(), engine.clone(), vec![]).with_lease_ttl_ms(300);
        assert_eq!(short.authority_ttl(), 300);
        assert_eq!(short.management_ttl(), 5_000);

        // Even the minimum clamped turn TTL (10ms) floors identically.
        let minimum = scripted_service(root.path(), engine, vec![]).with_lease_ttl_ms(10);
        assert_eq!(minimum.authority_ttl(), 10);
        assert_eq!(minimum.management_ttl(), 5_000);
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
                SessionId::from_uuid(Uuid::now_v7()),
                "create it".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let done = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(done.lifecycle, SessionLifecycle::Ready);
        assert_eq!(
            std::fs::read_to_string(root.path().join("created.txt")).unwrap(),
            "const token=value;\n"
        );
        assert!(
            done.turns
                .iter()
                .any(|run| run.status == latte_core::SessionTurnStatus::Completed)
        );
        let turn_id = done.latest_turn_id.unwrap();
        let handoff = engine.show(turn_id).unwrap().handoff.unwrap();
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
            .expect("completed session snapshot projects the redacted handoff");
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
        let database = root.path().join("session.db");
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
                SessionId::from_uuid(Uuid::now_v7()),
                "write and inspect source".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let waiting_json = serde_json::to_string(&waiting).unwrap();
        assert!(!waiting_json.contains("live-secret-value"));
        assert!(!waiting_json.contains("token=value"));
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let completed = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id.clone(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
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
            ("conversation_outbox", "entry_json"),
            ("runtime_checkpoints", "payload_json"),
            ("session_command_dedup", "result_json"),
        ] {
            let (table, column) = table_and_column;
            let query = format!("SELECT COALESCE(group_concat({column}, '\\n'), '') FROM {table}");
            let durable: String = connection.query_row(&query, [], |row| row.get(0)).unwrap();
            assert!(!durable.contains("live-secret-value"), "{table}.{column}");
            assert!(!durable.contains("token=value"), "{table}.{column}");
        }
        let canonical: String = connection
            .query_row(
                "SELECT descriptor_json FROM session_effect_canonical WHERE effect_id=?1",
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
                SessionId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        assert!(request_id.contains("call_ask-write"));
        assert!(!request_id.contains("[REDACTED]"));
        let completed = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
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
                SessionId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let completed = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
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
                SessionId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let denied = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                false,
            )
            .await
            .unwrap();
        assert_eq!(denied.lifecycle, SessionLifecycle::Ready);
        assert!(denied.active_turn_id.is_none());
        assert!(denied.pending.is_none());
        assert!(!root.path().join("new.txt").exists());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        let continued = service
            .follow_up(
                denied.session_id,
                denied.revision,
                "continue without those tools".into(),
            )
            .await
            .unwrap();
        assert_eq!(continued.lifecycle, SessionLifecycle::Ready);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_result_ids(&requests[1]), ["ask-write", "allowed-read"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v2_tool_round_resume_after_restart_uses_durable_assistant_queue() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("session.db");
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
                SessionId::from_uuid(Uuid::now_v7()),
                "perform the round".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        assert!(request_id.contains("call_ask-write"));
        assert!(!request_id.contains("[REDACTED]"));
        drop(first);
        let resumed = recording_service(root.path(), engine, provider.clone())
            .with_verification(passing_verification())
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(resumed.lifecycle, SessionLifecycle::Ready);
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
                SessionId::from_uuid(Uuid::now_v7()),
                "read".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let handoff = engine
            .show(complete.latest_turn_id.unwrap())
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
                SessionId::from_uuid(Uuid::now_v7()),
                "write".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let failed = mutated
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
        assert!(
            engine
                .show(failed.latest_turn_id.unwrap())
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
                SessionId::from_uuid(Uuid::now_v7()),
                "write".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let failed = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
        let run = engine.show(failed.latest_turn_id.unwrap()).unwrap();
        assert_eq!(run.status, latte_core::TurnStatus::Failed);
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
                SessionId::from_uuid(Uuid::now_v7()),
                "write".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let tool_request = match waiting_tool.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let waiting_verification = service
            .resolve_permission(
                waiting_tool.session_id,
                waiting_tool.revision,
                test_turn_revision(&waiting_tool),
                tool_request,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            waiting_verification.lifecycle,
            SessionLifecycle::WaitingPermission
        );
        let verification_request = match waiting_verification.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => {
                panic!("expected verification permission")
            }
        };
        let complete = service
            .resolve_permission(
                waiting_verification.session_id,
                waiting_verification.revision,
                test_turn_revision(&waiting_verification),
                verification_request,
                true,
            )
            .await
            .unwrap();
        assert_eq!(complete.lifecycle, SessionLifecycle::Ready);
        assert_eq!(
            engine
                .show(complete.latest_turn_id.unwrap())
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
                SessionId::from_uuid(Uuid::now_v7()),
                "choose a language".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingInput);
        let completed = service
            .provide_input(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                "language".into(),
                "Rust".into(),
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
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
                    waiting.session_id,
                    waiting.revision,
                    test_turn_revision(&waiting),
                    "language".into(),
                    "again".into()
                )
                .await,
            Err(SessionRuntimeError::InvalidState)
        ));
        assert_eq!(
            engine
                .session_snapshot_v2(completed.session_id, None, 100)
                .unwrap()
                .lifecycle,
            SessionLifecycle::Ready
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
                    SessionId::from_uuid(Uuid::now_v7()),
                    "must fail closed".into(),
                    binding(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
            assert!(failed.active_turn_id.is_none());
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
        let service = recording_service(root.path(), engine, Arc::new(RecordingProvider::scripted([]))).with_progress_sink(Arc::new(|_, _| {}));
        let failed = service.start(SessionId::from_uuid(Uuid::now_v7()), "provider error".into(), binding(), None).await.unwrap();
        assert_eq!(failed.lifecycle, SessionLifecycle::Ready); assert!(failed.active_turn_id.is_none()); assert!(failed.transcript.entries.iter().any(|entry| entry.kind == TranscriptKind::Failure)); let mut invalid_binding = binding(); invalid_binding.provider_name.clear(); assert!(matches!(service.start(SessionId::from_uuid(Uuid::now_v7()), "invalid binding".into(), invalid_binding, None).await, Err(SessionRuntimeError::ProviderConfiguration(_)))); let missing = SessionId::from_uuid(Uuid::now_v7()); assert!(service.follow_up(missing, 0, "missing".into()).await.is_err()); assert!(service.provide_input(missing, 0, 0, "missing".into(), "value".into()).await.is_err()); assert!(service.resolve_permission(missing, 0, 0, "missing".into(), false).await.is_err()); assert!(service.reconcile_unknown_effect(missing, "missing").is_err()); assert!(service.cancel_durable(missing, 0, 0).is_err());

        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new().workspace_root(root.path()).build().unwrap();
        let service = delayed_service(
            root.path(),
            engine,
            [(Duration::from_mins(1), response(Some("unused"), vec![]))],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let cancel = async { wait_until(|| service.active.lock().unwrap().contains_key(&session_id), "provider registration").await; service.cancel(session_id); };
        let (cancelled, ()) = tokio::join!(service.start(session_id, "cancel provider".into(), binding(), None), cancel);
        assert_eq!(cancelled.unwrap().lifecycle, SessionLifecycle::Interrupted);
    }

    #[tokio::test]
    async fn v2_rejects_secret_shaped_provider_tool_id_before_any_durable_tool_state() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("session.db");
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
                SessionId::from_uuid(Uuid::now_v7()),
                "reject unsafe provider identity".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
        assert!(failed.active_turn_id.is_none());
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
            ("session_effect_canonical", "descriptor_json"),
            ("pending_permissions", "effect_id"),
            ("conversation_outbox", "entry_json"),
            ("session_events", "event_json"),
            ("runtime_checkpoints", "payload_json"),
            ("session_command_dedup", "result_json"),
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
        let database = root.path().join("session.db");
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
                SessionId::from_uuid(Uuid::now_v7()),
                "reject unsafe input identity".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
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
                "SELECT COUNT(*) FROM session_commit_sources WHERE source_key LIKE '%:input-request:%'",
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
                "SELECT COUNT(*) FROM turns WHERE json_extract(state_json, '$.pending_input') IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending_inputs, 0,
            "unsafe input must not create durable pending state"
        );
        for (table, column) in [
            ("conversation_outbox", "entry_json"),
            ("session_events", "event_json"),
            ("session_command_dedup", "digest"),
            ("session_command_dedup", "result_json"),
            ("session_commit_sources", "source_key"),
            ("session_commit_sources", "digest"),
            ("session_commit_sources", "result_json"),
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
            SessionHistoryPolicy {
                max_request_bytes: 0,
                ..SessionHistoryPolicy::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SessionHistoryPolicy {
                max_request_bytes: 10,
                max_input_bytes: 10,
                reserved_output_bytes: 10,
                context_cap_bytes: 1,
                ..SessionHistoryPolicy::default()
            }
            .validate()
            .is_err()
        );

        let received = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<dyn SessionProgressSink> = {
            let received = Arc::clone(&received);
            Arc::new(move |_session_id, progress| received.lock().unwrap().push(progress))
        };
        let turn_id = TurnId::from_uuid(Uuid::now_v7());
        let progress = ProviderProgress {
            session_id: SessionId::from_uuid(Uuid::now_v7()),
            turn_id,
            sink,
        };
        progress.observe(ProviderEvent::Attempt { number: 2 });
        progress.observe(ProviderEvent::AssistantDelta {
            text: "ok\u{1b}[31m sk-hidden".into(),
        });
        assert!(matches!(
            received.lock().unwrap().as_slice(),
            [
                SessionTransientProgress::ProviderAttempt { turn_id: observed, number: 2 },
                SessionTransientProgress::AssistantDelta { text, .. },
            ] if *observed == turn_id && !text.contains("sk-hidden")
        ));
        assert_eq!(
            SessionRuntimeService::new(
                EngineBuilder::new().build().unwrap(),
                std::env::current_dir().unwrap(),
                SessionHistoryPolicy::default(),
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
                SessionId::from_uuid(Uuid::now_v7()),
                "read for history".into(),
                binding(),
                None,
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

        let constrained = SessionRuntimeService::new(
            EngineBuilder::new()
                .workspace_root(root.path())
                .build()
                .unwrap(),
            root.path(),
            SessionHistoryPolicy {
                max_request_bytes: 4096,
                max_input_bytes: 4096,
                reserved_output_bytes: 1,
                context_cap_bytes: 1,
                ..SessionHistoryPolicy::default()
            },
            Arc::new(|_| Err("not used".into())),
        );
        let mut bounded = completed.clone(); bounded.transcript.entries.iter_mut().find(|entry| entry.kind == TranscriptKind::User).unwrap().text = "word ".repeat(1_000);
        assert!(constrained.history_with_prompt(&bounded, "small").unwrap().iter().any(|message| matches!(message, Message::User { content } if content == "small")));
        assert!(SessionRuntimeService::enforce_budget(vec![Message::User { content: "word ".repeat(1_000) }], &SessionHistoryPolicy {
                max_request_bytes: 4096,
                max_input_bytes: 4096,
                reserved_output_bytes: 1,
                context_cap_bytes: 1,
                ..SessionHistoryPolicy::default()
            }).is_err());
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
                SessionId::from_uuid(Uuid::now_v7()),
                "inspect provider settings".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let durable = engine
            .session_snapshot_v2(completed.session_id, None, 100)
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
                    SessionId::from_uuid(Uuid::now_v7()),
                    "create it".into(),
                    binding(),
                    None,
                )
                .await
                .unwrap();
            let terminal = if cancel {
                let turn_revision = active_turn_revision(&waiting).unwrap();
                service
                    .cancel_durable(waiting.session_id, waiting.revision, turn_revision)
                    .unwrap()
            } else {
                let request_id = match waiting.pending.as_ref().unwrap() {
                    latte_core::SessionPendingRequest::Permission { request_id, .. } => {
                        request_id.clone()
                    }
                    latte_core::SessionPendingRequest::Input { .. } => {
                        panic!("expected permission")
                    }
                };
                service
                    .resolve_permission(
                        waiting.session_id,
                        waiting.revision,
                        test_turn_revision(&waiting),
                        request_id,
                        false,
                    )
                    .await
                    .unwrap()
            };
            assert_eq!(
                terminal.lifecycle,
                if cancel {
                    SessionLifecycle::Failed
                } else {
                    SessionLifecycle::Ready
                }
            );
            assert!(!root.path().join("created.txt").exists());
            assert!(terminal.active_turn_id.is_none());
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
                SessionId::from_uuid(Uuid::now_v7()),
                "show cwd".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Ready);
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
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .start(session_id, "wait".into(), binding(), None)
                .await
        });
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(matches!(
            engine.acquire_session_lease(session_id, now_ms(), 60),
            Err(StorageError::EngineUnavailable)
        ));
        let parallel = engine
            .acquire_session_lease(SessionId::from_uuid(Uuid::now_v7()), now_ms(), 60)
            .unwrap();
        engine.release_lease(&parallel).unwrap();
        assert_eq!(
            run.await.unwrap().unwrap().lifecycle,
            SessionLifecycle::Ready
        );
        let resumed = engine
            .acquire_session_lease(session_id, now_ms(), 60)
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
        let factory: SessionProviderFactory = Arc::new(|_| {
            Ok(ResolvedProvider {
                provider: Arc::new(DelayedProvider::scripted([(
                    Duration::from_millis(200),
                    response(Some("done"), vec![]),
                )])),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let first = service.clone();
        let second = service.clone();
        let first_session = SessionId::from_uuid(Uuid::now_v7());
        let second_session = SessionId::from_uuid(Uuid::now_v7());

        let (first, second) = tokio::join!(
            first.start(first_session, "first".into(), binding(), None),
            second.start(second_session, "second".into(), binding(), None),
        );

        assert_eq!(first.unwrap().lifecycle, SessionLifecycle::Ready);
        assert_eq!(second.unwrap().lifecycle, SessionLifecycle::Ready);
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
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .start(session_id, "wait for fencing".into(), binding(), None)
                .await
        });

        wait_until(
            || {
                engine
                    .session_snapshot_v2(session_id, None, 100)
                    .is_ok_and(|snapshot| {
                        snapshot.lifecycle == SessionLifecycle::Running
                            && snapshot.active_turn_id.is_some()
                    })
            },
            "provider turn to become active",
        )
        .await;
        let active = engine.session_snapshot_v2(session_id, None, 100).unwrap();
        let turn_id = active.active_turn_id.unwrap();
        force_lease_renewal_failure(&database);

        let error = run.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("lease heartbeat lost during provider call"),
            "unexpected provider lease-loss error: {error}"
        );
        let terminal = engine.session_snapshot_v2(session_id, None, 100).unwrap();
        assert_eq!(terminal.lifecycle, SessionLifecycle::Interrupted);
        assert!(terminal.active_turn_id.is_none());
        assert_eq!(
            engine.show(turn_id).unwrap().status,
            latte_core::TurnStatus::Interrupted
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
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let waiting = service
            .start(session_id, "sleep".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .resolve_permission(
                    session_id,
                    waiting.revision,
                    test_turn_revision(&waiting),
                    request_id,
                    true,
                )
                .await
        });
        let effect_id = loop {
            if let Ok(snapshot) = engine.session_snapshot_v2(session_id, None, 100)
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
            .acquire_session_lease(SessionId::from_uuid(Uuid::now_v7()), now_ms(), 60)
            .unwrap();
        engine.release_lease(&parallel).unwrap();
        service.cancel(session_id);
        let terminal = run.await.unwrap().unwrap();
        assert_eq!(terminal.lifecycle, SessionLifecycle::ReconciliationRequired);
        assert!(terminal.active_turn_id.is_none());
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
        .with_lease_ttl_ms(1000);
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let waiting = service
            .start(session_id, "sleep until fenced".into(), binding(), None)
            .await
            .unwrap();
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let runner = service.clone();
        let run = tokio::spawn(async move {
            runner
                .resolve_permission(
                    session_id,
                    waiting.revision,
                    test_turn_revision(&waiting),
                    request_id,
                    true,
                )
                .await
        });
        let effect_id = loop {
            let snapshot = engine.session_snapshot_v2(session_id, None, 100).unwrap();
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
        let before_loss = engine.session_snapshot_v2(session_id, None, 100).unwrap();
        let linked_turn_id = before_loss.active_turn_id.unwrap();
        force_lease_renewal_failure(&database);

        let error = run.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("lease heartbeat lost during started effect")
        );
        let recovered = engine.session_snapshot_v2(session_id, None, 100).unwrap();
        assert_eq!(
            recovered.lifecycle,
            SessionLifecycle::ReconciliationRequired
        );
        assert!(recovered.active_turn_id.is_none());
        assert_eq!(
            engine.effect_status(&effect_id).unwrap(),
            latte_engine::EffectStatus::Unknown
        );
        assert_eq!(
            engine.show(linked_turn_id).unwrap().status,
            latte_core::TurnStatus::Interrupted
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
            .reconcile_unknown_effect(session_id, &effect_id)
            .unwrap();
        assert_eq!(reconciled.lifecycle, SessionLifecycle::Failed);
        assert!(reconciled.active_turn_id.is_none());
        assert_eq!(
            engine.effect_status(&effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedFailed
        );
        assert_eq!(
            engine.show(linked_turn_id).unwrap().status,
            latte_core::TurnStatus::Failed
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
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        engine
            .create_session_v2(session_id, turn_id, binding(), "recover", 1)
            .unwrap();
        let lease = engine
            .acquire_session_lease(session_id, now_ms(), 10_000)
            .unwrap();
        let running = engine
            .commit_session_turn_update(
                SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: 0,
                    expected_turn_revision: 0,
                    command_id: SessionCommandId::from_uuid(Uuid::now_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitSessionTurnUpdate::Start {
                        source_key: format!("{turn_id}:start"),
                    },
                },
                &lease,
                now_ms(),
            )
            .unwrap()
            .snapshot;
        let descriptor = SessionEffectDescriptor {
            effect_id: format!("session-effect:{turn_id}:recover"),
            tool_call_id: "recover".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"note.txt"}),
            attempt: 1,
        };
        let prepared = engine
            .prepare_session_effect(
                session_effect_request(&running, descriptor.clone(), format!("{turn_id}:prepare"))
                    .unwrap(),
                &lease,
                now_ms(),
            )
            .unwrap();
        let started = engine
            .start_session_effect(
                session_effect_start_request(
                    &prepared.snapshot,
                    descriptor.effect_id.clone(),
                    format!("{turn_id}:start-effect"),
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
                    "SELECT EXISTS(SELECT 1 FROM runtime_checkpoints WHERE turn_id=?1)",
                    [turn_id.to_string()],
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
        let recovered = reopened.session_snapshot_v2(session_id, None, 100).unwrap();
        assert_eq!(
            recovered.lifecycle,
            SessionLifecycle::ReconciliationRequired
        );
        assert!(recovered.active_turn_id.is_none());
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            latte_engine::EffectStatus::Unknown
        );
        assert_eq!(
            reopened.show(turn_id).unwrap().status,
            latte_core::TurnStatus::Interrupted
        );
        assert!(
            rusqlite::Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_checkpoints WHERE turn_id=?1)",
                    [turn_id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert!(recovered.transcript.entries.windows(2).any(|pair| {
            pair[0].kind == TranscriptKind::System && pair[1].kind == TranscriptKind::Failure
        }));
        let service = scripted_service(root.path(), reopened.clone(), vec![]);
        let reconciled = service
            .reconcile_unknown_effect(session_id, &descriptor.effect_id)
            .unwrap();
        assert_eq!(reconciled.lifecycle, SessionLifecycle::Failed);
        assert!(reconciled.active_turn_id.is_none());
        assert_eq!(
            reopened.show(turn_id).unwrap().status,
            latte_core::TurnStatus::Failed
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
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        engine
            .create_session_v2(session_id, turn_id, binding(), "prepare", 1)
            .unwrap();
        let lease = engine
            .acquire_session_lease(session_id, now_ms(), 500)
            .unwrap();
        let running = engine
            .commit_session_turn_update(
                SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: 0,
                    expected_turn_revision: 0,
                    command_id: SessionCommandId::from_uuid(Uuid::now_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitSessionTurnUpdate::Start {
                        source_key: format!("{turn_id}:start"),
                    },
                },
                &lease,
                now_ms(),
            )
            .unwrap()
            .snapshot;
        let descriptor = SessionEffectDescriptor {
            effect_id: format!("session-effect:{turn_id}:prepared"),
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
            .prepare_session_effect(
                session_effect_request(&running, descriptor.clone(), format!("{turn_id}:prepare"))
                    .unwrap(),
                &lease,
                now_ms(),
            )
            .unwrap();
        assert_eq!(
            prepared.snapshot.lifecycle,
            SessionLifecycle::WaitingPermission
        );
        drop(engine);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let reopened = EngineBuilder::new()
            .workspace_root(root.path())
            .database_path(&db)
            .build()
            .unwrap();
        let recovered = reopened.session_snapshot_v2(session_id, None, 100).unwrap();
        assert_eq!(recovered.lifecycle, SessionLifecycle::Interrupted);
        assert!(recovered.active_turn_id.is_none());
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            latte_engine::EffectStatus::ObservedFailed
        );
        assert_eq!(
            reopened.show(turn_id).unwrap().status,
            latte_core::TurnStatus::Interrupted
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
        let service = SessionRuntimeService::new(
            engine.clone(),
            root.path(),
            SessionHistoryPolicy::default(),
            Arc::new(|_| Err("provider must not be constructed".into())),
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let running = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();

        assert!(matches!(
            service
                .follow_up(session_id, running.revision, "too early".into())
                .await,
            Err(SessionRuntimeError::InvalidState)
        ));
        assert!(matches!(
            service
                .provide_input(
                    session_id,
                    running.revision,
                    test_turn_revision(&running),
                    "missing".into(),
                    "value".into()
                )
                .await,
            Err(SessionRuntimeError::InvalidState)
        ));
        assert!(matches!(
            service
                .resolve_permission(
                    session_id,
                    running.revision,
                    test_turn_revision(&running),
                    "missing".into(),
                    true
                )
                .await,
            Err(SessionRuntimeError::InvalidState)
        ));
        assert!(matches!(
            service.reconcile_unknown_effect(session_id, "missing"),
            Err(SessionRuntimeError::InvalidState)
        ));

        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<dyn SessionProgressSink> = {
            let observed = Arc::clone(&observed);
            Arc::new(move |_session_id, progress| observed.lock().unwrap().push(progress))
        };
        let service = service.with_progress_sink(sink);
        let token = CancellationToken::new();
        service
            .active
            .lock()
            .unwrap()
            .insert(session_id, token.clone());
        service.cancel(session_id);
        assert!(token.is_cancelled());
        let live = service.load_full(session_id).unwrap();
        let live_turn_revision = active_turn_revision(&live).unwrap();
        assert_eq!(
            service
                .cancel_durable(session_id, live.revision, live_turn_revision)
                .unwrap()
                .session_id,
            session_id,
            "registered in-flight work is cancelled transiently without forging a durable terminal"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    #[rustfmt::skip]
    fn verification_and_transcript_helpers_fail_closed_on_missing_authority() {
        use latte_core::{SessionTurnStatus, SessionTurnSummary, TranscriptEntry, TranscriptEntryId};

        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let mut snapshot = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        let base = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            Arc::new(|_| Err("not used".into())),
        );
        assert!(matches!(
            base.verification_descriptor(&snapshot, "done"),
            Err(SessionRuntimeError::Effect(message)) if message.contains("no configured verification")
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
            Err(SessionRuntimeError::Effect(message)) if message.contains("argv is empty")
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

        snapshot.active_turn_id = None;
        assert!(matches!(
            active_turn_revision(&snapshot),
            Err(SessionRuntimeError::InvalidState)
        ));
        assert!(
            session_effect_request(&snapshot, descriptor.clone(), "missing-run".into()).is_err()
        );
        assert!(
            session_effect_start_request(
                &snapshot,
                descriptor.effect_id.clone(),
                "missing-run".into()
            )
            .is_err()
        );
        let live = verified.acquire(session_id).unwrap();
        assert!(verified.recover_lease_loss(&snapshot, &live, "test").to_string().contains("active linked turn is unavailable"));

        snapshot.active_turn_id = Some(turn_id);
        snapshot.turns = vec![SessionTurnSummary {
            turn_id: new_turn_id(),
            parent_turn_id: None,
            ordinal: 0,
            status: SessionTurnStatus::Running,
            turn_revision: 1,
            completed_at_ms: None,
            failure_code: None,
        }];
        assert!(matches!(
            active_turn_revision(&snapshot),
            Err(SessionRuntimeError::InvalidState)
        ));
        assert!(verified.recover_lease_loss(&snapshot, &live, "test").to_string().contains("linked turn revision is unavailable"));
        snapshot.turns[0].turn_id = turn_id;
        let presentation = SessionEffectPresentation { effect_id: descriptor.effect_id.clone(), tool_call_id: descriptor.tool_call_id.clone(), name: descriptor.name.clone(), input: descriptor.input.clone(), attempt: descriptor.attempt };
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
                turn_id: Some(turn_id),
                kind: TranscriptKind::Assistant,
                text: "call".into(),
                payload: Some(serde_json::json!({"tool_calls":[call.clone()]})),
                source_key: "assistant".into(),
                created_at_ms: 2,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 3,
                turn_id: Some(turn_id),
                kind: TranscriptKind::System,
                text: "ignore".into(),
                payload: None,
                source_key: "system".into(),
                created_at_ms: 3,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 4,
                turn_id: Some(turn_id),
                kind: TranscriptKind::ToolResult,
                text: "missing payload".into(),
                payload: None,
                source_key: "result-empty".into(),
                created_at_ms: 4,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 5,
                turn_id: Some(turn_id),
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

        let mut running = verified.commit(session_id, turn_id, 0, 0, CommitSessionTurnUpdate::Start { source_key: "helper:start".into() }, &live).unwrap();
        assert!(verified.recover_lease_loss(&running, &live, "test").to_string().contains("recovery failed"));
        let takeover = verified
            .engine
            .acquire_session_lease(session_id, live.expires_at_ms(), 60_000)
            .unwrap();
        let mut mismatched = running.clone();
        mismatched.turns[0].turn_revision += 1;
        assert!(
            verified
                .recover_lease_loss(&mismatched, &live, "test")
                .to_string()
                .contains("newer owner fenced")
        );
        let live = SessionLeaseGuard {
            engine: verified.engine.clone(),
            lease: takeover,
        };
        for index in 0..501 { running = verified.commit(session_id, turn_id, running.revision, 1, CommitSessionTurnUpdate::AppendTranscript { source_key: format!("helper:page:{index}"), kind: TranscriptKind::System, text: index.to_string(), payload: None }, &live).unwrap(); }
        assert_eq!(verified.load_full(session_id).unwrap().transcript.entries.len(), 502);
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
        assert_eq!(verified.finish_verification(&observed, &presentation, &live).unwrap().lifecycle, SessionLifecycle::Failed);
        verified.engine.release_lease(&live).unwrap(); assert!(verified.recover_lease_loss(&observed, &live, "test").to_string().contains("already terminal"));
    }

    #[tokio::test]
    async fn start_accepted_signals_durable_acceptance_then_completes() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![response(Some("done"), Vec::new())],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let (tx, rx) = oneshot::channel();
        // Drive the future on a task so it makes progress while we await the
        // acceptance signal (the future is otherwise lazy).
        let handle = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .start_accepted(
                        session_id,
                        SessionCommandId::from_uuid(Uuid::now_v7()),
                        "hello".into(),
                        binding(),
                        None,
                        tx,
                    )
                    .await
            })
        };

        let accepted = rx.await.unwrap().expect("accepted snapshot");
        let accepted_snapshot = match accepted {
            latte_core::CreateOutcome::Created(s) | latte_core::CreateOutcome::Replayed(s) => s,
        };
        assert_eq!(accepted_snapshot.session_id, session_id);
        let finished = handle.await.unwrap().unwrap();
        let finished_snapshot = match finished {
            latte_core::CreateOutcome::Created(s) | latte_core::CreateOutcome::Replayed(s) => s,
        };
        assert_eq!(finished_snapshot.session_id, session_id);
    }

    #[tokio::test]
    async fn start_accepted_signals_rejection_on_invalid_binding() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![response(Some("unused"), Vec::new())],
        );
        let mut invalid = binding();
        invalid.provider_name.clear();
        let (tx, rx) = oneshot::channel();
        let result = service
            .start_accepted(
                SessionId::from_uuid(Uuid::now_v7()),
                SessionCommandId::from_uuid(Uuid::now_v7()),
                "x".into(),
                invalid,
                None,
                tx,
            )
            .await;
        // The accept signal carries the rejection, and the call itself errors.
        assert!(rx.await.unwrap().is_err());
        assert!(matches!(
            result,
            Err(SessionRuntimeError::ProviderConfiguration(_))
        ));
    }

    #[tokio::test]
    async fn follow_up_accepted_signals_acceptance_on_ready_session() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(Some("first"), Vec::new()),
                response(Some("second"), Vec::new()),
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(ready.lifecycle, SessionLifecycle::Ready);

        let (tx, rx) = oneshot::channel();
        let handle = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .follow_up_accepted(
                        session_id,
                        SessionCommandId::from_uuid(Uuid::now_v7()),
                        ready.revision,
                        "second".into(),
                        tx,
                    )
                    .await
            })
        };
        let accepted = rx.await.unwrap().expect("follow-up accepted");
        let accepted = match accepted {
            latte_core::CreateOutcome::Created(snapshot)
            | latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
        };
        assert_eq!(accepted.session_id, session_id);
        assert!(handle.await.unwrap().is_ok());

        // A stale follow-up signals rejection and errors (call errors directly).
        let (tx, rx) = oneshot::channel();
        let stale = service
            .follow_up_accepted(
                session_id,
                SessionCommandId::from_uuid(Uuid::now_v7()),
                999,
                "stale".into(),
                tx,
            )
            .await;
        assert!(rx.await.unwrap().is_err());
        assert!(matches!(stale, Err(SessionRuntimeError::InvalidState)));
    }

    #[tokio::test]
    async fn follow_up_accepted_replays_durable_acceptance_and_rejects_mismatch() {
        // A same-command_id same-payload retry after the turn completes replays
        // the original acceptance (Replayed, no duplicate turn); a same-id
        // different-payload retry is a durable mismatch.
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(Some("first"), Vec::new()),
                response(Some("second"), Vec::new()),
                response(Some("third"), Vec::new()),
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(ready.lifecycle, SessionLifecycle::Ready);

        let command_id = SessionCommandId::from_uuid(Uuid::now_v7());

        // First follow-up → Created.
        let (tx, rx) = oneshot::channel();
        let handle = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .follow_up_accepted(session_id, command_id, ready.revision, "second".into(), tx)
                    .await
            })
        };
        let accepted = rx.await.unwrap().expect("follow-up accepted");
        let first_revision = match accepted {
            latte_core::CreateOutcome::Created(snapshot) => snapshot.revision,
            latte_core::CreateOutcome::Replayed(_) => panic!("first follow-up must be Created"),
        };
        handle.await.unwrap().unwrap();

        // Wait for the follow-up turn to complete.
        let ready2 = loop {
            let snap = service.load_full(session_id).unwrap();
            if snap.lifecycle == SessionLifecycle::Ready {
                break snap;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };

        // Replay with the same command_id + payload → Replayed (no new turn).
        let replay = service
            .follow_up(session_id, ready2.revision, "second".into())
            .await;
        // follow_up mints a fresh command_id, so this is a new follow-up, not
        // a replay. To test the durable replay, use follow_up_accepted with
        // the same command_id.
        let (tx, rx) = oneshot::channel();
        let replayed = service
            .follow_up_accepted(session_id, command_id, ready.revision, "second".into(), tx)
            .await
            .unwrap();
        match replayed {
            latte_core::CreateOutcome::Replayed(snapshot) => {
                assert_eq!(snapshot.revision, first_revision);
            }
            latte_core::CreateOutcome::Created(_) => {
                panic!("same command_id replay must be Replayed")
            }
        }
        let _ = replay;
        let _ = rx.await;

        // Same command_id, different prompt → mismatch error.
        let (tx, rx) = oneshot::channel();
        let mismatch = service
            .follow_up_accepted(
                session_id,
                command_id,
                ready.revision,
                "DIFFERENT".into(),
                tx,
            )
            .await;
        assert!(matches!(
            mismatch,
            Err(SessionRuntimeError::Storage(
                latte_engine::StorageError::SessionCommandReplayMismatch
            ))
        ));
        let _ = rx.await;
    }

    // -- Pure helper coverage ------------------------------------------------

    #[test]
    fn append_denied_tool_results_adds_missing_tool_messages() {
        use crate::provider::{Message, ToolCall};
        let mut segment = vec![
            Message::Assistant {
                content: Some("calling tools".into()),
                tool_calls: vec![
                    ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "a.txt"}),
                    },
                    ToolCall {
                        id: "call-2".into(),
                        name: "write_file".into(),
                        input: serde_json::json!({"path": "b.txt", "content": "x"}),
                    },
                ],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                name: Some("read_file".into()),
                content: "file content".into(),
            },
        ];
        append_denied_tool_results(&mut segment);
        // call-1 already has a Tool message; call-2 should get a denial message.
        assert_eq!(segment.len(), 3);
        match &segment[2] {
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "call-2");
                assert!(content.contains("permission denied"));
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[test]
    fn policy_treats_missing_round_budget_as_unlimited_and_rejects_zero() {
        assert!(
            SessionHistoryPolicy {
                max_tool_rounds: Some(0),
                ..SessionHistoryPolicy::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SessionHistoryPolicy {
                provider_timeout_ms: 0,
                ..SessionHistoryPolicy::default()
            }
            .validate()
            .is_err()
        );
        // The default is unlimited and valid; an explicit positive bound is
        // valid too.
        assert!(SessionHistoryPolicy::default().max_tool_rounds.is_none());
        assert!(SessionHistoryPolicy::default().validate().is_ok());
        assert!(
            SessionHistoryPolicy {
                max_tool_rounds: Some(1),
                ..SessionHistoryPolicy::default()
            }
            .validate()
            .is_ok()
        );
    }

    fn read_note_call(id: &str) -> crate::provider::ToolCall {
        crate::provider::ToolCall {
            id: id.into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "note.txt"}),
        }
    }

    /// The bound counts *opened tool batches*. Once the bound-reaching batch
    /// has executed, the model must still be allowed to read its results and
    /// return the final answer; the bound only blocks opening another batch.
    /// A pre-request check (the old shape) refused this very completion with
    /// `max_tool_rounds=1`.
    #[tokio::test]
    async fn round_bound_of_one_still_allows_the_final_completion() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "hello").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service_with_policy(
            root.path(),
            engine,
            vec![
                response(Some("reading"), vec![read_note_call("only-round")]),
                response(Some("final answer"), vec![]),
            ],
            SessionHistoryPolicy {
                max_tool_rounds: Some(1),
                ..SessionHistoryPolicy::default()
            },
        );
        let snapshot = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "read and answer".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Ready);
        assert_eq!(
            snapshot.turns.last().unwrap().status,
            latte_core::SessionTurnStatus::Completed,
            "the final answer must complete even with a one-round bound: {snapshot:?}"
        );
    }

    /// At a one-round bound, an attempt to open a second tool batch ends the
    /// turn retryably without consuming further scripted responses.
    #[tokio::test]
    async fn round_bound_stops_only_another_tool_batch() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "hello").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service_with_policy(
            root.path(),
            engine,
            vec![
                response(Some("one"), vec![read_note_call("round-1")]),
                response(Some("two"), vec![read_note_call("round-2-blocked")]),
                response(Some("must not be reached"), vec![]),
            ],
            SessionHistoryPolicy {
                max_tool_rounds: Some(1),
                ..SessionHistoryPolicy::default()
            },
        );
        let snapshot = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "loop".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Ready);
        assert_eq!(
            snapshot.turns.last().unwrap().status,
            latte_core::SessionTurnStatus::Failed
        );
        let failure = snapshot
            .transcript
            .entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::Failure)
            .map(|entry| entry.text.as_str())
            .next_back()
            .unwrap_or("");
        assert!(
            failure.contains("tool rounds") && failure.contains("max_tool_rounds"),
            "unexpected failure text: {failure}"
        );
    }

    /// Regression guard for the removed default cap: with no explicit bound
    /// the policy runs more than the old 48-round default to completion.
    #[tokio::test]
    async fn unlimited_default_runs_past_the_legacy_default_cap() {
        // Also a stack-safety regression: the turn loop must be iterative so
        // dozens of tool batches do not grow a recursive future stack.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("note.txt"), "hello").unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let mut responses: Vec<ProviderResponse> = (0..50)
            .map(|i| response(Some("step"), vec![read_note_call(&format!("call-{i}"))]))
            .collect();
        responses.push(response(Some("finished"), vec![]));
        let service = scripted_service(root.path(), engine, responses);
        let snapshot = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "very long task".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Ready);
        assert_eq!(
            snapshot.turns.last().unwrap().status,
            latte_core::SessionTurnStatus::Completed,
            "the default policy must not stop a 50-round converging turn: {snapshot:?}"
        );
    }

    #[test]
    fn declared_tool_call_admits_only_ids_the_assistant_emitted() {
        use crate::provider::{Message, ToolCall};
        let segment = vec![
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
            },
            Message::Tool {
                tool_call_id: "call-1".into(),
                name: Some("read_file".into()),
                content: "contents".into(),
            },
        ];
        assert!(declared_tool_call(&segment, "call-1"));
        // The verification run is a durable tool result whose id the engine
        // minted. Replaying it would put a `tool` message in front of no
        // matching call, which the Provider rejects for the whole Session.
        assert!(!declared_tool_call(
            &segment,
            "verification-01a0800c-42f4-7b82-ae01-bf81b981852c"
        ));
        assert!(!declared_tool_call(&[], "call-1"));
    }

    #[test]
    fn append_denied_tool_results_handles_empty_segment() {
        let mut segment: Vec<Message> = vec![];
        append_denied_tool_results(&mut segment);
        assert!(segment.is_empty());
    }

    #[test]
    fn provider_configuration_failure_message_is_stable() {
        let msg = provider_configuration_failure_message();
        assert!(msg.contains("provider configuration"));
        assert!(msg.contains("credentials"));
    }

    #[test]
    fn wire_bytes_counts_serialized_size() {
        use crate::provider::Message;
        let messages = vec![Message::User {
            content: "hello".into(),
        }];
        let size = wire_bytes(&messages).unwrap();
        assert!(size > 0);
    }

    #[test]
    fn new_turn_id_returns_unique_ids() {
        let id1 = new_turn_id();
        let id2 = new_turn_id();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn signal_accept_sends_to_receiver() {
        let (tx, rx) = oneshot::channel();
        signal_accept(Some(tx), Ok::<_, String>(42));
        assert_eq!(rx.await.unwrap().unwrap(), 42);
        // None sender is a no-op.
        signal_accept(None, Ok::<_, String>(42));
    }

    #[test]
    fn classify_create_error_maps_conflict_and_failed() {
        let mismatch =
            SessionRuntimeError::Storage(latte_engine::StorageError::SessionCommandReplayMismatch);
        assert!(matches!(
            classify_create_error(&mismatch),
            latte_core::CreateAcceptError::IdempotencyMismatch(_)
        ));
        let conflict =
            SessionRuntimeError::Storage(latte_engine::StorageError::SessionAlreadyExists(
                latte_core::SessionId::from_uuid(uuid::Uuid::now_v7()),
            ));
        assert!(matches!(
            classify_create_error(&conflict),
            latte_core::CreateAcceptError::Conflict(_)
        ));
        // InvalidState (stale revision / not accepting follow-up) is a
        // client-side conflict, not a server failure.
        let invalid_state = SessionRuntimeError::InvalidState;
        assert!(matches!(
            classify_create_error(&invalid_state),
            latte_core::CreateAcceptError::Conflict(_)
        ));
        let failed = SessionRuntimeError::MailboxFull;
        assert!(matches!(
            classify_create_error(&failed),
            latte_core::CreateAcceptError::Failed(_)
        ));
    }

    #[test]
    fn active_turn_revision_finds_active_run() {
        use latte_core::{
            IdSource, SessionTurnStatus, SessionTurnSummary, SystemIdSource, TranscriptPage,
        };
        let turn_id = TurnId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let snapshot = SessionSnapshot {
            session_id: SessionId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            revision: 1,
            sequence: 1,
            lifecycle: SessionLifecycle::Running,
            binding: SessionProviderBinding {
                version: 2,
                provider_name: String::new(),
                provider_type: String::new(),
                protocol: String::new(),
                model: String::new(),
                config_fingerprint: String::new(),
                tools_fingerprint: String::new(),
                aliases: std::collections::BTreeMap::default(),
                credential_ref_id: String::new(),
                data_scope_id: String::new(),
                credential_generation: 0,
            },
            latest_turn_id: None,
            active_turn_id: Some(turn_id),
            pending: None,
            turns: vec![SessionTurnSummary {
                turn_id,
                parent_turn_id: None,
                ordinal: 0,
                status: SessionTurnStatus::Running,
                turn_revision: 7,
                completed_at_ms: None,
                failure_code: None,
            }],
            transcript: TranscriptPage {
                entries: vec![],
                next_after: None,
                has_more: false,
            },
            focus: None,
        };
        assert_eq!(active_turn_revision(&snapshot).unwrap(), 7);
        // No active run → error.
        let mut no_active = snapshot.clone();
        no_active.active_turn_id = None;
        assert!(active_turn_revision(&no_active).is_err());
    }

    // -- start / start_accepted error paths --------------------------------

    #[tokio::test]
    async fn start_accepted_rejects_escaping_focus_before_durable_create() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(root.path(), engine, vec![response(Some("unused"), vec![])]);
        let (tx, rx) = oneshot::channel();
        let err = service
            .start_accepted(
                SessionId::from_uuid(Uuid::now_v7()),
                SessionCommandId::from_uuid(Uuid::now_v7()),
                "prompt".into(),
                binding(),
                Some(Path::new("../outside")),
                tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::History(_)), "{err:?}");
        let accepted = rx.await.unwrap().unwrap_err();
        assert!(
            matches!(accepted, latte_core::CreateAcceptError::Failed(_)),
            "{accepted:?}"
        );
    }

    #[tokio::test]
    async fn start_accepted_on_existing_session_with_fresh_command_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(Some("first"), vec![]),
                response(Some("unused"), vec![]),
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(ready.lifecycle, SessionLifecycle::Ready);
        let (tx, rx) = oneshot::channel();
        let err = service
            .start_accepted(
                session_id,
                SessionCommandId::from_uuid(Uuid::now_v7()),
                "second".into(),
                binding(),
                None,
                tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
        let accepted = rx.await.unwrap().unwrap_err();
        assert!(
            matches!(accepted, latte_core::CreateAcceptError::Conflict(_)),
            "{accepted:?}"
        );
    }

    #[tokio::test]
    async fn start_accepted_replays_same_command_id_without_restarting_provider() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(Some("first"), vec![]),
                response(Some("must not be called"), vec![]),
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let command_id = SessionCommandId::from_uuid(Uuid::now_v7());
        // First create with this command id.
        let (tx, rx) = oneshot::channel();
        let handle = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .start_accepted(session_id, command_id, "hello".into(), binding(), None, tx)
                    .await
            })
        };
        let accepted = rx.await.unwrap().unwrap();
        assert!(matches!(accepted, latte_core::CreateOutcome::Created(_)));
        let first = handle.await.unwrap().unwrap();
        assert!(matches!(first, latte_core::CreateOutcome::Created(_)));

        // Second create with the SAME command id must replay durably without
        // acquiring a lease or calling the provider.
        let (tx, rx) = oneshot::channel();
        let replayed = service
            .start_accepted(session_id, command_id, "hello".into(), binding(), None, tx)
            .await
            .unwrap();
        let replayed_snapshot = match replayed {
            latte_core::CreateOutcome::Replayed(s) => s,
            latte_core::CreateOutcome::Created(_) => panic!("expected replay, got Created"),
        };
        assert_eq!(replayed_snapshot.session_id, session_id);
        let accepted = rx.await.unwrap().unwrap();
        assert!(matches!(accepted, latte_core::CreateOutcome::Replayed(_)));
    }

    #[tokio::test]
    async fn start_accepted_fails_when_lease_is_held() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![
                response(Some("first"), vec![]),
                response(Some("unused"), vec![]),
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(ready.lifecycle, SessionLifecycle::Ready);
        // Hold the lease so the next create's acquire fails.
        let _held = engine
            .acquire_session_lease(session_id, now_ms(), 60_000)
            .unwrap();
        let (tx, rx) = oneshot::channel();
        let err = service
            .start_accepted(
                session_id,
                SessionCommandId::from_uuid(Uuid::now_v7()),
                "second".into(),
                binding(),
                None,
                tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
        let accepted = rx.await.unwrap().unwrap_err();
        assert!(
            matches!(accepted, latte_core::CreateAcceptError::Failed(_)),
            "{accepted:?}"
        );
    }

    #[tokio::test]
    async fn start_rejects_while_runner_is_active() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = delayed_service(
            root.path(),
            engine,
            [(Duration::from_millis(300), simple_response("done"))],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let first = service.clone();
        let running = tokio::spawn(async move {
            first
                .start(session_id, "slow turn".into(), binding(), None)
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = service
            .start(session_id, "concurrent".into(), binding(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::InvalidState));
        let completed = running.await.unwrap().unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
    }

    #[tokio::test]
    async fn start_with_missing_root_fails_context_build() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(&root.path().join("missing"), engine, vec![]);
        let err = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "hi".into(),
                binding(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::History(_)), "{err:?}");
    }

    #[tokio::test]
    async fn invalid_history_policy_rejects_start() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let policy = SessionHistoryPolicy {
            max_request_bytes: 1024,
            max_input_bytes: 100,
            reserved_output_bytes: 100,
            context_cap_bytes: 64,
            ..SessionHistoryPolicy::default()
        };
        let provider = Arc::new(FakeProvider::scripted([response(Some("x"), vec![])]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), policy, factory);
        let err = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "hi".into(),
                binding(),
                None,
            )
            .await
            .unwrap_err();
        // An invalid policy surfaces at profile resolution, before any
        // history is built: the typed error is ProviderConfiguration, not a
        // late History failure. Production never reaches this path (the
        // config loader validates the session section at startup); this is
        // the service's defensive contract.
        assert!(
            matches!(err, SessionRuntimeError::ProviderConfiguration(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn prompt_exceeding_budget_fails_start() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let policy = SessionHistoryPolicy {
            max_request_bytes: 64,
            max_input_bytes: 64,
            reserved_output_bytes: 0,
            context_cap_bytes: 64,
            ..SessionHistoryPolicy::default()
        };
        let provider = Arc::new(FakeProvider::scripted([response(Some("x"), vec![])]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), policy, factory);
        let err = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "x".repeat(10_000),
                binding(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::History(_)), "{err:?}");
    }

    // -- follow_up error paths ----------------------------------------------

    #[tokio::test]
    async fn follow_up_rejects_oversized_prompt() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![response(Some("first"), vec![])],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        let tiny_policy = SessionHistoryPolicy {
            max_request_bytes: 64,
            max_input_bytes: 64,
            reserved_output_bytes: 0,
            context_cap_bytes: 64,
            ..SessionHistoryPolicy::default()
        };
        let provider = Arc::new(FakeProvider::scripted([response(Some("x"), vec![])]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let tiny = SessionRuntimeService::new(engine, root.path(), tiny_policy, factory);
        let err = tiny
            .follow_up(session_id, ready.revision, "x".repeat(10_000))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::History(_)), "{err:?}");
    }

    /// The issue-#21 window: the `Ready` commit is durable while the dying
    /// runner's mailbox entry is still present, so a follow-up that raced the
    /// teardown hits the strict `contains_key` rejection. When the residue is
    /// removed shortly after — exactly what the dying `drain_mailbox` does —
    /// the follow-up must succeed instead of surfacing the transient 409.
    #[tokio::test]
    async fn follow_up_waits_out_the_runner_teardown_residue() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(Some("first done"), vec![]),
                response(Some("second done"), vec![]),
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(ready.lifecycle, SessionLifecycle::Ready);

        // Re-install the residue the dying runner has not removed yet, then
        // simulate its teardown: the entry disappears mid-wait.
        let residue = Arc::clone(&service.mailboxes);
        residue.lock().unwrap().insert(session_id, VecDeque::new());
        let sweeper_session_id = session_id;
        let sweeper = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            residue.lock().unwrap().remove(&sweeper_session_id);
        });

        let second = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        assert_eq!(second.lifecycle, SessionLifecycle::Ready);
        sweeper.await.unwrap();
    }

    /// Residue is never taken over: a mailbox entry that carries queued work
    /// (or any residue nobody drains) keeps excluding follow-ups. The wait is
    /// bounded, and the elapsed check pins that the bounded wait actually
    /// ran — a mutation back to the plain `begin_runner` fails it instantly.
    #[tokio::test]
    async fn follow_up_residue_is_waited_out_never_taken_over() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![response(Some("first done"), vec![])],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();

        // Persistent residue with queued work inside: nobody will drain it.
        service.mailboxes.lock().unwrap().insert(
            session_id,
            VecDeque::from(["queued while dying".to_owned()]),
        );

        let started = std::time::Instant::now();
        let error = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionRuntimeError::InvalidState),
            "{error:?}"
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(RESIDUE_SETTLE_BUDGET_MS),
            "the bounded wait must have run before rejecting"
        );
        let residue = service.mailboxes.lock().unwrap().get(&session_id).cloned();
        assert_eq!(
            residue,
            Some(VecDeque::from(["queued while dying".to_owned()])),
            "waiting must never steal or wipe the residue"
        );
    }

    /// The immediate-reject guard: a non-idle lifecycle (here
    /// `WaitingInput`) is not teardown residue, so the follow-up must fail
    /// right away instead of waiting out the settle budget. Both paths
    /// surface the same `InvalidState`, so only the elapsed check separates
    /// them — deleting the guard makes this test fail on the elapsed assert,
    /// mirroring the review probe that found the gap.
    #[tokio::test]
    async fn follow_up_rejects_immediately_when_the_session_is_not_idle() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine,
            vec![
                response(Some("first done"), vec![]),
                ProviderResponse {
                    message: None,
                    tool_calls: vec![],
                    input_request: Some(InputRequest {
                        id: "shape".into(),
                        prompt: "Which shape?".into(),
                        secret: false,
                    }),
                    usage: crate::provider::ProviderUsage::default(),
                    finish_reason: None,
                    provider_state: None,
                },
            ],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        let waiting = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingInput);

        // Residue from the parked turn's runner teardown window.
        service
            .mailboxes
            .lock()
            .unwrap()
            .insert(session_id, VecDeque::new());

        let started = std::time::Instant::now();
        let error = service
            .follow_up(session_id, waiting.revision, "third".into())
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionRuntimeError::InvalidState),
            "{error:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(RESIDUE_SETTLE_BUDGET_MS),
            "a non-idle lifecycle must reject immediately, not wait out the budget"
        );
    }

    #[tokio::test]
    async fn follow_up_accepted_fails_when_lease_is_held() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![response(Some("first"), vec![])],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        let _held = engine
            .acquire_session_lease(session_id, now_ms(), 60_000)
            .unwrap();
        let (tx, rx) = oneshot::channel();
        let err = service
            .follow_up_accepted(
                session_id,
                SessionCommandId::from_uuid(Uuid::now_v7()),
                ready.revision,
                "second".into(),
                tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
        let accepted = rx.await.unwrap().unwrap_err();
        let message = match accepted {
            latte_core::CreateAcceptError::Conflict(message)
            | latte_core::CreateAcceptError::IdempotencyMismatch(message)
            | latte_core::CreateAcceptError::Failed(message) => message,
        };
        assert!(message.contains("session storage"));
    }

    #[tokio::test]
    async fn queue_follow_up_validates_prompt_budget() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let policy = SessionHistoryPolicy {
            max_request_bytes: 64,
            max_input_bytes: 64,
            reserved_output_bytes: 0,
            context_cap_bytes: 64,
            ..SessionHistoryPolicy::default()
        };
        let provider = Arc::new(FakeProvider::scripted([response(Some("x"), vec![])]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), policy, factory);
        let err = service
            .queue_follow_up(SessionId::from_uuid(Uuid::now_v7()), "x".repeat(10_000))
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::History(_)), "{err:?}");
    }

    #[tokio::test]
    async fn follow_up_with_invalid_policy_fails_history_build() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![response(Some("first"), vec![])],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        let invalid_policy = SessionHistoryPolicy {
            max_request_bytes: 1024,
            max_input_bytes: 100,
            reserved_output_bytes: 100,
            context_cap_bytes: 64,
            ..SessionHistoryPolicy::default()
        };
        let provider = Arc::new(FakeProvider::scripted([response(Some("x"), vec![])]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let broken = SessionRuntimeService::new(engine, root.path(), invalid_policy, factory);
        let err = broken
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap_err();
        // Same defensive contract as start: the invalid policy fails at
        // profile resolution with ProviderConfiguration.
        assert!(
            matches!(err, SessionRuntimeError::ProviderConfiguration(_)),
            "{err:?}"
        );
    }

    // -- switch_model / resolve_permission / cancel_durable -----------------

    #[tokio::test]
    async fn switch_model_unknown_session_fails() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(root.path(), engine, vec![]);
        let err = service
            .switch_model(SessionId::from_uuid(Uuid::now_v7()), 0, &binding())
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
    }

    #[tokio::test]
    async fn switch_model_fails_when_lease_held() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![response(Some("first"), vec![])],
        );
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "first".into(), binding(), None)
            .await
            .unwrap();
        let mut next = binding();
        next.provider_name = "other".into();
        next.model = "reasoning".into();
        next.config_fingerprint = "other-config".into();
        let _held = engine
            .acquire_session_lease(session_id, now_ms(), 60_000)
            .unwrap();
        let err = service
            .switch_model(session_id, ready.revision, &next)
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
    }

    #[tokio::test]
    async fn resolve_permission_without_active_run_fails() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(root.path(), engine, vec![response(Some("first"), vec![])]);
        let ready = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "first".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let err = service
            .resolve_permission(ready.session_id, ready.revision, 0, "any".into(), true)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::InvalidState));
    }

    #[tokio::test]
    async fn cancel_durable_rejects_stale_revision() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(root.path(), engine.clone(), vec![]);
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let running = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        let err = service
            .cancel_durable(
                session_id,
                running.revision + 1,
                test_turn_revision(&running),
            )
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::InvalidState));
    }

    #[tokio::test]
    async fn cancel_durable_fails_when_lease_held() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(root.path(), engine.clone(), vec![]);
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let running = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        let _held = engine
            .acquire_session_lease(session_id, now_ms(), 60_000)
            .unwrap();
        let err = service
            .cancel_durable(session_id, running.revision, test_turn_revision(&running))
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
    }

    // -- input request / permission flows -----------------------------------

    fn waiting_input_service(
        root: &std::path::Path,
        engine: EngineHandle,
        policy: SessionHistoryPolicy,
    ) -> (SessionRuntimeService, SessionId) {
        let provider = Arc::new(FakeProvider::scripted([ProviderResponse {
            message: None,
            tool_calls: vec![],
            input_request: Some(InputRequest {
                id: "lang".into(),
                prompt: "Which language?".into(),
                secret: false,
            }),
            usage: crate::provider::ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        }]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root, policy, factory);
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        (service, session_id)
    }

    #[tokio::test]
    async fn provide_input_rejects_oversized_value() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        // The budget must clear the system prompt and still reject the
        // oversized input value below; it is not a minimum-size probe.
        let policy = SessionHistoryPolicy {
            max_request_bytes: 4096,
            max_input_bytes: 4096,
            reserved_output_bytes: 0,
            context_cap_bytes: 64,
            ..SessionHistoryPolicy::default()
        };
        let (service, session_id) = waiting_input_service(root.path(), engine, policy);
        let waiting = service
            .start(session_id, "hi".into(), binding(), None)
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingInput);
        let err = service
            .provide_input(
                session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                "lang".into(),
                "x".repeat(10_000),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::History(_)), "{err:?}");
    }

    #[tokio::test]
    async fn provide_input_fails_when_provider_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let provider = Arc::new(FakeProvider::scripted([ProviderResponse {
            message: None,
            tool_calls: vec![],
            input_request: Some(InputRequest {
                id: "lang".into(),
                prompt: "Which?".into(),
                secret: false,
            }),
            usage: crate::provider::ProviderUsage::default(),
            finish_reason: None,
            provider_state: None,
        }]));
        let factory: SessionProviderFactory = Arc::new(move |_| {
            if factory_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Err("secret reference unavailable".into())
            } else {
                Ok(ResolvedProvider {
                    provider: provider.clone(),
                    binding: crate::registry::ProviderBinding::direct(&[]),
                })
            }
        });
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let waiting = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "hi".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingInput);
        let err = service
            .provide_input(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                "lang".into(),
                "Rust".into(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SessionRuntimeError::ProviderConfiguration(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_permission_allow_fails_when_provider_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let provider = Arc::new(FakeProvider::scripted([response(
            Some("creating"),
            vec![crate::provider::ToolCall {
                id: "create-note".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path":"created.txt",
                    "content":"x",
                    "create_intent":true
                }),
            }],
        )]));
        let factory: SessionProviderFactory = Arc::new(move |_| {
            if factory_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                Err("secret reference unavailable".into())
            } else {
                Ok(ResolvedProvider {
                    provider: provider.clone(),
                    binding: crate::registry::ProviderBinding::direct(&[]),
                })
            }
        });
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        );
        let waiting = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "create it".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let err = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, SessionRuntimeError::ProviderConfiguration(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_permission_fails_when_lease_held() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(
            root.path(),
            engine.clone(),
            vec![response(
                Some("creating"),
                vec![crate::provider::ToolCall {
                    id: "create-note".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path":"created.txt",
                        "content":"x",
                        "create_intent":true
                    }),
                }],
            )],
        );
        let waiting = service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "create it".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);
        let request_id = match waiting.pending.as_ref().unwrap() {
            latte_core::SessionPendingRequest::Permission { request_id, .. } => request_id.clone(),
            latte_core::SessionPendingRequest::Input { .. } => panic!("expected permission"),
        };
        let _held = engine
            .acquire_session_lease(waiting.session_id, now_ms(), 60_000)
            .unwrap();
        let err = service
            .resolve_permission(
                waiting.session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                request_id,
                true,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::Storage(_)), "{err:?}");
    }

    // -- focus / progress / helper coverage ---------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_focus_passes_none_to_durable_layer() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let service = scripted_service(root.path(), engine, vec![response(Some("done"), vec![])]);
        let focus = Path::new(OsStr::from_bytes(b"caf\xE9/rs"));
        let (tx, rx) = oneshot::channel();
        let handle = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .start_accepted(
                        SessionId::from_uuid(Uuid::now_v7()),
                        SessionCommandId::from_uuid(Uuid::now_v7()),
                        "hi".into(),
                        binding(),
                        Some(focus),
                        tx,
                    )
                    .await
            })
        };
        let accepted = rx.await.unwrap().unwrap();
        assert!(matches!(accepted, latte_core::CreateOutcome::Created(_)));
        let finished = handle.await.unwrap().unwrap();
        assert!(matches!(finished, latte_core::CreateOutcome::Created(_)));
    }

    #[tokio::test]
    async fn provider_turn_bridges_progress_events_to_sink() {
        struct EventProvider;
        impl Provider for EventProvider {
            fn complete(
                &self,
                _: ProviderRequest,
                context: ProviderContext,
            ) -> crate::provider::ProviderFuture<'_> {
                Box::pin(async move {
                    if let Some(sink) = &context.events {
                        sink.observe(ProviderEvent::Attempt { number: 1 });
                    }
                    Ok(simple_response("done"))
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<dyn SessionProgressSink> = {
            let observed = Arc::clone(&observed);
            Arc::new(move |_session_id, progress| observed.lock().unwrap().push(progress))
        };
        let provider = Arc::new(EventProvider);
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            factory,
        )
        .with_progress_sink(sink);
        service
            .start(
                SessionId::from_uuid(Uuid::now_v7()),
                "hi".into(),
                binding(),
                None,
            )
            .await
            .unwrap();
        let events = observed.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionTransientProgress::ProviderAttempt { number: 1, .. }
        )));
    }

    #[test]
    fn verification_descriptor_requires_active_run() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let mut snapshot = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            Arc::new(|_| Err("unused".into())),
        )
        .with_verification(VerificationPlan {
            argv: vec!["/bin/true".into()],
            cwd: ".".into(),
            timeout_ms: 1_000,
            grace_ms: 100,
            stdout_cap: 1_024,
            stderr_cap: 1_024,
        });
        snapshot.active_turn_id = None;
        assert!(matches!(
            service.verification_descriptor(&snapshot, "done"),
            Err(SessionRuntimeError::InvalidState)
        ));
    }

    #[tokio::test]
    async fn begin_verification_without_active_run_fails() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let mut snapshot = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        snapshot.active_turn_id = None;
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            Arc::new(|_| Err("unused".into())),
        );
        let lease = service.acquire(session_id).unwrap();
        let err = service
            .begin_verification(snapshot, "summary".into(), &lease)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionRuntimeError::InvalidState));
    }

    #[test]
    fn finish_verification_without_active_run_fails() {
        use latte_engine::SessionEffectPresentation;
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let mut snapshot = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        let service = SessionRuntimeService::new(
            engine,
            root.path(),
            SessionHistoryPolicy::default(),
            Arc::new(|_| Err("unused".into())),
        );
        let lease = service.acquire(session_id).unwrap();
        let presentation = SessionEffectPresentation {
            effect_id: "e".into(),
            tool_call_id: "t".into(),
            name: "process".into(),
            input: serde_json::json!({}),
            attempt: 1,
        };
        // No active run → InvalidState at the active_turn_id guard.
        snapshot.active_turn_id = None;
        assert!(matches!(
            service.finish_verification(&snapshot, &presentation, &lease),
            Err(SessionRuntimeError::InvalidState)
        ));
        // Active run id but missing from runs → InvalidState at turn_revision.
        snapshot.active_turn_id = Some(turn_id);
        snapshot.turns = vec![];
        assert!(matches!(
            service.finish_verification(&snapshot, &presentation, &lease),
            Err(SessionRuntimeError::InvalidState)
        ));
    }

    #[test]
    fn tool_round_for_call_skips_entries_without_tool_calls() {
        use latte_core::{TranscriptEntry, TranscriptEntryId};
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let mut snapshot = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        let call = crate::provider::ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
        };
        snapshot.transcript.entries = vec![
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 1,
                turn_id: Some(turn_id),
                kind: TranscriptKind::Assistant,
                text: "no calls here".into(),
                payload: Some(serde_json::json!({})),
                source_key: "empty".into(),
                created_at_ms: 1,
            },
            TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: 2,
                turn_id: Some(turn_id),
                kind: TranscriptKind::Assistant,
                text: "with calls".into(),
                payload: Some(serde_json::json!({"tool_calls":[call.clone()]})),
                source_key: "real".into(),
                created_at_ms: 2,
            },
        ];
        let (sequence, calls, ordinal) = tool_round_for_call(&snapshot, "call-1").unwrap();
        assert_eq!((sequence, ordinal), (2, 0));
        assert_eq!(calls, vec![call]);
    }

    #[test]
    fn effect_request_helpers_reject_missing_turn_revision() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let turn_id = new_turn_id();
        let mut snapshot = engine
            .create_session_v2(session_id, turn_id, binding(), "initial", 1)
            .unwrap();
        // Active run id set but the run is absent from the summary list.
        snapshot.active_turn_id = Some(turn_id);
        snapshot.turns = vec![];
        let descriptor = SessionEffectDescriptor {
            effect_id: "e".into(),
            tool_call_id: "t".into(),
            name: "read_file".into(),
            input: serde_json::json!({}),
            attempt: 1,
        };
        assert!(session_effect_request(&snapshot, descriptor.clone(), "k".into()).is_err());
        assert!(session_effect_start_request(&snapshot, "e".into(), "k".into()).is_err());
        // A snapshot without any active run also fails.
        snapshot.active_turn_id = None;
        assert!(session_effect_request(&snapshot, descriptor, "k".into()).is_err());
    }

    /// A catalog whose profile enables compaction with a tight request
    /// budget, so a long first turn is forced out of the follow-up window.
    fn compacting_catalog() -> Arc<ProfileCatalog> {
        Arc::new(ProfileCatalog::without_registry(ContextPolicy {
            max_request_bytes: 5_600,
            max_input_bytes: 5_600,
            reserved_output_bytes: 1,
            context_cap_bytes: 64 * 1024,
            max_tool_rounds: None,
            provider_timeout_ms: 60_000,
            compaction: latte_core::CompactionPolicy {
                strategy: latte_core::CompactionStrategy::SummarizeOnDiscard,
                ..latte_core::CompactionPolicy::default()
            },
            token_estimate: latte_core::TokenEstimateParams::default(),
        }))
    }

    fn tight_policy() -> SessionHistoryPolicy {
        SessionHistoryPolicy {
            max_request_bytes: 5_600,
            max_input_bytes: 5_600,
            reserved_output_bytes: 1,
            context_cap_bytes: 64 * 1024,
            ..SessionHistoryPolicy::default()
        }
    }

    #[tokio::test]
    async fn compaction_enabled_follow_up_summarizes_discarded_history() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(Some(&"A".repeat(2_000)), vec![]),
            response(Some("COMPACT-SUMMARY-MARKER"), vec![]),
            response(Some("second done"), vec![]),
        ]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), tight_policy(), factory)
            .with_profile_catalog(compacting_catalog());
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "x".repeat(3_000), binding(), None)
            .await
            .unwrap();
        let snapshot = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();

        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3, "start + summarize + continuation");
        // The summarize request: profile summarize instructions plus the
        // bounded plain text of the discarded first segment.
        assert!(
            matches!(
                &requests[1][0],
                Message::System { content } if content.contains("compacting the earlier history")
            ),
            "summarize request must carry the profile summarize slot"
        );
        assert!(
            matches!(
                &requests[1][1],
                Message::User { content } if content.contains("xxx") && content.contains("AAA")
            ),
            "summarize request must carry the discarded history text"
        );
        // The continuation request carries the durable summary instead of the
        // discarded segment.
        assert!(
            requests[2].iter().any(|message| matches!(
                message,
                Message::User { content } if content.contains("COMPACT-SUMMARY-MARKER")
            )),
            "continuation must carry the generated summary"
        );
        assert!(
            !requests[2].iter().any(
                |message| matches!(message, Message::User { content } if content.contains("xxx"))
            ),
            "discarded history must not re-enter the continuation request"
        );
        // The durable compaction card records the summary and the superseded
        // sequence watermark.
        let card = snapshot
            .transcript
            .entries
            .iter()
            .find(|entry| entry.kind == TranscriptKind::CompactSummary)
            .expect("compact summary card must be durable");
        assert_eq!(card.text, "COMPACT-SUMMARY-MARKER");
        assert!(
            card.payload
                .as_ref()
                .and_then(|payload| payload.get("superseded_through_sequence"))
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|sequence| sequence >= 1),
            "superseded watermark must be recorded"
        );
    }

    #[tokio::test]
    async fn compaction_disabled_keeps_the_silent_discard_behavior() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(Some(&"A".repeat(2_000)), vec![]),
            response(Some("second done"), vec![]),
        ]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        // No profile catalog: the base policy keeps compaction disabled —
        // the exact pre-compaction behavior.
        let service = SessionRuntimeService::new(engine, root.path(), tight_policy(), factory);
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "x".repeat(3_000), binding(), None)
            .await
            .unwrap();
        let snapshot = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            2,
            "no summary request without compaction"
        );
        assert!(
            !snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::CompactSummary),
            "no compaction card without compaction"
        );
    }

    #[tokio::test]
    async fn compaction_summary_overflow_degrades_with_a_durable_audit() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(Some(&"A".repeat(2_000)), vec![]),
            response(Some(&"X".repeat(6_000)), vec![]),
            response(Some("second done"), vec![]),
        ]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), tight_policy(), factory)
            .with_profile_catalog(compacting_catalog());
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "x".repeat(3_000), binding(), None)
            .await
            .unwrap();
        let snapshot = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            3,
            "the summary request is still attempted"
        );
        assert!(
            !snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::CompactSummary),
            "an overflowing summary is never persisted"
        );
        assert!(
            snapshot.transcript.entries.iter().any(|entry| {
                entry.kind == TranscriptKind::System
                    && entry.text.contains("context compaction failed")
            }),
            "degradation must leave a durable audit card"
        );
    }

    #[tokio::test]
    async fn compaction_summary_failure_degrades_with_a_durable_audit() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(Some(&"A".repeat(2_000)), vec![]),
            response(Some("second done"), vec![]),
        ]));
        // The summarize request (index 1, right after the first turn)
        // fails; the continuation request (index 2) consumes the scripted
        // response.
        provider.fail_request(1);
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), tight_policy(), factory)
            .with_profile_catalog(compacting_catalog());
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "x".repeat(3_000), binding(), None)
            .await
            .unwrap();
        let snapshot = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        assert!(
            snapshot.transcript.entries.iter().any(|entry| {
                entry.kind == TranscriptKind::System
                    && entry.text.contains("context compaction failed")
            }),
            "a failed summary must degrade with a durable audit card"
        );
        assert!(
            !snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::CompactSummary),
            "a failed summary is never persisted"
        );
    }

    /// The input path compacts on the same contract as a new child: the
    /// `ProvideInput` commit advances both revisions, so the summary card
    /// CASes on the fresh values and the turn continues from the commit's
    /// returned snapshot. This is the regression test for the review
    /// finding that the append used stale CAS coordinates and silently
    /// discarded its own result — compaction burned a summary request
    /// without ever persisting the card on this path.
    #[tokio::test]
    async fn compaction_on_the_input_path_persists_the_summary_card() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            // Turn 1: long content that the follow-up window must discard.
            response(Some(&"A".repeat(2_000)), vec![]),
            // Follow-up compaction summary for the discarded turn 1. It is
            // intentionally large: the input window must overflow again, or
            // the second compaction never triggers.
            response(Some(&"B".repeat(3_000)), vec![]),
            // Turn 2 continuation parks at an input request.
            ProviderResponse {
                message: None,
                tool_calls: vec![],
                input_request: Some(InputRequest {
                    id: "shape".into(),
                    prompt: "Which shape?".into(),
                    secret: false,
                }),
                usage: crate::provider::ProviderUsage::default(),
                finish_reason: None,
                provider_state: None,
            },
            // The large input value forces another discard: the window loses
            // the summary card and the second prompt, so a fresh summary is
            // requested before the input continuation.
            response(Some("INPUT-PATH-SUMMARY-MARKER"), vec![]),
            // The input continuation completes the turn.
            response(Some("input done"), vec![]),
        ]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), tight_policy(), factory)
            .with_profile_catalog(compacting_catalog());
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "x".repeat(3_000), binding(), None)
            .await
            .unwrap();
        let waiting = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingInput);

        let completed = service
            .provide_input(
                session_id,
                waiting.revision,
                test_turn_revision(&waiting),
                "shape".into(),
                "y".repeat(2_000),
            )
            .await
            .unwrap();
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);

        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            5,
            "turn1 + input park + input summary + input continuation"
        );
        // The input path's summary request carries the card from the new
        // child path plus the superseded prompt text.
        assert!(matches!(
            &requests[3][0],
            Message::System { content } if content.contains("compacting the earlier history")
        ));
        assert!(matches!(
            &requests[3][1],
            Message::User { content }
                if content.contains("BBB")
                    // The current input value must be part of the summary
                    // source: the fresh card lands after the input entry.
                    && content.contains("yyy")
        ));
        // The input continuation carries the fresh summary instead of the
        // superseded range.
        assert!(requests[4].iter().any(|message| matches!(
            message,
            Message::User { content } if content.contains("INPUT-PATH-SUMMARY-MARKER")
        )));
        // Two durable cards: one from the new child, one from the input
        // path — both on fresh CAS coordinates.
        let cards: Vec<_> = completed
            .transcript
            .entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::CompactSummary)
            .collect();
        assert_eq!(cards.len(), 2, "both compaction points persist their card");
    }

    #[tokio::test]
    async fn compact_summary_card_supersedes_older_history_in_later_windows() {
        let root = tempfile::tempdir().unwrap();
        let engine = EngineBuilder::new()
            .workspace_root(root.path())
            .build()
            .unwrap();
        let provider = Arc::new(RecordingProvider::scripted([
            response(Some(&"A".repeat(2_000)), vec![]),
            response(Some("COMPACT-SUMMARY-MARKER"), vec![]),
            response(Some("second done"), vec![]),
            response(Some("third done"), vec![]),
        ]));
        let factory_provider = Arc::clone(&provider);
        let factory: SessionProviderFactory = Arc::new(move |_| {
            Ok(ResolvedProvider {
                provider: factory_provider.clone(),
                binding: crate::registry::ProviderBinding::direct(&[]),
            })
        });
        let service = SessionRuntimeService::new(engine, root.path(), tight_policy(), factory)
            .with_profile_catalog(compacting_catalog());
        let session_id = SessionId::from_uuid(Uuid::now_v7());
        let ready = service
            .start(session_id, "x".repeat(3_000), binding(), None)
            .await
            .unwrap();
        let second = service
            .follow_up(session_id, ready.revision, "second".into())
            .await
            .unwrap();
        service
            .follow_up(session_id, second.revision, "third".into())
            .await
            .unwrap();
        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            4,
            "no new summary request: nothing discarded"
        );
        let continuation = &requests[3];
        assert!(
            continuation.iter().any(|message| matches!(
                message,
                Message::User { content } if content.contains("COMPACT-SUMMARY-MARKER")
            )),
            "the summary travels into every later window"
        );
        assert!(
            continuation.iter().any(|message| matches!(
                message,
                Message::Assistant { content: Some(text), .. } if text.contains("second done")
            )),
            "post-card history stays in the window"
        );
        assert!(
            !continuation.iter().any(|message| matches!(
                message,
                Message::User { content } if content.trim() == "second"
            )),
            "the superseded prompt travels only inside the summary"
        );
        assert!(
            !continuation.iter().any(|message| matches!(
                message,
                Message::User { content } if content.contains("xxx")
            )),
            "pre-summary history is permanently superseded"
        );
    }
}
