//! Private `SQLite` authority for durable engine state.
use crate::VerificationEvidence;
use latte_core::{
    CompletionPolicy, EventEnvelope, EventId, Evidence, FailureCode, Handoff, PROTOCOL_VERSION,
    Paged, Retryability, RuntimeEvent, SessionEvent, SessionEventEnvelope, SessionEventId,
    SessionLifecycle, SessionPendingRequest, SessionProviderBinding, SessionSnapshot,
    SessionSummary, SessionTurnStatus, SessionTurnSummary, TranscriptEntry, TranscriptEntryId,
    TranscriptKind, TranscriptPage, Transition, TurnFailure, TurnId, TurnState, TurnStatus,
    VerificationStatus, redact_session_text, redact_session_value,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, sync::Mutex};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 15;
const LEGACY_RUNTIME_LEASE_SCOPE: &str = "runtime";
/// The interactive session list carries a recent, bounded transcript per
/// session.  The bound prevents a single long-running conversation from
/// allocating an unbounded amount of terminal projection memory while still
/// being large enough to cover normal active conversations.
const SESSION_PROJECTION_TRANSCRIPT_LIMIT: usize = 500;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite storage failure: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
    #[error("database schema version {found} is newer than supported version {supported}")]
    NewerSchema { found: i64, supported: i64 },
    #[error("run {0} was not found")]
    TurnNotFound(TurnId),
    #[error("stale run revision: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("runtime lease is held by another owner")]
    EngineUnavailable,
    #[error("runtime lease was lost")]
    LeaseLost,
    #[error("effect terminal write was fenced")]
    EffectFenced,
    #[error("session {0} was not found")]
    SessionNotFound(latte_core::SessionId),
    #[error("session revision is stale: expected {expected}, actual {actual}")]
    StaleSessionRevision { expected: u64, actual: u64 },
    #[error("linked session turns must use CommitSessionTurnUpdate")]
    LinkedTurnRequiresSessionCommit,
    #[error("session command id was reused with different content")]
    SessionCommandReplayMismatch,
    #[error("session {0} already exists")]
    SessionAlreadyExists(latte_core::SessionId),
    #[error("session does not have the requested active turn")]
    SessionActiveTurnMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub sequence: u64,
    pub envelope: EventEnvelope,
}

/// Durable session event returned only after its containing `SQLite` transaction
/// has committed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredSessionEvent {
    pub sequence: u64,
    pub envelope: SessionEventEnvelope,
}

/// Engine-only mutation variants for a linked child turn. The direct engine
/// APIs reject these turn IDs so every state/event/transcript write is kept
/// in one fenced transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum CommitSessionTurnUpdate {
    Start {
        source_key: String,
    },
    AppendTranscript {
        source_key: String,
        kind: TranscriptKind,
        text: String,
        payload: Option<serde_json::Value>,
    },
    /// Persists a fully validated v2 effect before an external operation can
    /// begin.  The descriptor is already redacted by the engine wrapper.
    PrepareEffect {
        source_key: String,
        effect_id: String,
        operation_digest: String,
        /// Redacted projection. This is the only descriptor shape that may
        /// enter the effect ledger, transcript, event stream, or command
        /// deduplication record.
        descriptor_json: String,
        /// Exact engine-private descriptor. It is written only to the
        /// restricted v2 descriptor table in the same transaction as the
        /// preparation record, and is never returned by a session snapshot.
        canonical_descriptor_json: String,
        policy: SessionEffectPolicy,
        description: String,
        checkpoint_json: String,
    },
    /// Fenced, single-use transition from durable preparation to external
    /// authority.  This is the only v2 path which can make an effect started.
    StartEffect {
        source_key: String,
        effect_id: String,
        operation_digest: String,
        checkpoint_json: String,
    },
    /// Records a certified terminal observation and its redacted provider
    /// result before the caller may re-enter the provider loop.
    ObserveEffect {
        source_key: String,
        effect_id: String,
        operation_digest: String,
        success: bool,
        result: String,
        payload: Option<serde_json::Value>,
        checkpoint_json: String,
    },
    /// Conservative terminal path once an effect has started but its outcome
    /// can no longer be certified.
    UnknownEffect {
        source_key: String,
        effect_id: String,
        operation_digest: String,
        checkpoint_json: String,
    },
    /// Explicit reconciliation acknowledgement for a previously unknown v2
    /// effect.  This is deliberately a v2 terminal path, never a legacy run
    /// mutation.
    ReconcileUnknownEffect {
        source_key: String,
        effect_id: String,
        checkpoint_json: String,
    },
    RequestPermission {
        source_key: String,
        request: latte_core::PendingPermission,
    },
    ResolvePermission {
        source_key: String,
        request_id: String,
        allow: bool,
        /// Engine-computed operation digest rebound to the coordinator lease
        /// which is resolving a previously prepared Ask effect. Generic
        /// permission states which do not authorize an effect leave this
        /// empty.
        rebound_operation_digest: Option<String>,
    },
    RequestInput {
        source_key: String,
        request: latte_core::PendingInput,
    },
    ProvideInput {
        source_key: String,
        request_id: String,
        value: String,
    },
    Complete {
        source_key: String,
        handoff: Handoff,
    },
    /// Completes only after the engine has recorded a passing verification
    /// result for this exact linked-child revision/effect epoch.  The caller
    /// can construct this variant only through the engine-owned verified
    /// completion method, which supplies a stable current manifest digest.
    CompleteVerified {
        source_key: String,
        summary: String,
        verification_effect_id: String,
        verified_manifest_digest: String,
        files_changed: Vec<String>,
    },
    Fail {
        source_key: String,
        failure: TurnFailure,
    },
    Interrupt {
        source_key: String,
        reconciliation_effect_id: Option<String>,
    },
}

impl CommitSessionTurnUpdate {
    fn source_key(&self) -> &str {
        match self {
            Self::Start { source_key }
            | Self::AppendTranscript { source_key, .. }
            | Self::PrepareEffect { source_key, .. }
            | Self::StartEffect { source_key, .. }
            | Self::ObserveEffect { source_key, .. }
            | Self::UnknownEffect { source_key, .. }
            | Self::ReconcileUnknownEffect { source_key, .. }
            | Self::RequestPermission { source_key, .. }
            | Self::ResolvePermission { source_key, .. }
            | Self::RequestInput { source_key, .. }
            | Self::ProvideInput { source_key, .. }
            | Self::Complete { source_key, .. }
            | Self::CompleteVerified { source_key, .. }
            | Self::Fail { source_key, .. }
            | Self::Interrupt { source_key, .. } => source_key,
        }
    }
}

/// Policy result captured durably during v2 preparation.  It is intentionally
/// separate from the legacy policy module: storage only accepts the result
/// which the engine computed from its private registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEffectPolicy {
    Allow,
    Ask,
}

/// Exact mutation preconditions.  `command_id` is deduplicated using a
/// canonical digest of the raw request identity; replaying it with different
/// content fails closed (422 `idempotency_mismatch`) before any write.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionCommitRequest {
    pub session_id: latte_core::SessionId,
    pub turn_id: TurnId,
    pub expected_session_revision: u64,
    pub expected_turn_revision: u64,
    pub command_id: latte_core::SessionCommandId,
    pub request_id: Option<String>,
    pub effect_id: Option<String>,
    pub update: CommitSessionTurnUpdate,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionCommitResponse {
    pub snapshot: SessionSnapshot,
    pub session_event: StoredSessionEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub(crate) scope: String,
    pub(crate) owner: String,
    pub(crate) fencing_token: u64,
    pub(crate) expires_at_ms: u64,
}
impl Lease {
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }
    #[must_use]
    pub const fn fencing_token(&self) -> u64 {
        self.fencing_token
    }
    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseLossRecovery {
    Interrupted(TurnState),
    FencedNoop,
    AlreadyTerminal(TurnState),
}

/// Result of recovering a stale v2 linked child.  Unlike the legacy result,
/// a session recovery includes the durable v2 event that was committed with
/// the v1 interruption and effect ledger changes.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionLeaseLossRecovery {
    Recovered(SessionCommitResponse),
    FencedNoop,
    AlreadyTerminal(SessionSnapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectStatus {
    /// Written only by `declare_effect`, which is `#[cfg(test)]`: the production
    /// paths insert `prepared` or `started` directly. Kept so the parser accepts
    /// rows an older binary may have left behind, not as a live lifecycle stage.
    Declared,
    Prepared,
    Started,
    ObservedSuccess,
    ObservedFailed,
    Unknown,
}
#[derive(Clone, Debug)]
pub(crate) struct EffectAuthority {
    turn_id: TurnId,
    expected_revision: u64,
    lease: Lease,
    effect_id: String,
    digest: String,
    attempt: u64,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct VerificationRecord {
    pub revision: u64,
    pub effect_epoch: u64,
    pub effect_id: String,
    pub passed: bool,
    pub workspace_manifest_digest: String,
    pub summary: String,
}

#[derive(Debug)]
pub(crate) struct Storage {
    connection: Mutex<Connection>,
}

impl Storage {
    pub(crate) fn open(path: &Path) -> Result<Self, StorageError> {
        Self::open_with_legacy_workspace(path, None)
    }

    pub(crate) fn open_in_workspace(
        path: &Path,
        workspace_root: &str,
    ) -> Result<Self, StorageError> {
        Self::open_with_legacy_workspace(path, Some(workspace_root))
    }

    fn open_with_legacy_workspace(
        path: &Path,
        legacy_workspace_root: Option<&str>,
    ) -> Result<Self, StorageError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Self::bootstrap(&connection, legacy_workspace_root)?;
        let storage = Self {
            connection: Mutex::new(connection),
        };
        storage.recover_at(crate::wall_now_ms())?;
        Ok(storage)
    }

    pub(crate) fn memory() -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory()?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Self::bootstrap(&connection, None)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn bootstrap(
        connection: &Connection,
        legacy_workspace_root: Option<&str>,
    ) -> Result<(), StorageError> {
        let mut version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StorageError::NewerSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version == 0 {
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"
              CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL);
              CREATE TABLE sessions(id TEXT PRIMARY KEY, created_at_ms INTEGER NOT NULL);
              CREATE TABLE runs(
                run_id TEXT PRIMARY KEY, state_json TEXT NOT NULL, status TEXT NOT NULL,
                revision INTEGER NOT NULL, last_seq INTEGER NOT NULL DEFAULT 0,
                lease_token INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
              );
              CREATE TABLE events(
                run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                seq INTEGER NOT NULL, event_id TEXT NOT NULL UNIQUE, revision INTEGER NOT NULL,
                event_json TEXT NOT NULL, created_at_ms INTEGER NOT NULL, PRIMARY KEY(run_id, seq)
              );
              CREATE TABLE command_dedup(command_id TEXT PRIMARY KEY, result_json TEXT NOT NULL, created_at_ms INTEGER NOT NULL);
              CREATE TABLE effects(
                effect_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                status TEXT NOT NULL, started_at_ms INTEGER NOT NULL, observed_at_ms INTEGER
              );
              CREATE TABLE effect_attempts(effect_id TEXT NOT NULL REFERENCES effects(effect_id) ON DELETE CASCADE,
                attempt INTEGER NOT NULL, status TEXT NOT NULL, metadata_json TEXT NOT NULL, PRIMARY KEY(effect_id, attempt));
              CREATE TABLE evidence(id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                metadata_json TEXT NOT NULL, blob_ref TEXT);
              CREATE TABLE run_read_model(run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE CASCADE,
                revision INTEGER NOT NULL, last_seq INTEGER NOT NULL, state_json TEXT NOT NULL);
              CREATE TABLE runtime_lease(singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL,
                fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL);
              INSERT INTO schema_migrations(version, applied_at_ms) VALUES(1, CAST(strftime('%s','now') AS INTEGER)*1000);
              PRAGMA user_version=1;
            ")?;
            tx.commit()?;
            version = 1;
        }
        if version == 1 {
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"
              ALTER TABLE effects ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
              ALTER TABLE effects ADD COLUMN approval_digest TEXT;
              ALTER TABLE effects ADD COLUMN descriptor_json TEXT;
              ALTER TABLE effects ADD COLUMN pre_evidence_json TEXT;
              ALTER TABLE effects ADD COLUMN post_evidence_json TEXT;
              ALTER TABLE effects ADD COLUMN prepared_at_ms INTEGER;
              INSERT INTO schema_migrations(version, applied_at_ms) VALUES(2, CAST(strftime('%s','now') AS INTEGER)*1000);
              PRAGMA user_version=2;
            ")?;
            tx.commit()?;
            version = 2;
        }
        if version == 2 {
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"
              CREATE TABLE pending_permissions(
                effect_id TEXT PRIMARY KEY REFERENCES effects(effect_id) ON DELETE CASCADE,
                run_id TEXT NOT NULL, run_revision INTEGER NOT NULL, lease_owner TEXT NOT NULL,
                lease_token INTEGER NOT NULL, approval_digest TEXT NOT NULL UNIQUE,
                consumed_at_ms INTEGER
              );
              INSERT INTO schema_migrations(version, applied_at_ms) VALUES(3, CAST(strftime('%s','now') AS INTEGER)*1000);
              PRAGMA user_version=3;
            ")?;
            tx.commit()?;
            version = 3;
        }
        if version == 3 {
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"CREATE TABLE runtime_checkpoints(run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE CASCADE,payload_json TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);INSERT INTO schema_migrations(version,applied_at_ms) VALUES(4,CAST(strftime('%s','now') AS INTEGER)*1000);PRAGMA user_version=4;")?;
            tx.commit()?;
            version = 4;
        }
        if version == 4 {
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"
              ALTER TABLE runs ADD COLUMN effect_epoch INTEGER NOT NULL DEFAULT 0;
              INSERT INTO schema_migrations(version,applied_at_ms) VALUES(5,CAST(strftime('%s','now') AS INTEGER)*1000);
              PRAGMA user_version=5;
            ")?;
            tx.commit()?;
            version = 5;
        }
        if version == 5 {
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"CREATE TABLE run_baselines(run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE CASCADE,manifest_json TEXT NOT NULL);INSERT INTO schema_migrations(version,applied_at_ms) VALUES(6,CAST(strftime('%s','now') AS INTEGER)*1000);PRAGMA user_version=6;")?;
            tx.commit()?;
            version = 6;
        }
        if version == 6 {
            let tx = connection.unchecked_transaction()?;
            // V2 deliberately has its own tables and event stream.  In
            // particular, `thread_active_runs_v2` is the sole active-run
            // authority; no v1 table is overloaded with a second meaning.
            tx.execute_batch(r"
              CREATE TABLE threads_v2(
                thread_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                last_seq INTEGER NOT NULL DEFAULT 0,
                lifecycle TEXT NOT NULL,
                binding_json TEXT NOT NULL,
                latest_run_id TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                focus TEXT
              );
              CREATE TABLE thread_runs_v2(
                run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE RESTRICT,
                thread_id TEXT NOT NULL REFERENCES threads_v2(thread_id) ON DELETE CASCADE,
                parent_run_id TEXT REFERENCES runs(run_id) ON DELETE RESTRICT,
                ordinal INTEGER NOT NULL,
                completed_at_ms INTEGER,
                UNIQUE(thread_id, ordinal)
              );
              CREATE TABLE thread_active_runs_v2(
                thread_id TEXT PRIMARY KEY REFERENCES threads_v2(thread_id) ON DELETE CASCADE,
                run_id TEXT NOT NULL UNIQUE REFERENCES thread_runs_v2(run_id) ON DELETE RESTRICT,
                lease_token INTEGER NOT NULL DEFAULT 0
              );
              CREATE TABLE conversation_outbox(
                thread_id TEXT NOT NULL REFERENCES threads_v2(thread_id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                entry_id TEXT NOT NULL UNIQUE,
                run_id TEXT REFERENCES thread_runs_v2(run_id) ON DELETE RESTRICT,
                kind TEXT NOT NULL,
                source_key TEXT NOT NULL,
                entry_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY(thread_id, seq),
                UNIQUE(thread_id, source_key)
              );
              CREATE TABLE thread_events_v2(
                thread_id TEXT NOT NULL REFERENCES threads_v2(thread_id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                event_id TEXT NOT NULL UNIQUE,
                revision INTEGER NOT NULL,
                event_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY(thread_id, seq)
              );
              CREATE TABLE thread_command_dedup_v2(
                command_id TEXT PRIMARY KEY,
                digest TEXT NOT NULL,
                result_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
              );
              CREATE TABLE thread_commit_sources_v2(
                thread_id TEXT NOT NULL REFERENCES threads_v2(thread_id) ON DELETE CASCADE,
                source_key TEXT NOT NULL,
                digest TEXT NOT NULL,
                result_json TEXT NOT NULL,
                PRIMARY KEY(thread_id, source_key)
              );
              INSERT INTO schema_migrations(version,applied_at_ms) VALUES(7,CAST(strftime('%s','now') AS INTEGER)*1000);
              PRAGMA user_version=7;
            ")?;
            tx.commit()?;
            version = 7;
        }
        if version == 7 {
            // The effects ledger and transcript deliberately retain only a
            // redacted descriptor. Exact tool/process inputs live in this
            // engine-private table so an approved operation can be resumed
            // without treating display data as executable authority.
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(r"
              CREATE TABLE thread_effect_canonical_v2(
                effect_id TEXT PRIMARY KEY REFERENCES effects(effect_id) ON DELETE CASCADE,
                run_id TEXT NOT NULL REFERENCES thread_runs_v2(run_id) ON DELETE RESTRICT,
                descriptor_json TEXT NOT NULL
              );
              INSERT INTO schema_migrations(version,applied_at_ms) VALUES(8,CAST(strftime('%s','now') AS INTEGER)*1000);
              PRAGMA user_version=8;
            ")?;
            tx.commit()?;
            version = 8;
        }
        if version == 8 {
            let tx = connection.unchecked_transaction()?;
            let has_title: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2') WHERE name=?1)",
                ["title"],
                |row| row.get(0),
            )?;
            if !has_title {
                tx.execute_batch(
                    "ALTER TABLE threads_v2 ADD COLUMN title TEXT NOT NULL DEFAULT '';",
                )?;
            }
            let has_workspace_root: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2') WHERE name=?1)",
                ["workspace_root"],
                |row| row.get(0),
            )?;
            if !has_workspace_root {
                tx.execute_batch(
                    "ALTER TABLE threads_v2 ADD COLUMN workspace_root TEXT NOT NULL DEFAULT '';",
                )?;
            }
            let legacy_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM threads_v2 WHERE workspace_root=''",
                [],
                |row| row.get(0),
            )?;
            if legacy_count > 0 {
                let workspace_root = legacy_workspace_root
                    .map(validate_workspace_root)
                    .transpose()?
                    .ok_or_else(|| {
                        StorageError::InvalidData(
                            "legacy Sessions have no workspace identity; open this database from its original workspace before sharing it"
                                .into(),
                        )
                    })?;
                tx.execute(
                    "UPDATE threads_v2 SET workspace_root=?1 WHERE workspace_root=''",
                    [workspace_root],
                )?;
                let titles = {
                    let mut statement = tx.prepare(
                        "SELECT t.thread_id, x.entry_json FROM threads_v2 t \
                         LEFT JOIN conversation_outbox x ON x.thread_id=t.thread_id \
                           AND x.seq=(SELECT MIN(y.seq) FROM conversation_outbox y \
                                      WHERE y.thread_id=t.thread_id AND y.kind='user') \
                         WHERE t.title=''",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                for (thread_id, entry_json) in titles {
                    let title = entry_json
                        .as_deref()
                        .map(serde_json::from_str::<TranscriptEntry>)
                        .transpose()
                        .map_err(invalid_json)?
                        .map_or_else(
                            || "Untitled session".into(),
                            |entry| session_title(&entry.text),
                        );
                    tx.execute(
                        "UPDATE threads_v2 SET title=?1 WHERE thread_id=?2 AND title=''",
                        params![title, thread_id],
                    )?;
                }
            }
            tx.execute_batch(
                "INSERT INTO schema_migrations(version,applied_at_ms) VALUES(9,CAST(strftime('%s','now') AS INTEGER)*1000); PRAGMA user_version=9;",
            )?;
            tx.commit()?;
            version = 9;
        }
        if version == 9 {
            let tx = connection.unchecked_transaction()?;
            // Runtime authority is isolated by scope (v2 uses
            // `thread:{thread_id}` with a unique coordinator owner). Fencing
            // tokens remain globally monotonic so startup recovery can still
            // identify the exact live authority from the token persisted on a
            // run.
            tx.execute_batch(
                r"
                CREATE TABLE runtime_lease_v10(
                  scope TEXT PRIMARY KEY,
                  owner TEXT NOT NULL UNIQUE,
                  fencing_token INTEGER NOT NULL,
                  expires_at_ms INTEGER NOT NULL
                );
                INSERT INTO runtime_lease_v10(scope,owner,fencing_token,expires_at_ms)
                  SELECT 'runtime',owner,fencing_token,expires_at_ms FROM runtime_lease;
                CREATE TABLE runtime_lease_epoch(
                  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                  last_token INTEGER NOT NULL
                );
                INSERT INTO runtime_lease_epoch(singleton,last_token)
                  SELECT 1,COALESCE(MAX(fencing_token),0) FROM runtime_lease;
                DROP TABLE runtime_lease;
                ALTER TABLE runtime_lease_v10 RENAME TO runtime_lease;
                INSERT INTO schema_migrations(version,applied_at_ms)
                  VALUES(10,CAST(strftime('%s','now') AS INTEGER)*1000);
                PRAGMA user_version=10;
                ",
            )?;
            tx.commit()?;
            version = 10;
        }
        if version == 10 {
            let tx = connection.unchecked_transaction()?;
            let has_legacy_transcript: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='thread_transcript_v2')",
                [],
                |row| row.get(0),
            )?;
            if has_legacy_transcript {
                tx.execute_batch(
                    "ALTER TABLE thread_transcript_v2 RENAME TO conversation_outbox;",
                )?;
            }
            let has_parent: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2') WHERE name='parent_thread_id')",
                [],
                |row| row.get(0),
            )?;
            if !has_parent {
                tx.execute_batch("ALTER TABLE threads_v2 ADD COLUMN parent_thread_id TEXT REFERENCES threads_v2(thread_id) ON DELETE SET NULL;")?;
            }
            tx.execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS projects(
                  project_key TEXT PRIMARY KEY,
                  vcs_identity TEXT NOT NULL UNIQUE,
                  created_at_ms INTEGER NOT NULL,
                  updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS workspaces(
                  workspace_root TEXT PRIMARY KEY,
                  project_key TEXT NOT NULL REFERENCES projects(project_key) ON DELETE RESTRICT,
                  storage_key TEXT NOT NULL UNIQUE,
                  git_common_dir TEXT,
                  first_seen_at_ms INTEGER NOT NULL,
                  last_seen_at_ms INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS legacy_imports(
                  source_path TEXT PRIMARY KEY,
                  fingerprint TEXT NOT NULL,
                  imported_at_ms INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS thread_sessions_workspace_activity
                  ON threads_v2(workspace_root,updated_at_ms DESC);
                CREATE INDEX IF NOT EXISTS thread_sessions_parent
                  ON threads_v2(parent_thread_id,created_at_ms);
                INSERT INTO schema_migrations(version,applied_at_ms)
                  VALUES(11,CAST(strftime('%s','now') AS INTEGER)*1000);
                PRAGMA user_version=11;
                ",
            )?;
            tx.commit()?;
            version = 11;
        }
        if version == 11 {
            let tx = connection.unchecked_transaction()?;
            let has_focus: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2') WHERE name=?1)",
                ["focus"],
                |row| row.get(0),
            )?;
            if !has_focus {
                tx.execute_batch("ALTER TABLE threads_v2 ADD COLUMN focus TEXT;")?;
            }
            tx.execute_batch(
                "INSERT INTO schema_migrations(version,applied_at_ms) VALUES(12,CAST(strftime('%s','now') AS INTEGER)*1000); PRAGMA user_version=12;",
            )?;
            tx.commit()?;
            version = 12;
        }
        if version == 12 {
            // Concept alignment: the v2 "thread" tables become "session", and the
            // dead v1 `sessions` table is dropped to free that name. With
            // legacy_alter_table off (the modern default), RENAME rewrites foreign
            // key references in sibling tables, so the v2 graph stays consistent.
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch("PRAGMA legacy_alter_table=OFF;")?;
            // v1 `sessions` was never written or read by shipped code and nothing
            // references it; drop it before the v2 table takes the name.
            tx.execute_batch("DROP TABLE IF EXISTS sessions;")?;
            tx.execute_batch(
                r"
                ALTER TABLE threads_v2 RENAME TO sessions;
                ALTER TABLE thread_runs_v2 RENAME TO session_runs;
                ALTER TABLE thread_active_runs_v2 RENAME TO session_active_runs;
                ALTER TABLE thread_events_v2 RENAME TO session_events;
                ALTER TABLE thread_command_dedup_v2 RENAME TO session_command_dedup;
                ALTER TABLE thread_commit_sources_v2 RENAME TO session_commit_sources;
                ALTER TABLE thread_effect_canonical_v2 RENAME TO session_effect_canonical;
                ",
            )?;
            // Rename the parent PK first so RENAME COLUMN cascades the FK column
            // references in child tables, then each local column.
            tx.execute_batch(
                r"
                ALTER TABLE sessions RENAME COLUMN thread_id TO session_id;
                ALTER TABLE sessions RENAME COLUMN parent_thread_id TO parent_session_id;
                ALTER TABLE session_runs RENAME COLUMN thread_id TO session_id;
                ALTER TABLE session_active_runs RENAME COLUMN thread_id TO session_id;
                ALTER TABLE session_events RENAME COLUMN thread_id TO session_id;
                ALTER TABLE session_commit_sources RENAME COLUMN thread_id TO session_id;
                ALTER TABLE conversation_outbox RENAME COLUMN thread_id TO session_id;
                ",
            )?;
            // RENAME TABLE leaves indexes attached but under their old names;
            // rebuild them so the catalog has no stale `thread_*` identifiers.
            tx.execute_batch(
                r"
                DROP INDEX IF EXISTS thread_sessions_workspace_activity;
                DROP INDEX IF EXISTS thread_sessions_parent;
                CREATE INDEX IF NOT EXISTS sessions_workspace_activity
                  ON sessions(workspace_root,updated_at_ms DESC);
                CREATE INDEX IF NOT EXISTS sessions_parent
                  ON sessions(parent_session_id,created_at_ms);
                ",
            )?;
            tx.execute_batch(
                "INSERT INTO schema_migrations(version,applied_at_ms) VALUES(13,CAST(strftime('%s','now') AS INTEGER)*1000); PRAGMA user_version=13;",
            )?;
            tx.commit()?;
            version = 13;
        }
        if version == 13 {
            // The per-turn tool-round budget used to be reconstructed from the
            // tail-500 transcript snapshot, which undercounts turns longer than
            // the projection bound (and counts nothing once the outbox is
            // drained into the JSONL conversation log). Persist an authoritative
            // per-turn counter incremented in the same transaction as each
            // assistant tool-round card, and backfill it from whatever durable
            // transcript rows remain.
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE session_runs ADD COLUMN tool_round_count INTEGER NOT NULL DEFAULT 0;",
            )?;
            tx.execute_batch(
                r"
                UPDATE session_runs
                   SET tool_round_count = (
                         SELECT COUNT(*) FROM conversation_outbox
                          WHERE conversation_outbox.run_id = session_runs.run_id
                            AND conversation_outbox.kind = 'assistant'
                            AND json_array_length(
                                  COALESCE(json_extract(entry_json,'$.payload.tool_calls'),'[]')
                                ) > 0
                       );
                ",
            )?;
            tx.execute_batch(
                "INSERT INTO schema_migrations(version,applied_at_ms) VALUES(14,CAST(strftime('%s','now') AS INTEGER)*1000); PRAGMA user_version=14;",
            )?;
            tx.commit()?;
            version = 14;
        }
        if version == 14 {
            // Concept alignment: the durable "run" concept is named "turn" in
            // the canonical hierarchy (Workspace → Session → Turn → Round →
            // Call → Effect). This renames the physical v1 graph and the
            // linked columns. UUID values, revisions, idempotency rows, and
            // approval identities are untouched; historical serialized rows
            // carry read-side serde aliases for their old `run_*` JSON keys.
            // With legacy_alter_table off, table and column renames rewrite
            // foreign-key references in sibling tables.
            let tx = connection.unchecked_transaction()?;
            tx.execute_batch("PRAGMA legacy_alter_table=OFF;")?;
            tx.execute_batch(
                r"
                ALTER TABLE runs RENAME TO turns;
                ALTER TABLE run_read_model RENAME TO turn_read_model;
                ALTER TABLE run_baselines RENAME TO turn_baselines;
                ALTER TABLE session_active_runs RENAME TO session_active_turns;
                ALTER TABLE session_runs RENAME TO session_turns;
                -- Renaming the parent key first cascades the referenced column
                -- into every child table's foreign key; each local column is
                -- then renamed below.
                ALTER TABLE turns RENAME COLUMN run_id TO turn_id;
                ALTER TABLE events RENAME COLUMN run_id TO turn_id;
                ALTER TABLE effects RENAME COLUMN run_id TO turn_id;
                ALTER TABLE evidence RENAME COLUMN run_id TO turn_id;
                ALTER TABLE turn_read_model RENAME COLUMN run_id TO turn_id;
                ALTER TABLE turn_baselines RENAME COLUMN run_id TO turn_id;
                ALTER TABLE runtime_checkpoints RENAME COLUMN run_id TO turn_id;
                ALTER TABLE pending_permissions RENAME COLUMN run_id TO turn_id;
                ALTER TABLE pending_permissions RENAME COLUMN run_revision TO turn_revision;
                ALTER TABLE session_turns RENAME COLUMN run_id TO turn_id;
                ALTER TABLE session_turns RENAME COLUMN parent_run_id TO parent_turn_id;
                ALTER TABLE session_active_turns RENAME COLUMN run_id TO turn_id;
                ALTER TABLE conversation_outbox RENAME COLUMN run_id TO turn_id;
                ALTER TABLE session_effect_canonical RENAME COLUMN run_id TO turn_id;
                ALTER TABLE sessions RENAME COLUMN latest_run_id TO latest_turn_id;
                ",
            )?;
            tx.execute_batch(
                "INSERT INTO schema_migrations(version,applied_at_ms) VALUES(15,CAST(strftime('%s','now') AS INTEGER)*1000); PRAGMA user_version=15;",
            )?;
            tx.commit()?;
        }
        let integrity: String =
            connection.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
        if integrity != "ok" {
            return Err(StorageError::InvalidData(format!(
                "integrity_check: {integrity}"
            )));
        }
        Ok(())
    }

    pub(crate) fn register_workspace(
        &self,
        workspace_root: &str,
        project_key: &str,
        vcs_identity: &str,
        storage_key: &str,
        git_common_dir: Option<&str>,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let workspace_root = validate_workspace_root(workspace_root)?;
        validate_catalog_key(project_key, "project key")?;
        validate_catalog_key(storage_key, "storage key")?;
        if vcs_identity.is_empty()
            || vcs_identity.len() > 8 * 1024
            || vcs_identity.chars().any(char::is_control)
        {
            return Err(StorageError::InvalidData(
                "invalid project VCS identity".into(),
            ));
        }
        if git_common_dir.is_some_and(|value| {
            value.is_empty() || value.len() > 8 * 1024 || value.chars().any(char::is_control)
        }) {
            return Err(StorageError::InvalidData(
                "invalid workspace Git common directory".into(),
            ));
        }
        let now_ms = to_i64(now_ms)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO projects(project_key,vcs_identity,created_at_ms,updated_at_ms) \
             VALUES(?1,?2,?3,?3) \
             ON CONFLICT(project_key) DO UPDATE SET \
               vcs_identity=excluded.vcs_identity,updated_at_ms=excluded.updated_at_ms",
            params![project_key, vcs_identity, now_ms],
        )?;
        tx.execute(
            "INSERT INTO workspaces(workspace_root,project_key,storage_key,git_common_dir,first_seen_at_ms,last_seen_at_ms) \
             VALUES(?1,?2,?3,?4,?5,?5) \
             ON CONFLICT(workspace_root) DO UPDATE SET \
               project_key=excluded.project_key,storage_key=excluded.storage_key,git_common_dir=excluded.git_common_dir,last_seen_at_ms=excluded.last_seen_at_ms",
            params![workspace_root, project_key, storage_key, git_common_dir, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn import_legacy_database(
        &self,
        path: &Path,
        source_path: &str,
        fingerprint: &str,
        workspace_root: &str,
        now_ms: u64,
    ) -> Result<bool, StorageError> {
        let workspace_root = validate_workspace_root(workspace_root)?;
        validate_catalog_key(fingerprint, "legacy import fingerprint")?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let current_database = conn
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default();
        if !current_database.is_empty()
            && std::fs::canonicalize(&current_database).ok().as_deref()
                == std::fs::canonicalize(path).ok().as_deref()
        {
            return Ok(false);
        }
        let previous: Option<String> = conn
            .query_row(
                "SELECT fingerprint FROM legacy_imports WHERE source_path=?1",
                [source_path],
                |row| row.get(0),
            )
            .optional()?;
        if previous.as_deref() == Some(fingerprint) {
            return Ok(false);
        }
        // A failed DETACH after a previous import leaves `legacy_import`
        // attached to this shared connection, and every later import would
        // fail at ATTACH with "already in use". The best-effort detach here
        // makes a retry after such a failure self-heal; it is allowed to
        // fail because with no stale alias attached it is a harmless no-op.
        let _ = conn.execute_batch("DETACH DATABASE legacy_import;");
        conn.execute(
            "ATTACH DATABASE ?1 AS legacy_import",
            [path.to_string_lossy().as_ref()],
        )?;
        let import_result = (|| {
            let version: i64 =
                conn.query_row("PRAGMA legacy_import.user_version", [], |row| row.get(0))?;
            // Legacy import copies the *historical* v2 layout (`threads_v2`)
            // from another database file. Only v9..=12 ever had that layout;
            // schema 13+ already uses session names and reaches this binary
            // through ordinary open-time migrations, never through import.
            if !(9..=12).contains(&version) {
                return Err(StorageError::InvalidData(format!(
                    "legacy database schema {version} cannot be imported; expected 9 through 12"
                )));
            }
            let foreign_workspace: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_import.threads_v2 WHERE workspace_root<>?1)",
                [workspace_root],
                |row| row.get(0),
            )?;
            if foreign_workspace {
                return Err(StorageError::InvalidData(
                    "legacy database contains Sessions from another workspace".into(),
                ));
            }
            if previous.is_none() {
                let collision: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM legacy_import.runs l JOIN main.turns m ON l.run_id = m.turn_id) \
                     OR EXISTS(SELECT 1 FROM legacy_import.threads_v2 l JOIN main.sessions m ON l.thread_id = m.session_id)",
                    [],
                    |row| row.get(0),
                )?;
                if collision {
                    return Err(StorageError::InvalidData(
                        "legacy import collides with existing global identifiers".into(),
                    ));
                }
            }
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // The v1 graph keeps the same rows but schema 15 renamed the tables
            // and their `run_id`/`run_revision` columns to `turn_*`. Copy with
            // explicit column lists so a legacy v9-v12 source maps onto the
            // current shape; identical-shape tables stay positional.
            for (main_table, legacy_table, column_map) in [
                (
                    "turns",
                    "runs",
                    Some((
                        "turn_id,state_json,status,revision,last_seq,lease_token,created_at_ms,updated_at_ms",
                        "run_id,state_json,status,revision,last_seq,lease_token,created_at_ms,updated_at_ms",
                    )),
                ),
                (
                    "events",
                    "events",
                    Some((
                        "turn_id,seq,event_id,revision,event_json,created_at_ms",
                        "run_id,seq,event_id,revision,event_json,created_at_ms",
                    )),
                ),
                ("command_dedup", "command_dedup", None),
                (
                    "effects",
                    "effects",
                    Some((
                        "effect_id,turn_id,status,started_at_ms,observed_at_ms",
                        "effect_id,run_id,status,started_at_ms,observed_at_ms",
                    )),
                ),
                ("effect_attempts", "effect_attempts", None),
                (
                    "evidence",
                    "evidence",
                    Some((
                        "id,turn_id,metadata_json,blob_ref",
                        "id,run_id,metadata_json,blob_ref",
                    )),
                ),
                (
                    "turn_read_model",
                    "run_read_model",
                    Some((
                        "turn_id,revision,last_seq,state_json",
                        "run_id,revision,last_seq,state_json",
                    )),
                ),
                (
                    "pending_permissions",
                    "pending_permissions",
                    Some((
                        "effect_id,turn_id,turn_revision,lease_owner,lease_token,approval_digest,consumed_at_ms",
                        "effect_id,run_id,run_revision,lease_owner,lease_token,approval_digest,consumed_at_ms",
                    )),
                ),
                (
                    "runtime_checkpoints",
                    "runtime_checkpoints",
                    Some((
                        "turn_id,payload_json,updated_at_ms",
                        "run_id,payload_json,updated_at_ms",
                    )),
                ),
                (
                    "turn_baselines",
                    "run_baselines",
                    Some(("turn_id,manifest_json", "run_id,manifest_json")),
                ),
            ] {
                match column_map {
                    Some((main_columns, legacy_columns)) => {
                        tx.execute_batch(&format!(
                            "INSERT OR IGNORE INTO main.{main_table}({main_columns}) \
                             SELECT {legacy_columns} FROM legacy_import.{legacy_table};"
                        ))?;
                    }
                    None => {
                        tx.execute_batch(&format!(
                            "INSERT OR IGNORE INTO main.{main_table} SELECT * FROM legacy_import.{legacy_table};"
                        ))?;
                    }
                }
            }
            // The legacy (schema <=12) session table is `threads_v2`; main is the
            // renamed `sessions`. It must be imported before the child tables that
            // reference it via foreign keys. Legacy v9-v12 may lack parent_thread_id
            // (added in v10) and focus (added in v12).
            // `pragma_table_info` must use the two-argument form to inspect the
            // ATTACHed schema; the schema-qualified string form silently
            // resolves against `main` and reports every legacy column missing.
            let has_parent: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2','legacy_import') WHERE name='parent_thread_id')",
                [],
                |row| row.get(0),
            )?;
            let has_focus: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2','legacy_import') WHERE name='focus')",
                [],
                |row| row.get(0),
            )?;
            // Preserve real parent linkage and focus when the source carries
            // them; substitute NULL for the columns a legacy schema predates.
            // Columns map by position: the legacy `thread_id` becomes main's
            // `session_id` (identical UUID values), and `parent_thread_id` becomes
            // `parent_session_id`.
            let parent_expr = if has_parent {
                "parent_thread_id"
            } else {
                "NULL"
            };
            let focus_expr = if has_focus { "focus" } else { "NULL" };
            tx.execute_batch(&format!(
                "INSERT OR IGNORE INTO main.sessions(session_id,revision,last_seq,lifecycle,binding_json,latest_turn_id,created_at_ms,updated_at_ms,title,workspace_root,parent_session_id,focus) \
                 SELECT thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root,{parent_expr},{focus_expr} FROM legacy_import.threads_v2;"
            ))?;
            // Legacy v2 child tables map onto the renamed main tables. Rows are
            // named explicitly: main `session_turns` gained the
            // `tool_round_count` column in schema 14, so a positional
            // `SELECT *` would not match. The counter is backfilled below once
            // the legacy transcript rows are imported.
            tx.execute_batch(
                "INSERT OR IGNORE INTO main.session_turns \
                 (turn_id,session_id,parent_turn_id,ordinal,completed_at_ms,tool_round_count) \
                 SELECT run_id,thread_id,parent_run_id,ordinal,completed_at_ms,0 \
                 FROM legacy_import.thread_runs_v2;",
            )?;
            for (legacy_name, main_name) in [
                ("thread_active_runs_v2", "session_active_turns"),
                ("thread_events_v2", "session_events"),
                ("thread_command_dedup_v2", "session_command_dedup"),
                ("thread_commit_sources_v2", "session_commit_sources"),
                ("thread_effect_canonical_v2", "session_effect_canonical"),
            ] {
                tx.execute_batch(&format!(
                    "INSERT OR IGNORE INTO main.{main_name} SELECT * FROM legacy_import.{legacy_name};"
                ))?;
            }
            let has_outbox: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_import.sqlite_master WHERE type='table' AND name='conversation_outbox')",
                [],
                |row| row.get(0),
            )?;
            let transcript_source = if has_outbox {
                "conversation_outbox"
            } else {
                "thread_transcript_v2"
            };
            tx.execute_batch(&format!(
                "INSERT OR IGNORE INTO main.conversation_outbox SELECT * FROM legacy_import.{transcript_source};"
            ))?;
            // Rebuild the imported turns' tool-round counters from their
            // durable cards, mirroring the schema-14 migration backfill.
            tx.execute_batch(
                r"
                UPDATE session_turns
                   SET tool_round_count = (
                         SELECT COUNT(*) FROM conversation_outbox
                          WHERE conversation_outbox.turn_id = session_turns.turn_id
                            AND conversation_outbox.kind = 'assistant'
                            AND json_array_length(
                                  COALESCE(json_extract(entry_json,'$.payload.tool_calls'),'[]')
                                ) > 0
                       );
                ",
            )?;
            tx.execute(
                "INSERT INTO legacy_imports(source_path,fingerprint,imported_at_ms) VALUES(?1,?2,?3) \
                 ON CONFLICT(source_path) DO UPDATE SET fingerprint=excluded.fingerprint,imported_at_ms=excluded.imported_at_ms",
                params![source_path, fingerprint, to_i64(now_ms)?],
            )?;
            tx.commit()?;
            Ok(true)
        })();
        let detach_result = conn.execute_batch("DETACH DATABASE legacy_import;");
        match (import_result, detach_result) {
            (Ok(value), Ok(())) => {
                drop(conn);
                self.recover_at(now_ms)?;
                Ok(value)
            }
            // Both failing at once is the poison case: the import error is
            // primary, but the alias is still attached and would break every
            // later import at ATTACH — the caller must see both facts (a
            // later import's pre-attach detach also self-heals this).
            (Err(error), Err(detach)) => Err(StorageError::InvalidData(format!(
                "legacy import failed ({error}); detaching legacy_import also failed \
                 ({detach}), so the alias stays attached until a later import's retry"
            ))),
            (Err(error), _) => Err(error),
            // The import committed before the detach failed: the caller must
            // not conclude the import can simply be retried — the fingerprint
            // row committed too, so a retry reports "already imported".
            (Ok(_), Err(detach)) => Err(StorageError::InvalidData(format!(
                "legacy import committed, but detaching legacy_import failed ({detach}); \
                 the import is durable and a retry will report it as already imported"
            ))),
        }
    }

    #[cfg(test)]
    pub(crate) fn create_turn(&self, state: &TurnState, now_ms: u64) -> Result<(), StorageError> {
        self.create_run_with_baseline(state, now_ms, None)
    }
    pub(crate) fn create_run_with_baseline(
        &self,
        state: &TurnState,
        now_ms: u64,
        baseline: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Result<(), StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json = serde_json::to_string(state).map_err(invalid_json)?;
        tx.execute("INSERT INTO turns(turn_id,state_json,status,revision,last_seq,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,0,?5,?5)",
            params![state.turn_id.to_string(), json, status_name(state.status), to_i64(state.revision)?, to_i64(now_ms)?])?;
        tx.execute(
            "INSERT INTO turn_read_model(turn_id,revision,last_seq,state_json) VALUES(?1,?2,0,?3)",
            params![
                state.turn_id.to_string(),
                to_i64(state.revision)?,
                serde_json::to_string(state).map_err(invalid_json)?
            ],
        )?;
        if let Some(baseline) = baseline {
            tx.execute(
                "INSERT INTO turn_baselines(turn_id,manifest_json) VALUES(?1,?2)",
                params![
                    state.turn_id.to_string(),
                    serde_json::to_string(baseline).map_err(invalid_json)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn load_turn(&self, turn_id: TurnId) -> Result<TurnState, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let json: Option<String> = conn
            .query_row(
                "SELECT state_json FROM turn_read_model WHERE turn_id=?1",
                [turn_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        json.ok_or(StorageError::TurnNotFound(turn_id))
            .and_then(|v| serde_json::from_str(&v).map_err(invalid_json))
    }

    pub(crate) fn list_runs(&self) -> Result<Vec<TurnState>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare("SELECT state_json FROM turn_read_model ORDER BY rowid")?;
        let values = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|v| serde_json::from_str(&v).map_err(invalid_json))
            .collect()
    }

    pub(crate) fn is_session_linked_turn(&self, turn_id: TurnId) -> Result<bool, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_turns WHERE turn_id=?1)",
            [turn_id.to_string()],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_session_v2(
        &self,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        binding: &SessionProviderBinding,
        workspace_root: &str,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<SessionSnapshot, StorageError> {
        self.create_session_v2_inner(
            None,
            session_id,
            turn_id,
            binding,
            workspace_root,
            prompt,
            baseline,
            None,
            now_ms,
            None,
        )
        .map(|(outcome, _)| match outcome {
            latte_core::CreateOutcome::Created(snapshot)
            | latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
        })
    }

    /// Pre-acquire durable dedup lookup for a session-create command. Returns
    /// the accepted snapshot when the same `command_id` + digest was already
    /// durably accepted (crash-safe replay), `None` on a miss, and
    /// `SessionCommandReplayMismatch` when the id was reused with a different
    /// command identity. This runs *before* lease acquisition so a retry after
    /// a crash never blocks on the dead owner's still-unexpired lease.
    pub(crate) fn lookup_create_replay(
        &self,
        command_id: &latte_core::SessionCommandId,
        session_id: latte_core::SessionId,
        workspace_root: &str,
        prompt: &str,
        binding: &SessionProviderBinding,
        focus: Option<&str>,
    ) -> Result<Option<SessionSnapshot>, StorageError> {
        let workspace_root = validate_workspace_root(workspace_root)?;
        let digest = create_command_digest(session_id, workspace_root, prompt, binding, focus);
        let legacy_digest =
            legacy_create_command_digest(session_id, workspace_root, prompt, binding, focus);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT digest,result_json FROM session_command_dedup WHERE command_id=?1",
                [command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_digest, result_json)) = row else {
            return Ok(None);
        };
        if stored_digest != digest && stored_digest != legacy_digest {
            return Err(StorageError::SessionCommandReplayMismatch);
        }
        let snapshot: SessionSnapshot = serde_json::from_str(&result_json).map_err(invalid_json)?;
        Ok(Some(snapshot))
    }

    /// Pre-acquire durable dedup lookup for a follow-up command. A hit means
    /// the follow-up was already durably accepted (possibly by a process that
    /// crashed before responding); the caller replays the snapshot and must
    /// not acquire a lease or start a runner. A same-id different-digest
    /// retry fails with [`StorageError::SessionCommandReplayMismatch`].
    pub(crate) fn lookup_follow_up_replay(
        &self,
        command_id: &latte_core::SessionCommandId,
        session_id: latte_core::SessionId,
        expected_session_revision: u64,
        prompt: &str,
    ) -> Result<Option<SessionSnapshot>, StorageError> {
        let digest = follow_up_command_digest(session_id, expected_session_revision, prompt);
        let legacy_digest =
            legacy_follow_up_command_digest(session_id, expected_session_revision, prompt);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT digest,result_json FROM session_command_dedup WHERE command_id=?1",
                [command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_digest, result_json)) = row else {
            return Ok(None);
        };
        if stored_digest != digest && stored_digest != legacy_digest {
            return Err(StorageError::SessionCommandReplayMismatch);
        }
        let snapshot: SessionSnapshot = serde_json::from_str(&result_json).map_err(invalid_json)?;
        Ok(Some(snapshot))
    }

    /// Atomically creates the Session, durable user card, linked child, and
    /// started run under the exact session lease. There is no committed token-0
    /// child for a caller to strand between acceptance and `Start`.
    ///
    /// When `command_id` is provided, the create is crash-safe idempotent: the
    /// command is deduplicated against `session_command_dedup` *inside* the
    /// write transaction (recheck after the pre-acquire lookup), a same-id
    /// different-digest retry fails with `SessionCommandReplayMismatch`, and a
    /// non-replay create for an already-existing session fails with
    /// `SessionAlreadyExists`. A same-id same-digest retry returns `Replayed`
    /// without starting a runner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_started_session_v2(
        &self,
        command_id: Option<&latte_core::SessionCommandId>,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        binding: &SessionProviderBinding,
        workspace_root: &str,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        lease: &Lease,
        now_ms: u64,
        focus: Option<&str>,
    ) -> Result<latte_core::CreateOutcome<SessionSnapshot>, StorageError> {
        let (outcome, session_event) = self.create_session_v2_inner(
            command_id,
            session_id,
            turn_id,
            binding,
            workspace_root,
            prompt,
            baseline,
            Some(lease),
            now_ms,
            focus,
        )?;
        let _ = session_event; // Event is handled by finish_session_response in engine layer
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn create_session_v2_inner(
        &self,
        command_id: Option<&latte_core::SessionCommandId>,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        binding: &SessionProviderBinding,
        workspace_root: &str,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        initial_lease: Option<&Lease>,
        now_ms: u64,
        focus: Option<&str>,
    ) -> Result<
        (
            latte_core::CreateOutcome<SessionSnapshot>,
            Option<StoredSessionEvent>,
        ),
        StorageError,
    > {
        binding.validate().map_err(StorageError::InvalidData)?;
        let workspace_root = validate_workspace_root(workspace_root)?;
        // Durable idempotency digest binds the raw request identity so that
        // payloads which collapse under redaction still produce distinct
        // digests and fail with 422 idempotency_mismatch on replay.
        let command_digest =
            create_command_digest(session_id, workspace_root, prompt, binding, focus);
        // Pre-rename (schema <13) durable accepts stored the `thread.*` digest
        // namespace; compute it from the same raw identity so an upgraded binary
        // recognizes a pre-upgrade retry as a replay instead of a mismatch.
        let legacy_command_digest =
            legacy_create_command_digest(session_id, workspace_root, prompt, binding, focus);
        // Redacted text is used for all durable readable records (transcript,
        // title); the raw prompt is never persisted.
        let prompt = redact_session_text(prompt);
        if prompt.trim().is_empty() {
            return Err(StorageError::InvalidData(
                "session prompt must not be empty".into(),
            ));
        }
        let title = session_title(&prompt);
        let expected_scope = session_lease_scope(session_id);
        if let Some(lease) = initial_lease {
            require_lease_scope(lease, &expected_scope)?;
        }
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(lease) = initial_lease {
            let authoritative: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",
                params![expected_scope, lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
                |row| row.get(0),
            )?;
            if !authoritative {
                return Err(StorageError::LeaseLost);
            }
        }
        // Durable command dedup, rechecked inside the write transaction so a
        // concurrent pair that both missed the pre-acquire lookup cannot both
        // create. A same-id same-digest retry replays; a same-id
        // different-digest retry fails with 422 idempotency_mismatch.
        if let Some(command_id) = command_id {
            let previous: Option<(String, String)> = tx
                .query_row(
                    "SELECT digest,result_json FROM session_command_dedup WHERE command_id=?1",
                    [command_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((stored_digest, result_json)) = previous {
                if stored_digest != command_digest && stored_digest != legacy_command_digest {
                    return Err(StorageError::SessionCommandReplayMismatch);
                }
                let snapshot: SessionSnapshot =
                    serde_json::from_str(&result_json).map_err(invalid_json)?;
                tx.commit()?;
                return Ok((latte_core::CreateOutcome::Replayed(snapshot), None));
            }
            // A non-replay create for an already-existing session is a conflict.
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                return Err(StorageError::SessionAlreadyExists(session_id));
            }
        } else {
            // Legacy in-process path: an existing session replays its snapshot.
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                let snapshot =
                    current_session_snapshot(&tx, session_id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
                tx.commit()?;
                return Ok((latte_core::CreateOutcome::Replayed(snapshot), None));
            }
        }
        let queued = TurnState::queued(turn_id);
        let state = if initial_lease.is_some() {
            queued
                .transition(0, Transition::Start)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?
        } else {
            queued
        };
        let turn_sequence = u64::from(initial_lease.is_some());
        let session_revision = u64::from(initial_lease.is_some());
        let session_sequence = if initial_lease.is_some() { 2 } else { 1 };
        let lease_token = initial_lease.map_or(0, |lease| lease.fencing_token);
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute(
            "INSERT INTO turns(turn_id,state_json,status,revision,last_seq,lease_token,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
            params![turn_id.to_string(), state_json, status_name(state.status), to_i64(state.revision)?, to_i64(turn_sequence)?, to_i64(lease_token)?, to_i64(now_ms)?],
        )?;
        tx.execute(
            "INSERT INTO turn_read_model(turn_id,revision,last_seq,state_json) VALUES(?1,?2,?3,?4)",
            params![
                turn_id.to_string(),
                to_i64(state.revision)?,
                to_i64(turn_sequence)?,
                serde_json::to_string(&state).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO turn_baselines(turn_id,manifest_json) VALUES(?1,?2)",
            params![
                turn_id.to_string(),
                serde_json::to_string(baseline).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO sessions(session_id,revision,last_seq,lifecycle,binding_json,latest_turn_id,created_at_ms,updated_at_ms,title,workspace_root,focus) VALUES(?1,?2,?3,'running',?4,?5,?6,?6,?7,?8,?9)",
            params![session_id.to_string(), to_i64(session_revision)?, to_i64(session_sequence)?, serde_json::to_string(binding).map_err(invalid_json)?, turn_id.to_string(), to_i64(now_ms)?, title, workspace_root, focus],
        )?;
        tx.execute(
            "INSERT INTO session_turns(turn_id,session_id,parent_turn_id,ordinal) VALUES(?1,?2,NULL,0)",
            params![turn_id.to_string(), session_id.to_string()],
        )?;
        tx.execute(
            "INSERT INTO session_active_turns(session_id,turn_id,lease_token) VALUES(?1,?2,?3)",
            params![
                session_id.to_string(),
                turn_id.to_string(),
                to_i64(lease_token)?
            ],
        )?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            // Transcript paging has an unsigned cursor, so reserve zero as
            // the initial cursor and put the first user card at one.
            sequence: 1,
            turn_id: Some(turn_id),
            kind: TranscriptKind::User,
            text: prompt,
            payload: None,
            source_key: "session:create:user".into(),
            created_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,1,?2,?3,'user',?4,?5,?6)",
            params![session_id.to_string(), entry.entry_id.to_string(), turn_id.to_string(), entry.source_key, serde_json::to_string(&entry).map_err(invalid_json)?, to_i64(now_ms)?],
        )?;
        let session_event = if initial_lease.is_some() {
            let turn_event = EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                event_id: EventId::from_uuid(Uuid::now_v7()),
                turn_id,
                revision: state.revision,
                event: RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
            };
            tx.execute(
                "INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,1,?2,?3,?4,?5)",
                params![turn_id.to_string(), turn_event.event_id.to_string(), to_i64(state.revision)?, serde_json::to_string(&turn_event).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            let envelope = SessionEventEnvelope {
                protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
                event_id: SessionEventId::from_uuid(Uuid::now_v7()),
                session_id,
                revision: session_revision,
                sequence: session_sequence,
                event: SessionEvent::LifecycleChanged {
                    lifecycle: SessionLifecycle::Running,
                    turn_id: Some(turn_id),
                },
            };
            tx.execute(
                "INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![session_id.to_string(), to_i64(session_sequence)?, envelope.event_id.to_string(), to_i64(session_revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            Some(StoredSessionEvent {
                sequence: session_sequence,
                envelope,
            })
        } else {
            None
        };
        let snapshot =
            current_session_snapshot(&tx, session_id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
        // Record the durable dedup entry so a retry after a crash (durable
        // accept before 202) replays this acceptance without re-creating the
        // session or restarting the provider runner.
        if let Some(command_id) = command_id {
            let result_json = serde_json::to_string(&snapshot).map_err(invalid_json)?;
            tx.execute(
                "INSERT INTO session_command_dedup(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",
                params![command_id.to_string(), command_digest, result_json, to_i64(now_ms)?],
            )?;
        }
        tx.commit()?;
        Ok((latte_core::CreateOutcome::Created(snapshot), session_event))
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn create_session_follow_up_v2(
        &self,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        expected_session_revision: u64,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<SessionSnapshot, StorageError> {
        self.create_session_follow_up_v2_inner(
            None,
            session_id,
            turn_id,
            expected_session_revision,
            prompt,
            baseline,
            None,
            now_ms,
        )
        .map(|(outcome, _)| match outcome {
            latte_core::CreateOutcome::Created(snapshot)
            | latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
        })
    }

    /// Atomically accepts and starts a follow-up child under the exact Session
    /// lease, preserving the completed parent if any precondition fails.
    ///
    /// When `command_id` is provided, the follow-up is crash-safe idempotent:
    /// the command is deduplicated against `session_command_dedup` *inside*
    /// the write transaction (recheck after the pre-acquire lookup), a
    /// same-id different-digest retry fails with
    /// [`StorageError::SessionCommandReplayMismatch`], and a same-id
    /// same-digest retry returns `Replayed` without starting a runner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_started_session_follow_up_v2(
        &self,
        command_id: Option<&latte_core::SessionCommandId>,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        expected_session_revision: u64,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<
        (
            latte_core::CreateOutcome<SessionSnapshot>,
            Option<StoredSessionEvent>,
        ),
        StorageError,
    > {
        self.create_session_follow_up_v2_inner(
            command_id,
            session_id,
            turn_id,
            expected_session_revision,
            prompt,
            baseline,
            Some(lease),
            now_ms,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn create_session_follow_up_v2_inner(
        &self,
        command_id: Option<&latte_core::SessionCommandId>,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        expected_session_revision: u64,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        initial_lease: Option<&Lease>,
        now_ms: u64,
    ) -> Result<
        (
            latte_core::CreateOutcome<SessionSnapshot>,
            Option<StoredSessionEvent>,
        ),
        StorageError,
    > {
        // Durable idempotency digest binds the raw request identity.
        let follow_up_digest =
            follow_up_command_digest(session_id, expected_session_revision, prompt);
        // Pre-rename (schema <13) follow-up accepts stored the
        // `thread.follow_up` digest namespace; accept it on replay so an
        // upgraded binary recognizes a pre-upgrade retry.
        let legacy_follow_up_digest =
            legacy_follow_up_command_digest(session_id, expected_session_revision, prompt);
        // Redacted text is used for all durable readable records.
        let prompt = redact_session_text(prompt);
        if prompt.trim().is_empty() {
            return Err(StorageError::InvalidData(
                "session follow-up must not be empty".into(),
            ));
        }
        let expected_scope = session_lease_scope(session_id);
        if let Some(lease) = initial_lease {
            require_lease_scope(lease, &expected_scope)?;
        }
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(lease) = initial_lease {
            let authoritative: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",
                params![expected_scope, lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
                |row| row.get(0),
            )?;
            if !authoritative {
                return Err(StorageError::LeaseLost);
            }
        }
        // Durable command dedup, rechecked inside the write transaction so a
        // concurrent pair that both missed the pre-acquire lookup cannot both
        // append a turn. A same-id same-digest retry replays; a same-id
        // different-digest retry fails with 422 idempotency_mismatch.
        if let Some(command_id) = command_id {
            let previous: Option<(String, String)> = tx
                .query_row(
                    "SELECT digest,result_json FROM session_command_dedup WHERE command_id=?1",
                    [command_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((stored_digest, result_json)) = previous {
                if stored_digest != follow_up_digest && stored_digest != legacy_follow_up_digest {
                    return Err(StorageError::SessionCommandReplayMismatch);
                }
                let snapshot: SessionSnapshot =
                    serde_json::from_str(&result_json).map_err(invalid_json)?;
                tx.commit()?;
                return Ok((latte_core::CreateOutcome::Replayed(snapshot), None));
            }
        }
        let (revision, lifecycle, latest, fork_parent): (
            i64,
            String,
            Option<String>,
            Option<String>,
        ) = tx
            .query_row(
                "SELECT revision,lifecycle,latest_turn_id,parent_session_id FROM sessions WHERE session_id=?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound(session_id))?;
        let revision = from_i64(revision)?;
        if revision != expected_session_revision {
            return Err(StorageError::StaleSessionRevision {
                expected: expected_session_revision,
                actual: revision,
            });
        }
        if lifecycle != "ready"
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_active_turns WHERE session_id=?1)",
                [session_id.to_string()],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(StorageError::InvalidData(
                "follow-up requires a ready session with no active child".into(),
            ));
        }
        if latest.is_none() && fork_parent.is_none() {
            return Err(StorageError::InvalidData(
                "follow-up session has no completed child".into(),
            ));
        }
        let parent_state = latest
            .as_ref()
            .map(|parent| {
                tx.query_row(
                    "SELECT state_json FROM turns WHERE turn_id=?1",
                    [parent],
                    |row| row.get::<_, String>(0),
                )
                .map_err(StorageError::from)
                .and_then(|state| serde_json::from_str::<TurnState>(&state).map_err(invalid_json))
            })
            .transpose()?;
        if parent_state.as_ref().is_some_and(|parent| {
            parent.status != TurnStatus::Completed
                && !(parent.status == TurnStatus::Failed
                    && parent.failure.as_ref().is_some_and(|failure| {
                        failure.retryability == Retryability::Retryable
                            || failure.code == FailureCode::PermissionDenied
                    }))
        }) {
            return Err(StorageError::InvalidData(
                "follow-up parent must be completed, retryably failed, or permission-denied".into(),
            ));
        }
        let ordinal: u64 = from_i64(tx.query_row(
            "SELECT COALESCE(MAX(ordinal),-1)+1 FROM session_turns WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get::<_, i64>(0),
        )?)?;
        let queued = TurnState::queued(turn_id);
        let state = if initial_lease.is_some() {
            queued
                .transition(0, Transition::Start)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?
        } else {
            queued
        };
        let turn_sequence = u64::from(initial_lease.is_some());
        let lease_token = initial_lease.map_or(0, |lease| lease.fencing_token);
        tx.execute("INSERT INTO turns(turn_id,state_json,status,revision,last_seq,lease_token,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",params![turn_id.to_string(),serde_json::to_string(&state).map_err(invalid_json)?,status_name(state.status),to_i64(state.revision)?,to_i64(turn_sequence)?,to_i64(lease_token)?,to_i64(now_ms)?])?;
        tx.execute(
            "INSERT INTO turn_read_model(turn_id,revision,last_seq,state_json) VALUES(?1,?2,?3,?4)",
            params![
                turn_id.to_string(),
                to_i64(state.revision)?,
                to_i64(turn_sequence)?,
                serde_json::to_string(&state).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO turn_baselines(turn_id,manifest_json) VALUES(?1,?2)",
            params![
                turn_id.to_string(),
                serde_json::to_string(baseline).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO session_turns(turn_id,session_id,parent_turn_id,ordinal) VALUES(?1,?2,?3,?4)",
            params![
                turn_id.to_string(),
                session_id.to_string(),
                latest,
                to_i64(ordinal)?
            ],
        )?;
        tx.execute(
            "INSERT INTO session_active_turns(session_id,turn_id,lease_token) VALUES(?1,?2,?3)",
            params![
                session_id.to_string(),
                turn_id.to_string(),
                to_i64(lease_token)?
            ],
        )?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session revision overflow".into()))?;
        let seq: u64 = from_i64(tx.query_row(
            "SELECT last_seq FROM sessions WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get::<_, i64>(0),
        )?)?
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidData("session sequence overflow".into()))?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: seq,
            turn_id: Some(turn_id),
            kind: TranscriptKind::User,
            text: prompt,
            payload: None,
            source_key: format!("follow-up:{turn_id}:user"),
            created_at_ms: now_ms,
        };
        tx.execute("INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,'user',?5,?6,?7)",params![session_id.to_string(),to_i64(seq)?,entry.entry_id.to_string(),turn_id.to_string(),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let summary = SessionTurnSummary {
            turn_id,
            parent_turn_id: parent_state.map(|parent| parent.turn_id),
            ordinal,
            status: SessionTurnStatus::Queued,
            turn_revision: 0,
            completed_at_ms: None,
            failure_code: None,
        };
        let event = SessionEventEnvelope {
            protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
            event_id: SessionEventId::from_uuid(Uuid::now_v7()),
            session_id,
            revision: next_revision,
            sequence: seq,
            event: SessionEvent::TurnLinked { turn: summary },
        };
        tx.execute("INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![session_id.to_string(),to_i64(seq)?,event.event_id.to_string(),to_i64(next_revision)?,serde_json::to_string(&event).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE sessions SET revision=?1,last_seq=?2,lifecycle='running',latest_turn_id=?3,updated_at_ms=?4 WHERE session_id=?5",params![to_i64(next_revision)?,to_i64(seq)?,turn_id.to_string(),to_i64(now_ms)?,session_id.to_string()])?;
        let session_event = if initial_lease.is_some() {
            let turn_event = EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                event_id: EventId::from_uuid(Uuid::now_v7()),
                turn_id,
                revision: state.revision,
                event: RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
            };
            tx.execute(
                "INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,1,?2,?3,?4,?5)",
                params![turn_id.to_string(), turn_event.event_id.to_string(), to_i64(state.revision)?, serde_json::to_string(&turn_event).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            let started_revision = next_revision
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("session revision overflow".into()))?;
            let started_sequence = seq
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("session sequence overflow".into()))?;
            let envelope = SessionEventEnvelope {
                protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
                event_id: SessionEventId::from_uuid(Uuid::now_v7()),
                session_id,
                revision: started_revision,
                sequence: started_sequence,
                event: SessionEvent::LifecycleChanged {
                    lifecycle: SessionLifecycle::Running,
                    turn_id: Some(turn_id),
                },
            };
            tx.execute(
                "INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![session_id.to_string(), to_i64(started_sequence)?, envelope.event_id.to_string(), to_i64(started_revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            tx.execute(
                "UPDATE sessions SET revision=?1,last_seq=?2 WHERE session_id=?3",
                params![
                    to_i64(started_revision)?,
                    to_i64(started_sequence)?,
                    session_id.to_string()
                ],
            )?;
            Some(StoredSessionEvent {
                sequence: started_sequence,
                envelope,
            })
        } else {
            None
        };
        let snapshot =
            current_session_snapshot(&tx, session_id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
        // Durable dedup record: a crash-safe retry with the same command_id
        // replays this acceptance instead of appending a duplicate turn.
        if let Some(command_id) = command_id {
            tx.execute(
                "INSERT INTO session_command_dedup(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",
                params![
                    command_id.to_string(),
                    follow_up_digest,
                    serde_json::to_string(&snapshot).map_err(invalid_json)?,
                    to_i64(now_ms)?
                ],
            )?;
        }
        tx.commit()?;
        Ok((latte_core::CreateOutcome::Created(snapshot), session_event))
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn switch_session_binding_v2(
        &self,
        session_id: latte_core::SessionId,
        expected_session_revision: u64,
        binding: &SessionProviderBinding,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<SessionCommitResponse, StorageError> {
        binding.validate().map_err(StorageError::InvalidData)?;
        let expected_scope = session_lease_scope(session_id);
        require_lease_scope(lease, &expected_scope)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",
            params![expected_scope, lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
            |row| row.get(0),
        )?;
        if !authoritative {
            return Err(StorageError::LeaseLost);
        }
        let (revision, sequence, lifecycle, current_binding): (i64, i64, String, String) = tx
            .query_row(
                "SELECT revision,last_seq,lifecycle,binding_json FROM sessions WHERE session_id=?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound(session_id))?;
        let revision = from_i64(revision)?;
        if revision != expected_session_revision {
            return Err(StorageError::StaleSessionRevision {
                expected: expected_session_revision,
                actual: revision,
            });
        }
        if lifecycle != "ready"
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_active_turns WHERE session_id=?1)",
                [session_id.to_string()],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(StorageError::InvalidData(
                "model switching requires a ready session with no active child".into(),
            ));
        }
        let current_binding: SessionProviderBinding =
            serde_json::from_str(&current_binding).map_err(invalid_json)?;
        if current_binding == *binding {
            return Err(StorageError::InvalidData(
                "selected provider and model are already active".into(),
            ));
        }
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session revision overflow".into()))?;
        let next_sequence = from_i64(sequence)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session sequence overflow".into()))?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: next_sequence,
            turn_id: None,
            kind: TranscriptKind::System,
            text: format!(
                "Model switched to {}/{}",
                binding.provider_name, binding.model
            ),
            payload: Some(serde_json::json!({
                "provider_name": binding.provider_name,
                "model": binding.model,
            })),
            source_key: format!("session:model:{next_revision}"),
            created_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,NULL,'system',?4,?5,?6)",
            params![session_id.to_string(), to_i64(next_sequence)?, entry.entry_id.to_string(), entry.source_key, serde_json::to_string(&entry).map_err(invalid_json)?, to_i64(now_ms)?],
        )?;
        let envelope = SessionEventEnvelope {
            protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
            event_id: SessionEventId::from_uuid(Uuid::now_v7()),
            session_id,
            revision: next_revision,
            sequence: next_sequence,
            event: SessionEvent::BindingChanged {
                provider_name: binding.provider_name.clone(),
                model: binding.model.clone(),
            },
        };
        tx.execute(
            "INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![session_id.to_string(), to_i64(next_sequence)?, envelope.event_id.to_string(), to_i64(next_revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?, to_i64(now_ms)?],
        )?;
        tx.execute(
            "UPDATE sessions SET revision=?1,last_seq=?2,binding_json=?3,updated_at_ms=?4 WHERE session_id=?5",
            params![to_i64(next_revision)?, to_i64(next_sequence)?, serde_json::to_string(binding).map_err(invalid_json)?, to_i64(now_ms)?, session_id.to_string()],
        )?;
        let snapshot =
            current_session_snapshot(&tx, session_id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
        tx.commit()?;
        Ok(SessionCommitResponse {
            snapshot,
            session_event: StoredSessionEvent {
                sequence: next_sequence,
                envelope,
            },
        })
    }

    pub(crate) fn session_snapshot_v2(
        &self,
        session_id: latte_core::SessionId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<SessionSnapshot, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        session_snapshot(&conn, session_id, after, limit)
    }

    pub(crate) fn session_snapshot_tail_v2(
        &self,
        session_id: latte_core::SessionId,
        limit: usize,
    ) -> Result<SessionSnapshot, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        current_session_snapshot(&conn, session_id, limit)
    }

    /// Reads the authoritative persisted tool-round count for one run. Unlike a
    /// tail transcript projection this never undercounts long runs and stays
    /// correct after the conversation outbox is drained.
    pub(crate) fn tool_round_count_for_turn(&self, turn_id: TurnId) -> Result<u32, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let count: i64 = conn.query_row(
            "SELECT tool_round_count FROM session_turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |row| row.get(0),
        )?;
        u32::try_from(count)
            .map_err(|_| StorageError::InvalidData("tool_round_count overflow".into()))
    }

    pub(crate) fn conversation_outbox_entries(
        &self,
        session_id: latte_core::SessionId,
    ) -> Result<Vec<TranscriptEntry>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT entry_json FROM conversation_outbox WHERE session_id=?1 ORDER BY seq ASC",
        )?;
        statement
            .query_map([session_id.to_string()], |row| row.get::<_, String>(0))?
            .map(|value| serde_json::from_str::<TranscriptEntry>(&value?).map_err(invalid_json))
            .collect()
    }

    pub(crate) fn acknowledge_conversation_outbox(
        &self,
        session_id: latte_core::SessionId,
        through_sequence: u64,
    ) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute(
            "DELETE FROM conversation_outbox WHERE session_id=?1 AND seq<=?2",
            params![session_id.to_string(), to_i64(through_sequence)?],
        )?;
        Ok(())
    }

    pub(crate) fn list_sessions(&self) -> Result<Vec<SessionSnapshot>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn
            .prepare("SELECT session_id FROM sessions ORDER BY updated_at_ms DESC, rowid DESC")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|value| {
                let id = parse_session_id(&value)?;
                // `session_snapshot` pages forward for history reconstruction.
                // The TUI instead needs the current end of a conversation: a
                // first-20 ascending page made a completed 21+ card session
                // look stale while silently hiding its newest work.
                let mut snapshot = session_snapshot(&conn, id, None, 1)?;
                snapshot.transcript =
                    session_transcript_tail(&conn, id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
                Ok(snapshot)
            })
            .collect()
    }

    pub(crate) fn list_sessions_for_workspace(
        &self,
        workspace_root: &str,
    ) -> Result<Vec<SessionSnapshot>, StorageError> {
        let workspace_root = validate_workspace_root(workspace_root)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT session_id FROM sessions WHERE workspace_root=?1 \
             ORDER BY updated_at_ms DESC, rowid DESC",
        )?;
        let ids = statement
            .query_map([workspace_root], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|value| {
                let id = parse_session_id(&value)?;
                let mut snapshot = session_snapshot(&conn, id, None, 1)?;
                snapshot.transcript =
                    session_transcript_tail(&conn, id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
                Ok(snapshot)
            })
            .collect()
    }

    pub(crate) fn list_session_summaries_for_workspace(
        &self,
        workspace_root: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let limit = limit.min(500);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms \
             FROM sessions WHERE workspace_root=?1 \
             ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?2",
        )?;
        let limit = u64::try_from(limit)
            .map_err(|_| StorageError::InvalidData("session list limit exceeds u64".into()))?;
        let rows = statement.query_map(params![workspace_root, to_i64(limit)?], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (
                session_id,
                title,
                workspace_root,
                parent,
                lifecycle,
                binding_json,
                created,
                updated,
            ) = row?;
            let binding: SessionProviderBinding =
                serde_json::from_str(&binding_json).map_err(invalid_json)?;
            Ok(SessionSummary {
                session_id: parse_session_id(&session_id)?,
                title,
                workspace_root,
                parent_session_id: parent.as_deref().map(parse_session_id).transpose()?,
                lifecycle: parse_lifecycle(&lifecycle)?,
                provider_name: binding.provider_name,
                model: binding.model,
                created_at_ms: from_i64(created)?,
                updated_at_ms: from_i64(updated)?,
            })
        })
        .collect()
    }

    pub(crate) fn find_sessions_by_exact_title_for_workspace(
        &self,
        workspace_root: &str,
        title: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        if title.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let limit = limit.min(500);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms,rowid \
             FROM sessions WHERE workspace_root=?1 AND title=?2 \
             ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                workspace_root,
                title,
                to_i64(u64::try_from(limit).map_err(|_| {
                    StorageError::InvalidData("session title limit exceeds u64".into())
                })?)?
            ],
            session_summary_row,
        )?;
        rows.map(|row| Ok(row?.0)).collect()
    }

    /// Lists one page of durable sessions bound to this workspace, newest
    /// transcript tail included, ordered by `(updated_at_ms, rowid)` descending.
    /// `cursor` is the opaque `next_cursor` of the previous page; an invalid
    /// cursor fails closed. `limit == 0` returns an empty page.
    ///
    /// # Errors
    /// Returns a storage error when the catalog cannot be read or the cursor
    /// is malformed.
    pub(crate) fn list_sessions_for_workspace_paged(
        &self,
        workspace_root: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Paged<SessionSnapshot>, StorageError> {
        let workspace_root = validate_workspace_root(workspace_root)?;
        if limit == 0 {
            return Ok(Paged {
                items: Vec::new(),
                next_cursor: None,
            });
        }
        let limit = limit.min(500);
        let keyset = cursor.map(decode_session_cursor).transpose()?;
        let fetch = limit
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session list limit overflow".into()))?
            .min(501);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let page_rows: Vec<(String, i64, i64)> = match keyset {
            None => {
                let mut statement = conn.prepare(
                    "SELECT session_id,updated_at_ms,rowid FROM sessions \
                     WHERE workspace_root=?1 \
                     ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?2",
                )?;
                let rows = statement.query_map(
                    params![
                        workspace_root,
                        to_i64(u64::try_from(fetch).map_err(|_| {
                            StorageError::InvalidData("session list limit exceeds u64".into())
                        })?)?
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            Some((updated, rowid)) => {
                let mut statement = conn.prepare(
                    "SELECT session_id,updated_at_ms,rowid FROM sessions \
                     WHERE workspace_root=?1 \
                     AND (updated_at_ms<?2 OR (updated_at_ms=?2 AND rowid<?3)) \
                     ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?4",
                )?;
                let rows = statement.query_map(
                    params![
                        workspace_root,
                        updated,
                        rowid,
                        to_i64(u64::try_from(fetch).map_err(|_| {
                            StorageError::InvalidData("session list limit exceeds u64".into())
                        })?)?
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        let next_cursor = if page_rows.len() > limit {
            let (_, updated, rowid) = page_rows[limit - 1];
            Some(encode_session_cursor(updated, rowid))
        } else {
            None
        };
        let items = page_rows
            .into_iter()
            .take(limit)
            .map(|(id, _, _)| -> Result<SessionSnapshot, StorageError> {
                let session_id = parse_session_id(&id)?;
                let mut snapshot = session_snapshot(&conn, session_id, None, 1)?;
                snapshot.transcript = session_transcript_tail(
                    &conn,
                    session_id,
                    SESSION_PROJECTION_TRANSCRIPT_LIMIT,
                )?;
                Ok(snapshot)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Paged { items, next_cursor })
    }

    /// Searches this workspace's local session catalog by title/id, one page at
    /// a time, in the same `(updated_at_ms, rowid)` descending order as
    /// [`Self::list_sessions_for_workspace_paged`].
    ///
    /// # Errors
    /// Returns a storage error when the catalog cannot be searched or the
    /// cursor is malformed.
    pub(crate) fn search_sessions_paged(
        &self,
        workspace_root: &str,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Paged<SessionSummary>, StorageError> {
        if limit == 0 {
            return Ok(Paged {
                items: Vec::new(),
                next_cursor: None,
            });
        }
        let limit = limit.min(500);
        let workspace_root = validate_workspace_root(workspace_root)?;
        let query = query.trim().to_lowercase();
        let keyset = cursor.map(decode_session_cursor).transpose()?;
        let fetch = limit
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session search limit overflow".into()))?
            .min(501);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let page_rows: Vec<(SessionSummary, i64, i64)> = match keyset {
            None => {
                let mut statement = conn.prepare(
                    "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms,rowid \
                     FROM sessions WHERE workspace_root=?1 AND \
                     (?2='' OR instr(lower(title),?2)>0 OR instr(lower(session_id),?2)>0) \
                     ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?3",
                )?;
                let rows = statement.query_map(
                    params![
                        workspace_root,
                        query,
                        to_i64(u64::try_from(fetch).map_err(|_| {
                            StorageError::InvalidData("session search limit exceeds u64".into())
                        })?)?
                    ],
                    session_summary_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            Some((updated, rowid)) => {
                let mut statement = conn.prepare(
                    "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms,rowid \
                     FROM sessions WHERE workspace_root=?1 AND \
                     (?2='' OR instr(lower(title),?2)>0 OR instr(lower(session_id),?2)>0) \
                     AND (updated_at_ms<?3 OR (updated_at_ms=?3 AND rowid<?4)) \
                     ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?5",
                )?;
                let rows = statement.query_map(
                    params![
                        workspace_root,
                        query,
                        updated,
                        rowid,
                        to_i64(u64::try_from(fetch).map_err(|_| {
                            StorageError::InvalidData("session search limit exceeds u64".into())
                        })?)?
                    ],
                    session_summary_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        let next_cursor = if page_rows.len() > limit {
            let (_, updated, rowid) = page_rows[limit - 1];
            Some(encode_session_cursor(updated, rowid))
        } else {
            None
        };
        Ok(Paged {
            items: page_rows
                .into_iter()
                .take(limit)
                .map(|(item, _, _)| item)
                .collect(),
            next_cursor,
        })
    }

    /// Finds sessions whose title exactly matches `title`, one page at a time,
    /// in the same `(updated_at_ms, rowid)` descending order as
    /// [`Self::list_sessions_for_workspace_paged`].
    ///
    /// # Errors
    /// Returns a storage error when the catalog cannot be searched or the
    /// cursor is malformed.
    pub(crate) fn find_sessions_by_exact_title_for_workspace_paged(
        &self,
        workspace_root: &str,
        title: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Paged<SessionSummary>, StorageError> {
        if title.is_empty() || limit == 0 {
            return Ok(Paged {
                items: Vec::new(),
                next_cursor: None,
            });
        }
        let limit = limit.min(500);
        let workspace_root = validate_workspace_root(workspace_root)?;
        let keyset = cursor.map(decode_session_cursor).transpose()?;
        let fetch = limit
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session title limit overflow".into()))?
            .min(501);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let page_rows: Vec<(SessionSummary, i64, i64)> = match keyset {
            None => {
                let mut statement = conn.prepare(
                    "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms,rowid \
                     FROM sessions WHERE workspace_root=?1 AND title=?2 \
                     ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?3",
                )?;
                let rows = statement.query_map(
                    params![
                        workspace_root,
                        title,
                        to_i64(u64::try_from(fetch).map_err(|_| {
                            StorageError::InvalidData("session title limit exceeds u64".into())
                        })?)?
                    ],
                    session_summary_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            Some((updated, rowid)) => {
                let mut statement = conn.prepare(
                    "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms,rowid \
                     FROM sessions WHERE workspace_root=?1 AND title=?2 \
                     AND (updated_at_ms<?3 OR (updated_at_ms=?3 AND rowid<?4)) \
                     ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?5",
                )?;
                let rows = statement.query_map(
                    params![
                        workspace_root,
                        title,
                        updated,
                        rowid,
                        to_i64(u64::try_from(fetch).map_err(|_| {
                            StorageError::InvalidData("session title limit exceeds u64".into())
                        })?)?
                    ],
                    session_summary_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };
        let next_cursor = if page_rows.len() > limit {
            let (_, updated, rowid) = page_rows[limit - 1];
            Some(encode_session_cursor(updated, rowid))
        } else {
            None
        };
        Ok(Paged {
            items: page_rows
                .into_iter()
                .take(limit)
                .map(|(item, _, _)| item)
                .collect(),
            next_cursor,
        })
    }

    pub(crate) fn session_v2(
        &self,
        session_id: latte_core::SessionId,
    ) -> Result<Option<SessionSummary>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let row = conn
            .query_row(
                "SELECT title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms \
                 FROM sessions WHERE session_id=?1",
                [session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(title, workspace_root, parent, lifecycle, binding_json, created, updated)| {
                let binding: SessionProviderBinding =
                    serde_json::from_str(&binding_json).map_err(invalid_json)?;
                Ok(SessionSummary {
                    session_id,
                    title,
                    workspace_root,
                    parent_session_id: parent.as_deref().map(parse_session_id).transpose()?,
                    lifecycle: parse_lifecycle(&lifecycle)?,
                    provider_name: binding.provider_name,
                    model: binding.model,
                    created_at_ms: from_i64(created)?,
                    updated_at_ms: from_i64(updated)?,
                })
            },
        )
        .transpose()
    }

    pub(crate) fn search_sessions(
        &self,
        workspace_root: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let query = query.trim().to_lowercase();
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT session_id,title,workspace_root,parent_session_id,lifecycle,binding_json,created_at_ms,updated_at_ms,rowid \
             FROM sessions WHERE workspace_root=?1 AND \
             (?2='' OR instr(lower(title),?2)>0 OR instr(lower(session_id),?2)>0) \
             ORDER BY updated_at_ms DESC,rowid DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                workspace_root,
                query,
                to_i64(u64::try_from(limit.min(500)).map_err(|_| {
                    StorageError::InvalidData("session search limit exceeds u64".into())
                })?)?
            ],
            session_summary_row,
        )?;
        rows.map(|row| Ok(row?.0)).collect()
    }

    pub(crate) fn rename_session(
        &self,
        session_id: latte_core::SessionId,
        title: &str,
    ) -> Result<(), StorageError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StorageError::InvalidData(
                "session title must not be empty".into(),
            ));
        }
        let title = session_title(title);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET title=?1 WHERE session_id=?2",
            params![title, session_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StorageError::SessionNotFound(session_id));
        }
        Ok(())
    }

    pub(crate) fn create_session_fork(
        &self,
        source_session_id: latte_core::SessionId,
        fork_session_id: latte_core::SessionId,
        history: &[TranscriptEntry],
        title: Option<&str>,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (source_title, workspace_root, binding_json, focus): (
            String,
            String,
            String,
            Option<String>,
        ) = tx
            .query_row(
                "SELECT title,workspace_root,binding_json,focus FROM sessions WHERE session_id=?1",
                [source_session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound(source_session_id))?;
        let title = title.map_or_else(
            || session_title(&format!("{source_title} (fork)")),
            session_title,
        );
        let last_sequence = history.last().map_or(0, |entry| entry.sequence);
        tx.execute(
            "INSERT INTO sessions(session_id,revision,last_seq,lifecycle,binding_json,latest_turn_id,created_at_ms,updated_at_ms,title,workspace_root,parent_session_id,focus) \
             VALUES(?1,0,?2,'ready',?3,NULL,?4,?4,?5,?6,?7,?8)",
            params![fork_session_id.to_string(),to_i64(last_sequence)?,binding_json,to_i64(now_ms)?,title,workspace_root,source_session_id.to_string(),focus],
        )?;
        let mut previous = 0;
        for source in history {
            if source.sequence <= previous {
                return Err(StorageError::InvalidData(
                    "fork history is not a strictly increasing sequence".into(),
                ));
            }
            previous = source.sequence;
            let mut entry = source.clone();
            entry.entry_id = TranscriptEntryId::from_uuid(Uuid::now_v7());
            entry.turn_id = None;
            entry.source_key = format!("fork:{source_session_id}:{}", source.sequence);
            // The fork replays this history to the provider, so redaction is
            // re-applied at copy time: entries written by older binaries —
            // before any redaction hardening — must not survive into the new
            // session unredacted. Redaction is a fixed point over
            // already-redacted text, so this never corrupts current entries.
            entry.text = redact_session_text(&entry.text);
            entry.payload = entry.payload.map(redact_session_value);
            tx.execute(
                "INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) \
                 VALUES(?1,?2,?3,NULL,?4,?5,?6,?7)",
                params![fork_session_id.to_string(),to_i64(entry.sequence)?,entry.entry_id.to_string(),transcript_kind_name(entry.kind),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(entry.created_at_ms)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Computes the exact workspace paths changed since this linked child was
    /// created.  The engine supplies the fresh manifest; the baseline never
    /// leaves durable storage except as this checked display list.
    pub(crate) fn session_changed_files(
        &self,
        turn_id: TurnId,
        current_manifest: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let baseline_json: String = conn
            .query_row(
                "SELECT manifest_json FROM turn_baselines WHERE turn_id=?1",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StorageError::InvalidData("linked child has no engine-owned baseline".into())
            })?;
        let baseline: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&baseline_json).map_err(invalid_json)?;
        let mut changed = std::collections::BTreeSet::new();
        for key in baseline.keys().chain(current_manifest.keys()) {
            if baseline.get(key) != current_manifest.get(key) {
                changed.insert(key.clone());
            }
        }
        let mut displayed = std::collections::BTreeMap::<String, String>::new();
        for encoded in changed {
            let components: Vec<String> = serde_json::from_str(&encoded).map_err(invalid_json)?;
            if components.is_empty()
                || components.iter().any(|component| {
                    component.is_empty()
                        || component.contains('/')
                        || component
                            .chars()
                            .any(|value| value == '\0' || value.is_control())
                })
            {
                return Err(StorageError::InvalidData(
                    "invalid manifest component key".into(),
                ));
            }
            let display = components.join("/");
            if displayed.insert(display, encoded).is_some() {
                return Err(StorageError::InvalidData(
                    "manifest display path collision".into(),
                ));
            }
        }
        Ok(displayed.into_keys().collect())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn commit_session_turn_update(
        &self,
        request: &SessionCommitRequest,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<SessionCommitResponse, StorageError> {
        let expected_scope = session_lease_scope(request.session_id);
        require_lease_scope(lease, &expected_scope)?;
        validate_session_source(request.update.source_key())?;
        let digest = session_command_digest(request)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous_command: Option<(String, String)> = tx
            .query_row(
                "SELECT digest,result_json FROM session_command_dedup WHERE command_id=?1",
                [request.command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_digest, result)) = previous_command {
            if stored_digest != digest {
                return Err(StorageError::SessionCommandReplayMismatch);
            }
            let replay = serde_json::from_str(&result).map_err(invalid_json)?;
            tx.commit()?;
            return Ok(replay);
        }
        let previous_source: Option<(String, String)> = tx.query_row("SELECT digest,result_json FROM session_commit_sources WHERE session_id=?1 AND source_key=?2",params![request.session_id.to_string(),request.update.source_key()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((stored_digest, result)) = previous_source {
            if stored_digest != digest {
                return Err(StorageError::SessionCommandReplayMismatch);
            }
            let replay = serde_json::from_str(&result).map_err(invalid_json)?;
            tx.execute("INSERT INTO session_command_dedup(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",params![request.command_id.to_string(),digest,result,to_i64(now_ms)?])?;
            tx.commit()?;
            return Ok(replay);
        }
        let lease_ok: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![expected_scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|row|row.get(0))?;
        if !lease_ok {
            return Err(StorageError::LeaseLost);
        }
        let (session_revision, last_seq, lifecycle, latest_turn): (i64, i64, String, Option<String>) = tx
            .query_row(
                "SELECT revision,last_seq,lifecycle,latest_turn_id FROM sessions WHERE session_id=?1",
                [request.session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::SessionNotFound(request.session_id))?;
        let session_revision = from_i64(session_revision)?;
        if session_revision != request.expected_session_revision {
            return Err(StorageError::StaleSessionRevision {
                expected: request.expected_session_revision,
                actual: session_revision,
            });
        }
        let active: Option<(String, i64)> = tx
            .query_row(
                "SELECT turn_id,lease_token FROM session_active_turns WHERE session_id=?1",
                [request.session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // Unknown-effect reconciliation happens only after the conservative
        // terminal path has removed the active child.  It is still fenced by
        // the exact session/run/revision binding below, but must not require a
        // row that recovery deliberately cleared.
        let recovered_reconciliation = active.is_none()
            && matches!(
                &request.update,
                CommitSessionTurnUpdate::ReconcileUnknownEffect { .. }
            )
            && lifecycle == "reconciliation_required"
            && latest_turn.as_deref() == Some(request.turn_id.to_string().as_str());
        // The queue-audit card (issue #22) is appended after a turn has
        // terminalized and the active row was deliberately cleared: queued
        // prompts that can no longer execute must still leave a durable
        // trace. The append stays fenced by the exact session revision, the
        // lease, and the just-terminalized `latest_turn_id`, so only the
        // latest turn of a terminal session can ever be appended this way.
        let terminal_queue_audit = active.is_none()
            && matches!(
                &request.update,
                CommitSessionTurnUpdate::AppendTranscript { .. }
            )
            && matches!(
                lifecycle.as_str(),
                "failed" | "interrupted" | "reconciliation_required"
            )
            && latest_turn.as_deref() == Some(request.turn_id.to_string().as_str());
        if let Some((active_turn, active_token)) = active {
            if active_turn != request.turn_id.to_string()
                || from_i64(active_token)? > lease.fencing_token
            {
                return Err(StorageError::SessionActiveTurnMismatch);
            }
        } else if !recovered_reconciliation && !terminal_queue_audit {
            return Err(StorageError::SessionActiveTurnMismatch);
        }
        let (state_json, turn_seq, turn_token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM turns WHERE turn_id=?1",
            [request.turn_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if from_i64(turn_token)? > lease.fencing_token {
            return Err(StorageError::LeaseLost);
        }
        let current: TurnState = serde_json::from_str(&state_json).map_err(invalid_json)?;
        if current.revision != request.expected_turn_revision {
            return Err(StorageError::StaleRevision {
                expected: request.expected_turn_revision,
                actual: current.revision,
            });
        }

        let mut next = current.clone();
        let mut turn_changed = false;
        let mut next_lifecycle = lifecycle;
        let mut card: Option<(TranscriptKind, String, Option<serde_json::Value>, String)> = None;
        let mut terminal = false;
        let mut reconciliation_effect = None;
        let mut checkpoint: Option<String> = None;
        match &request.update {
            CommitSessionTurnUpdate::Start { .. } => {
                next = current
                    .transition(current.revision, Transition::Start)
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                next_lifecycle = "running".into();
            }
            CommitSessionTurnUpdate::AppendTranscript {
                source_key,
                kind,
                text,
                payload,
            } => {
                card = Some((
                    *kind,
                    redact_session_text(text),
                    payload.clone().map(redact_session_value),
                    source_key.clone(),
                ));
            }
            CommitSessionTurnUpdate::PrepareEffect {
                source_key,
                effect_id,
                operation_digest,
                descriptor_json,
                canonical_descriptor_json,
                policy,
                description,
                checkpoint_json,
            } => {
                validate_session_effect_id(effect_id)?;
                validate_session_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(descriptor_json).map_err(invalid_json)?;
                serde_json::from_str::<crate::SessionEffectDescriptor>(canonical_descriptor_json)
                    .map_err(invalid_json)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                if current.status != TurnStatus::Running {
                    return Err(StorageError::InvalidData(
                        "only a running linked child can prepare an effect".into(),
                    ));
                }
                let pre_evidence = match policy {
                    SessionEffectPolicy::Allow => r#"{"session_policy":"allow"}"#,
                    SessionEffectPolicy::Ask => r#"{"session_policy":"ask"}"#,
                };
                tx.execute("INSERT INTO effects(effect_id,turn_id,status,started_at_ms,attempt,descriptor_json,approval_digest,pre_evidence_json,prepared_at_ms) VALUES(?1,?2,'prepared',?3,1,?4,?5,?6,?3)",params![effect_id,request.turn_id.to_string(),to_i64(now_ms)?,descriptor_json,operation_digest,pre_evidence])?;
                tx.execute(
                    "INSERT INTO session_effect_canonical(effect_id,turn_id,descriptor_json) VALUES(?1,?2,?3)",
                    params![effect_id, request.turn_id.to_string(), canonical_descriptor_json],
                )?;
                if *policy == SessionEffectPolicy::Ask {
                    let pending = latte_core::PendingPermission {
                        request_id: redact_session_text(effect_id),
                        operation_digest: redact_session_text(operation_digest),
                        description: redact_session_text(description),
                    };
                    next = current
                        .transition(current.revision, Transition::RequestPermission(pending))
                        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                    turn_changed = true;
                    next_lifecycle = "waiting_permission".into();
                    let post_approval_revision = current
                        .revision
                        .checked_add(2)
                        .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
                    tx.execute("INSERT INTO pending_permissions(effect_id,turn_id,turn_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![effect_id,request.turn_id.to_string(),to_i64(post_approval_revision)?,lease.owner,to_i64(lease.fencing_token)?,operation_digest])?;
                }
                card = Some((
                    TranscriptKind::ToolCall,
                    redact_session_text(description),
                    Some(redact_session_value(serde_json::json!({
                        "descriptor": serde_json::from_str::<serde_json::Value>(descriptor_json)
                            .map_err(invalid_json)?,
                        "operation_digest": operation_digest,
                    }))),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitSessionTurnUpdate::StartEffect {
                source_key,
                effect_id,
                operation_digest,
                checkpoint_json,
            } => {
                validate_session_effect_id(effect_id)?;
                validate_session_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                if current.status != TurnStatus::Running
                    || current.pending_permission.is_some()
                    || current.pending_input.is_some()
                {
                    return Err(StorageError::InvalidData(
                        "effect start requires a running child without a pending request".into(),
                    ));
                }
                let prepared: Option<(String, String)> = tx
                    .query_row(
                        "SELECT approval_digest,pre_evidence_json FROM effects WHERE effect_id=?1 AND turn_id=?2 AND status='prepared'",
                        params![effect_id, request.turn_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let Some((stored_digest, policy_marker)) = prepared else {
                    return Err(StorageError::InvalidData(
                        "effect is not a prepared linked effect".into(),
                    ));
                };
                if stored_digest != *operation_digest {
                    return Err(StorageError::InvalidData("effect digest mismatch".into()));
                }
                let pending: Option<(i64, String, i64, Option<i64>)> = tx
                    .query_row(
                        "SELECT turn_revision,lease_owner,lease_token,consumed_at_ms FROM pending_permissions WHERE effect_id=?1 AND turn_id=?2",
                        params![effect_id, request.turn_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                if let Some((bound_revision, owner, token, consumed)) = pending {
                    if from_i64(bound_revision)? != current.revision
                        || owner != lease.owner
                        || from_i64(token)? != lease.fencing_token
                        || consumed.is_some()
                    {
                        return Err(StorageError::InvalidData(
                            "prepared permission is stale, mismatched, or consumed".into(),
                        ));
                    }
                    tx.execute(
                        "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND consumed_at_ms IS NULL",
                        params![to_i64(now_ms)?, effect_id],
                    )?;
                } else if policy_marker != r#"{"session_policy":"allow"}"# {
                    return Err(StorageError::InvalidData(
                        "prepared effect has no durable allow authorization".into(),
                    ));
                }
                let started = tx.execute(
                    "UPDATE effects SET status='started',started_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='prepared' AND approval_digest=?4",
                    params![to_i64(now_ms)?,effect_id,request.turn_id.to_string(),operation_digest],
                )?;
                if started != 1 {
                    return Err(StorageError::EffectFenced);
                }
                let bumped = tx.execute(
                    "UPDATE turns SET effect_epoch=effect_epoch+1,lease_token=?1,updated_at_ms=?2 WHERE turn_id=?3 AND revision=?4 AND lease_token<=?1",
                    params![to_i64(lease.fencing_token)?,to_i64(now_ms)?,request.turn_id.to_string(),to_i64(current.revision)?],
                )?;
                if bumped != 1 {
                    return Err(StorageError::LeaseLost);
                }
                card = Some((
                    TranscriptKind::System,
                    "tool started".into(),
                    Some(
                        serde_json::json!({"effect_id":redact_session_text(effect_id),"status":"started"}),
                    ),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitSessionTurnUpdate::ObserveEffect {
                source_key,
                effect_id,
                operation_digest,
                success,
                result,
                payload,
                checkpoint_json,
            } => {
                validate_session_effect_id(effect_id)?;
                validate_session_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                if current.status != TurnStatus::Running {
                    return Err(StorageError::InvalidData(
                        "effect observation requires a running linked child".into(),
                    ));
                }
                let changed = tx.execute(
                    "UPDATE effects SET status=?1,post_evidence_json=?2,observed_at_ms=?3 WHERE effect_id=?4 AND turn_id=?5 AND status='started' AND approval_digest=?6",
                    params![if *success { "observed_success" } else { "observed_failed" },serde_json::to_string(&redact_session_value(serde_json::json!({"result":result,"payload":payload.clone()}))).map_err(invalid_json)?,to_i64(now_ms)?,effect_id,request.turn_id.to_string(),operation_digest],
                )?;
                if changed != 1 {
                    return Err(StorageError::EffectFenced);
                }
                card = Some((
                    TranscriptKind::ToolResult,
                    redact_session_text(result),
                    payload.clone().map(redact_session_value),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitSessionTurnUpdate::UnknownEffect {
                source_key,
                effect_id,
                operation_digest,
                checkpoint_json,
            } => {
                validate_session_effect_id(effect_id)?;
                validate_session_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                let changed = tx.execute(
                    r#"UPDATE effects SET status='unknown',post_evidence_json='{"outcome":"uncertain"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='started' AND approval_digest=?4"#,
                    params![to_i64(now_ms)?,effect_id,request.turn_id.to_string(),operation_digest],
                )?;
                if changed != 1 {
                    return Err(StorageError::EffectFenced);
                }
                let cancelling = current
                    .transition(current.revision, Transition::Cancel)
                    .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                next = cancelling
                    .transition(cancelling.revision, Transition::Interrupt)
                    .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                turn_changed = true;
                terminal = true;
                reconciliation_effect = Some(redact_session_text(effect_id));
                next_lifecycle = "reconciliation_required".into();
                card = Some((
                    TranscriptKind::Failure,
                    "effect outcome unknown; reconciliation required".into(),
                    Some(
                        serde_json::json!({"effect_id":redact_session_text(effect_id),"status":"unknown"}),
                    ),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitSessionTurnUpdate::ReconcileUnknownEffect {
                source_key,
                effect_id,
                checkpoint_json,
            } => {
                validate_session_effect_id(effect_id)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                let changed = tx.execute(
                    r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"reconciliation":"acknowledged_failed"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='unknown'"#,
                    params![to_i64(now_ms)?,effect_id,request.turn_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(StorageError::InvalidData(
                        "unknown effect does not belong to linked child".into(),
                    ));
                }
                if recovered_reconciliation {
                    // Recovery first records the v1 Interrupted state so no
                    // observer can infer the external result.  A later
                    // explicit acknowledgement terminalizes that exact
                    // interrupted child without fabricating a legal v1
                    // transition that the state machine does not expose.
                    if current.status != TurnStatus::Interrupted {
                        return Err(StorageError::InvalidData(
                            "recovered reconciliation requires an interrupted child".into(),
                        ));
                    }
                    next.status = TurnStatus::Failed;
                    next.revision = current
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
                    next.failure = Some(TurnFailure {
                        code: FailureCode::RuntimeFailed,
                        message: format!(
                            "unknown effect {} acknowledged failed; turn aborted",
                            redact_session_text(effect_id)
                        ),
                        retryability: Retryability::Terminal,
                    });
                    next.pending_input = None;
                    next.pending_permission = None;
                } else {
                    next = current
                        .transition(
                            current.revision,
                            Transition::Fail(TurnFailure {
                                code: FailureCode::RuntimeFailed,
                                message: format!(
                                    "unknown effect {} acknowledged failed; turn aborted",
                                    redact_session_text(effect_id)
                                ),
                                retryability: Retryability::Terminal,
                            }),
                        )
                        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                }
                turn_changed = true;
                terminal = true;
                next_lifecycle = "failed".into();
                card = Some((
                    TranscriptKind::Failure,
                    "unknown effect acknowledged failed; turn aborted".into(),
                    Some(
                        serde_json::json!({"effect_id":redact_session_text(effect_id),"status":"reconciled"}),
                    ),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitSessionTurnUpdate::RequestPermission {
                source_key,
                request: pending,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::RequestPermission(redact_permission(pending)),
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                next_lifecycle = "waiting_permission".into();
                card = Some((
                    TranscriptKind::Permission,
                    next.pending_permission.as_ref().map_or_else(
                        || "permission requested".into(),
                        |value| redact_session_text(&value.description),
                    ),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitSessionTurnUpdate::ResolvePermission {
                source_key,
                request_id,
                allow,
                rebound_operation_digest,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::ResolvePermission {
                            request_id: redact_session_text(request_id),
                            allowed: *allow,
                        },
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                terminal = !allow;
                next_lifecycle = if *allow {
                    "running".into()
                } else {
                    // Denial terminalizes this immutable child, but it does
                    // not terminalize the conversation. The user may explain
                    // the denial or choose another approach in a new child.
                    "ready".into()
                };
                if *allow {
                    // A waiting Session deliberately releases its coordinator
                    // lease before returning to the caller.  The next
                    // coordinator therefore owns a newer fencing epoch. For
                    // an Ask effect, transfer both the single-use capability
                    // and its engine-computed operation digest to that epoch
                    // in this same permission-resolution transaction.
                    let pending: Option<(i64, String, i64, String, String)> = tx
                        .query_row(
                            "SELECT p.turn_revision,p.lease_owner,p.lease_token,\
                                    p.approval_digest,e.approval_digest \
                             FROM pending_permissions p \
                             JOIN effects e ON e.effect_id=p.effect_id AND e.turn_id=p.turn_id \
                             WHERE p.effect_id=?1 AND p.turn_id=?2 \
                               AND p.consumed_at_ms IS NULL AND e.status='prepared'",
                            params![request_id, request.turn_id.to_string()],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    if let Some((bound_revision, owner, token, pending_digest, effect_digest)) =
                        pending
                    {
                        if from_i64(bound_revision)? != next.revision
                            || pending_digest != effect_digest
                        {
                            return Err(StorageError::InvalidData(
                                "prepared permission binding is corrupt or stale".into(),
                            ));
                        }
                        let changed_epoch =
                            owner != lease.owner || from_i64(token)? != lease.fencing_token;
                        let digest = rebound_operation_digest.as_deref();
                        if changed_epoch && digest.is_none() {
                            return Err(StorageError::LeaseLost);
                        }
                        if let Some(digest) = digest {
                            validate_session_digest(digest)?;
                            let effect_changed = tx.execute(
                                "UPDATE effects SET approval_digest=?1 \
                                 WHERE effect_id=?2 AND turn_id=?3 AND status='prepared' \
                                   AND approval_digest=?4",
                                params![
                                    digest,
                                    request_id,
                                    request.turn_id.to_string(),
                                    effect_digest
                                ],
                            )?;
                            let permission_changed = tx.execute(
                                "UPDATE pending_permissions \
                                 SET lease_owner=?1,lease_token=?2,approval_digest=?3 \
                                 WHERE effect_id=?4 AND turn_id=?5 AND turn_revision=?6 \
                                   AND approval_digest=?7 AND consumed_at_ms IS NULL",
                                params![
                                    lease.owner,
                                    to_i64(lease.fencing_token)?,
                                    digest,
                                    request_id,
                                    request.turn_id.to_string(),
                                    to_i64(next.revision)?,
                                    pending_digest
                                ],
                            )?;
                            if effect_changed != 1 || permission_changed != 1 {
                                return Err(StorageError::EffectFenced);
                            }
                        }
                    } else if rebound_operation_digest.is_some() {
                        return Err(StorageError::InvalidData(
                            "prepared permission capability is missing".into(),
                        ));
                    }
                }
                card = Some((
                    if *allow {
                        TranscriptKind::System
                    } else {
                        TranscriptKind::Failure
                    },
                    if *allow {
                        "permission allowed".into()
                    } else {
                        "permission denied".into()
                    },
                    (!allow).then(|| {
                        serde_json::json!({
                            "provider_tool_round_aborted": "permission_denied"
                        })
                    }),
                    format!("{source_key}:card"),
                ));
                if !allow {
                    // A prepared ask effect has never crossed the Started
                    // boundary.  Denial consumes its approval capability and
                    // records a terminal non-execution observation in the
                    // same transaction which removes the active child.
                    tx.execute(
                        "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND consumed_at_ms IS NULL",
                        params![to_i64(now_ms)?, request_id, request.turn_id.to_string()],
                    )?;
                    tx.execute(
                        r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"permission":"denied_before_start"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='prepared'"#,
                        params![to_i64(now_ms)?, request_id, request.turn_id.to_string()],
                    )?;
                }
            }
            CommitSessionTurnUpdate::RequestInput {
                source_key,
                request: pending,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::RequestInput(redact_input(pending)),
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                next_lifecycle = "waiting_input".into();
                card = Some((
                    TranscriptKind::Input,
                    next.pending_input.as_ref().map_or_else(
                        || "input requested".into(),
                        |value| redact_session_text(&value.prompt),
                    ),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitSessionTurnUpdate::ProvideInput {
                source_key,
                request_id,
                value,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::ProvideInput {
                            request_id: redact_session_text(request_id),
                        },
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                next_lifecycle = "running".into();
                card = Some((
                    TranscriptKind::User,
                    redact_session_text(value),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitSessionTurnUpdate::Complete {
                source_key,
                handoff,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::Complete {
                            handoff: redact_handoff(handoff),
                            policy: CompletionPolicy::VerificationNotRequired,
                        },
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                terminal = true;
                next_lifecycle = "ready".into();
                card = Some((
                    TranscriptKind::Completion,
                    next.handoff.as_ref().map_or_else(
                        || "completed".into(),
                        |value| redact_session_text(&value.summary),
                    ),
                    next.handoff
                        .as_ref()
                        .map(|handoff| serde_json::json!({"handoff": redact_handoff(handoff)})),
                    format!("{source_key}:card"),
                ));
            }
            CommitSessionTurnUpdate::CompleteVerified {
                source_key,
                summary,
                verification_effect_id,
                verified_manifest_digest,
                files_changed,
            } => {
                let effect_epoch = from_i64(tx.query_row(
                    "SELECT effect_epoch FROM turns WHERE turn_id=?1",
                    [request.turn_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )?)?;
                let metadata: Option<String> = tx
                    .query_row(
                        "SELECT metadata_json FROM evidence WHERE turn_id=?1 ORDER BY rowid DESC LIMIT 1",
                        [request.turn_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let record = metadata
                    .map(|raw| {
                        serde_json::from_str::<VerificationRecord>(&raw).map_err(invalid_json)
                    })
                    .transpose()?
                    .filter(|record| {
                        record.revision == current.revision
                            && record.effect_epoch == effect_epoch
                            && record.effect_id == *verification_effect_id
                            && record.passed
                            && record.workspace_manifest_digest == *verified_manifest_digest
                    })
                    .ok_or_else(|| {
                        StorageError::InvalidData(
                            "missing current passing verification evidence for linked child".into(),
                        )
                    })?;
                let handoff = Handoff {
                    summary: redact_session_text(summary),
                    files_changed: files_changed
                        .iter()
                        .map(|path| redact_session_text(path))
                        .collect(),
                    evidence: vec![Evidence {
                        name: format!(
                            "verification: {}",
                            redact_session_text(verification_effect_id)
                        ),
                        status: VerificationStatus::Passed,
                        summary: format!(
                            "{}; verified_manifest_sha256={}; verified_at_ms={now_ms}; change_source=manifest_v1",
                            redact_session_text(&record.summary),
                            redact_session_text(verified_manifest_digest),
                        ),
                    }],
                };
                next = current
                    .transition(
                        current.revision,
                        Transition::Complete {
                            handoff,
                            policy: CompletionPolicy::VerificationRequired,
                        },
                    )
                    .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                turn_changed = true;
                terminal = true;
                next_lifecycle = "ready".into();
                card = Some((
                    TranscriptKind::Completion,
                    next.handoff.as_ref().map_or_else(
                        || "completed".into(),
                        |value| redact_session_text(&value.summary),
                    ),
                    next.handoff
                        .as_ref()
                        .map(|handoff| serde_json::json!({"handoff": redact_handoff(handoff)})),
                    format!("{source_key}:card"),
                ));
            }
            CommitSessionTurnUpdate::Fail {
                source_key,
                failure,
            } => {
                let retryable = failure.retryability == Retryability::Retryable;
                next = current
                    .transition(current.revision, Transition::Fail(redact_failure(failure)))
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                turn_changed = true;
                terminal = true;
                // A terminal child record and a usable conversation are
                // separate concerns. Provider/configuration failures are
                // durable but retryable, so the active child is removed while
                // the Session returns to Ready and the composer remains
                // available for a new immutable child.
                next_lifecycle = if retryable { "ready" } else { "failed" }.into();
                card = Some((
                    TranscriptKind::Failure,
                    next.failure.as_ref().map_or_else(
                        || "failed".into(),
                        |value| redact_session_text(&value.message),
                    ),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitSessionTurnUpdate::Interrupt {
                source_key,
                reconciliation_effect_id,
            } => {
                reconciliation_effect = reconciliation_effect_id
                    .as_ref()
                    .map(|value| redact_session_text(value));
                if matches!(
                    current.status,
                    TurnStatus::WaitingPermission | TurnStatus::WaitingInput
                ) {
                    // Waiting requests have not started an external effect. The
                    // mandatory cancellation mapping is terminal Cancelled,
                    // not an ambiguous interruption.
                    next.revision = current
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
                    next.status = TurnStatus::Failed;
                    next.pending_input = None;
                    next.pending_permission = None;
                    next.failure = Some(TurnFailure {
                        code: FailureCode::Cancelled,
                        message: "run cancelled while waiting".into(),
                        retryability: Retryability::Terminal,
                    });
                    reconciliation_effect = None;
                    next_lifecycle = "failed".into();
                    if let Some(permission) = current.pending_permission.as_ref() {
                        tx.execute(
                            "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND consumed_at_ms IS NULL",
                            params![to_i64(now_ms)?, permission.request_id, request.turn_id.to_string()],
                        )?;
                        tx.execute(
                            r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"cancelled":"before_start"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='prepared'"#,
                            params![to_i64(now_ms)?, permission.request_id, request.turn_id.to_string()],
                        )?;
                    }
                } else {
                    // A durable Started record is proof that an external
                    // call may already have happened.  A generic cancellation
                    // must therefore discover it and turn the session into a
                    // reconciliation case rather than claiming interruption.
                    if reconciliation_effect.is_none() {
                        reconciliation_effect = tx
                            .query_row(
                                "SELECT effect_id FROM effects WHERE turn_id=?1 AND status='started' ORDER BY rowid DESC LIMIT 1",
                                [request.turn_id.to_string()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?;
                    }
                    if let Some(effect_id) = reconciliation_effect.as_ref() {
                        tx.execute(
                            r#"UPDATE effects SET status='unknown',post_evidence_json='{"outcome":"cancelled_after_start"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='started'"#,
                            params![to_i64(now_ms)?, effect_id, request.turn_id.to_string()],
                        )?;
                    }
                    let cancelling = current
                        .transition(current.revision, Transition::Cancel)
                        .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                    next = cancelling
                        .transition(cancelling.revision, Transition::Interrupt)
                        .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                    next_lifecycle = if reconciliation_effect.is_some() {
                        "reconciliation_required".into()
                    } else {
                        "interrupted".into()
                    };
                }
                turn_changed = true;
                terminal = true;
                card = Some((
                    TranscriptKind::Failure,
                    if reconciliation_effect.is_some() {
                        "effect outcome unknown; reconciliation required".into()
                    } else {
                        "turn interrupted".into()
                    },
                    None,
                    format!("{source_key}:card"),
                ));
            }
        }
        let next_session_revision = session_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session revision overflow".into()))?;
        let next_sequence = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("session sequence overflow".into()))?;
        if turn_changed {
            append_linked_turn_transition(
                &tx,
                &current,
                &next,
                from_i64(turn_seq)?,
                lease,
                now_ms,
            )?;
        } else {
            let changed = tx.execute(
                "UPDATE turns SET lease_token=?1,updated_at_ms=?2 \
                 WHERE turn_id=?3 AND lease_token<=?1",
                params![
                    to_i64(lease.fencing_token)?,
                    to_i64(now_ms)?,
                    request.turn_id.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(StorageError::LeaseLost);
            }
        }
        if let Some(payload) = checkpoint {
            tx.execute(
                "INSERT INTO runtime_checkpoints(turn_id,payload_json,updated_at_ms) VALUES(?1,?2,?3) ON CONFLICT(turn_id) DO UPDATE SET payload_json=excluded.payload_json,updated_at_ms=excluded.updated_at_ms",
                params![request.turn_id.to_string(), payload, to_i64(now_ms)?],
            )?;
        }
        let transcript = if let Some((kind, text, payload, source_key)) = card {
            // Maintain the authoritative per-turn tool-round counter in the same
            // transaction as the card it describes. Reconstructing the count
            // from a transcript projection undercounts once a turn passes the
            // tail-500 bound or its outbox rows are drained into the JSONL
            // conversation log. Source-key idempotency above guarantees the
            // increment is not replayed.
            let counts_as_tool_round = kind == TranscriptKind::Assistant
                && payload
                    .as_ref()
                    .and_then(|value| value.get("tool_calls"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|calls| !calls.is_empty());
            let entry = TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: next_sequence,
                turn_id: Some(request.turn_id),
                kind,
                text,
                payload,
                source_key,
                created_at_ms: now_ms,
            };
            tx.execute("INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![request.session_id.to_string(),to_i64(next_sequence)?,entry.entry_id.to_string(),request.turn_id.to_string(),transcript_kind_name(entry.kind),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(now_ms)?])?;
            if counts_as_tool_round {
                tx.execute(
                    "UPDATE session_turns SET tool_round_count=tool_round_count+1 WHERE turn_id=?1",
                    [request.turn_id.to_string()],
                )?;
            }
            Some(entry)
        } else {
            None
        };
        if terminal {
            tx.execute(
                "UPDATE session_turns SET completed_at_ms=?1 WHERE turn_id=?2",
                params![to_i64(now_ms)?, request.turn_id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM session_active_turns WHERE session_id=?1 AND turn_id=?2",
                params![request.session_id.to_string(), request.turn_id.to_string()],
            )?;
        } else {
            tx.execute(
                "UPDATE session_active_turns SET lease_token=?1 WHERE session_id=?2 AND turn_id=?3",
                params![
                    to_i64(lease.fencing_token)?,
                    request.session_id.to_string(),
                    request.turn_id.to_string()
                ],
            )?;
        }
        tx.execute("UPDATE sessions SET revision=?1,last_seq=?2,lifecycle=?3,latest_turn_id=?4,updated_at_ms=?5 WHERE session_id=?6",params![to_i64(next_session_revision)?,to_i64(next_sequence)?,next_lifecycle,request.turn_id.to_string(),to_i64(now_ms)?,request.session_id.to_string()])?;
        let event = if let Some(entry) = transcript {
            SessionEvent::TranscriptAppended { entry }
        } else if let Some(effect_id) = reconciliation_effect {
            SessionEvent::ReconciliationRequired {
                turn_id: request.turn_id,
                effect_id,
            }
        } else {
            SessionEvent::LifecycleChanged {
                lifecycle: parse_lifecycle(&next_lifecycle)?,
                turn_id: Some(request.turn_id),
            }
        };
        let envelope = SessionEventEnvelope {
            protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
            event_id: SessionEventId::from_uuid(Uuid::now_v7()),
            session_id: request.session_id,
            revision: next_session_revision,
            sequence: next_sequence,
            event,
        };
        tx.execute("INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![request.session_id.to_string(),to_i64(next_sequence)?,envelope.event_id.to_string(),to_i64(next_session_revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let response = SessionCommitResponse {
            snapshot: current_session_snapshot(
                &tx,
                request.session_id,
                SESSION_PROJECTION_TRANSCRIPT_LIMIT,
            )?,
            session_event: StoredSessionEvent {
                sequence: next_sequence,
                envelope,
            },
        };
        let response_json = serde_json::to_string(&response).map_err(invalid_json)?;
        tx.execute("INSERT INTO session_command_dedup(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",params![request.command_id.to_string(),digest,response_json,to_i64(now_ms)?])?;
        tx.execute("INSERT INTO session_commit_sources(session_id,source_key,digest,result_json) VALUES(?1,?2,?3,?4)",params![request.session_id.to_string(),request.update.source_key(),session_command_digest(request)?,serde_json::to_string(&response).map_err(invalid_json)?])?;
        tx.commit()?;
        Ok(response)
    }

    #[cfg(test)]
    pub(crate) fn append_event(
        &self,
        next: &TurnState,
        expected_revision: u64,
        event_id: EventId,
        event: &RuntimeEvent,
        now_ms: u64,
        lease: &Lease,
    ) -> Result<StoredEvent, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owns_lease: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",
            params![lease.scope, lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
            |row| row.get(0),
        )?;
        if !owns_lease {
            return Err(StorageError::LeaseLost);
        }
        let (actual, last_seq, token): (i64, i64, i64) = tx
            .query_row(
                "SELECT revision,last_seq,lease_token FROM turns WHERE turn_id=?1",
                [next.turn_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(StorageError::TurnNotFound(next.turn_id))?;
        let (actual, last_seq, token) = (from_i64(actual)?, from_i64(last_seq)?, from_i64(token)?);
        if actual != expected_revision {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual,
            });
        }
        if lease.fencing_token < token {
            return Err(StorageError::LeaseLost);
        }
        if next.revision
            != expected_revision
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?
        {
            return Err(StorageError::InvalidData(
                "next state revision must increment once".into(),
            ));
        }
        let sequence = last_seq
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("event sequence overflow".into()))?;
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id,
            turn_id: next.turn_id,
            revision: next.revision,
            event: event.clone(),
        };
        let state_json = serde_json::to_string(next).map_err(invalid_json)?;
        let event_json = serde_json::to_string(&envelope).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![next.turn_id.to_string(), to_i64(sequence)?, event_id.to_string(), to_i64(next.revision)?, event_json, to_i64(now_ms)?])?;
        let changed = tx.execute("UPDATE turns SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE turn_id=?7 AND revision=?8",
            params![state_json, status_name(next.status), to_i64(next.revision)?, to_i64(sequence)?, to_i64(lease.fencing_token)?, to_i64(now_ms)?, next.turn_id.to_string(), to_i64(expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual,
            });
        }
        tx.execute(
            "UPDATE turn_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE turn_id=?4",
            params![
                to_i64(next.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(next).map_err(invalid_json)?,
                next.turn_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(StoredEvent { sequence, envelope })
    }
    pub(crate) fn apply_transition(
        &self,
        turn_id: TurnId,
        expected_revision: u64,
        transition: Transition,
        now_ms: u64,
        lease: &Lease,
    ) -> Result<(TurnState, StoredEvent), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let current: TurnState = serde_json::from_str(&json).map_err(invalid_json)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        if from_i64(token)? > lease.fencing_token {
            return Err(StorageError::LeaseLost);
        }
        let next = current
            .transition(expected_revision, transition)
            .map_err(|e| StorageError::InvalidData(e.to_string()))?;
        let event = if let Some(handoff) = next.handoff.clone() {
            RuntimeEvent::HandoffProduced { handoff }
        } else {
            RuntimeEvent::StateChanged {
                status: next.status,
            }
        };
        let sequence = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("event sequence overflow".into()))?;
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            turn_id,
            revision: next.revision,
            event,
        };
        let state_json = serde_json::to_string(&next).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![turn_id.to_string(),to_i64(sequence)?,envelope.event_id.to_string(),to_i64(next.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE turns SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE turn_id=?7 AND revision=?8",params![state_json,status_name(next.status),to_i64(next.revision)?,to_i64(sequence)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,turn_id.to_string(),to_i64(expected_revision)?])?;
        tx.execute(
            "UPDATE turn_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE turn_id=?4",
            params![
                to_i64(next.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(&next).map_err(invalid_json)?,
                turn_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok((next, StoredEvent { sequence, envelope }))
    }

    pub(crate) fn acquire_lease(
        &self,
        owner: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        self.acquire_scoped_lease("runtime", owner, None, now_ms, ttl_ms)
    }

    pub(crate) fn acquire_turn_lease(
        &self,
        turn_id: TurnId,
        owner: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        self.acquire_scoped_lease("runtime", owner, Some(turn_id), now_ms, ttl_ms)
    }

    pub(crate) fn acquire_session_lease(
        &self,
        session_id: latte_core::SessionId,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        let scope = session_lease_scope(session_id);
        let owner = format!("session-{session_id}-{}", Uuid::now_v7());
        self.acquire_scoped_lease(&scope, &owner, None, now_ms, ttl_ms)
    }

    fn acquire_scoped_lease(
        &self,
        scope: &str,
        owner: &str,
        turn_id: Option<TurnId>,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT owner,fencing_token,expires_at_ms FROM runtime_lease WHERE scope=?1",
                [scope],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let current = current
            .map(|(owner, token, expires)| -> Result<_, StorageError> {
                Ok((owner, from_i64(token)?, from_i64(expires)?))
            })
            .transpose()?;
        let token = match current {
            Some((held, token, expires)) if expires > now_ms && held == owner => token,
            Some((_, _, expires)) if expires > now_ms => {
                return Err(StorageError::EngineUnavailable);
            }
            None | Some(_) => {
                let last_token: i64 = tx.query_row(
                    "SELECT last_token FROM runtime_lease_epoch WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?;
                let token = from_i64(last_token)?
                    .checked_add(1)
                    .ok_or_else(|| StorageError::InvalidData("fencing token overflow".into()))?;
                tx.execute(
                    "UPDATE runtime_lease_epoch SET last_token=?1 WHERE singleton=1",
                    [to_i64(token)?],
                )?;
                token
            }
        };
        let expires = now_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| StorageError::InvalidData("lease expiry overflow".into()))?;
        tx.execute("INSERT INTO runtime_lease(scope,owner,fencing_token,expires_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(scope) DO UPDATE SET owner=excluded.owner,fencing_token=excluded.fencing_token,expires_at_ms=excluded.expires_at_ms", params![scope,owner,to_i64(token)?,to_i64(expires)?])?;
        if scope == "runtime" {
            if let Some(turn_id) = turn_id {
                let changed = tx.execute(
                    "UPDATE turns SET lease_token=?1 WHERE turn_id=?2 \
                     AND NOT EXISTS(SELECT 1 FROM session_turns WHERE session_turns.turn_id=turns.turn_id)",
                    params![to_i64(token)?, turn_id.to_string()],
                )?;
                if changed != 1 {
                    let exists: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM turns WHERE turn_id=?1)",
                        [turn_id.to_string()],
                        |row| row.get(0),
                    )?;
                    return Err(if exists {
                        StorageError::LinkedTurnRequiresSessionCommit
                    } else {
                        StorageError::TurnNotFound(turn_id)
                    });
                }
            } else {
                tx.execute(
                    "UPDATE turns SET lease_token=?1 WHERE status='queued' \
                     AND NOT EXISTS(SELECT 1 FROM session_turns WHERE session_turns.turn_id=turns.turn_id)",
                    [to_i64(token)?],
                )?;
            }
        }
        tx.commit()?;
        Ok(Lease {
            scope: scope.into(),
            owner: owner.into(),
            fencing_token: token,
            expires_at_ms: expires,
        })
    }

    pub(crate) fn renew_lease(
        &self,
        lease: &Lease,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        let expires = now_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| StorageError::InvalidData("lease expiry overflow".into()))?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let changed = conn.execute("UPDATE runtime_lease SET expires_at_ms=?1 WHERE scope=?2 AND owner=?3 AND fencing_token=?4 AND expires_at_ms>?5", params![to_i64(expires)?,lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?])?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        Ok(Lease {
            expires_at_ms: expires,
            ..lease.clone()
        })
    }

    pub(crate) fn release_lease(
        &self,
        lease: &Lease,
    ) -> Result<Option<SessionCommitResponse>, StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut recovery = None;
        if let Some(session) = lease.scope.strip_prefix("session:") {
            let session_id = parse_session_id(session)?;
            let active_turn: Option<(String, i64)> = tx
                .query_row(
                    "SELECT turn_id,lease_token FROM session_active_turns WHERE session_id=?1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            // Returning from a durable input/permission wait is a clean
            // quiescence boundary, not a coordinator crash. Token zero means
            // that no writer is authoritative and lets a later coordinator
            // acquire a fresh global epoch immediately. Nonzero stale tokens
            // remain distinguishable for conservative startup recovery.
            let quiesced = tx.execute(
                "UPDATE turns SET lease_token=0 \
                 WHERE turn_id=(SELECT turn_id FROM session_active_turns \
                               WHERE session_id=?1 AND lease_token=?2) \
                   AND lease_token=?2 \
                   AND status IN ('waiting_permission','waiting_input')",
                params![session_id.to_string(), to_i64(lease.fencing_token)?],
            )?;
            if quiesced == 1 {
                let active_quiesced = tx.execute(
                    "UPDATE session_active_turns SET lease_token=0 \
                     WHERE session_id=?1 AND lease_token=?2",
                    params![session_id.to_string(), to_i64(lease.fencing_token)?],
                )?;
                if active_quiesced != 1 {
                    return Err(StorageError::LeaseLost);
                }
            } else if let Some((turn_id, active_token)) = active_turn
                && from_i64(active_token)? == lease.fencing_token
            {
                recovery = recover_linked_session_turn(
                    &tx,
                    session_id,
                    parse_turn_id(&turn_id)?,
                    lease.fencing_token,
                    None,
                    crate::wall_now_ms(),
                )?;
            }
        }
        let changed = tx.execute(
            "DELETE FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3",
            params![lease.scope, lease.owner, to_i64(lease.fencing_token)?],
        )?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        tx.commit()?;
        Ok(recovery)
    }

    #[cfg(test)]
    pub(crate) fn start_effect(
        &self,
        id: &str,
        turn_id: TurnId,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute(
            "INSERT INTO effects(effect_id,turn_id,status,started_at_ms) VALUES(?1,?2,'started',?3)",
            params![id, turn_id.to_string(), to_i64(now_ms)?],
        )?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn declare_effect(
        &self,
        id: &str,
        turn_id: TurnId,
        attempt: u64,
        descriptor_json: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        serde_json::from_str::<serde_json::Value>(descriptor_json).map_err(invalid_json)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute("INSERT INTO effects(effect_id,turn_id,status,started_at_ms,attempt,descriptor_json) VALUES(?1,?2,'declared',?3,?4,?5)",params![id,turn_id.to_string(),to_i64(now_ms)?,to_i64(attempt)?,descriptor_json])?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn prepare_effect(
        &self,
        id: &str,
        approval_digest: &str,
        pre_evidence_json: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        serde_json::from_str::<serde_json::Value>(pre_evidence_json).map_err(invalid_json)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let changed=conn.execute("UPDATE effects SET status='prepared',approval_digest=?1,pre_evidence_json=?2,prepared_at_ms=?3 WHERE effect_id=?4 AND status='declared'",params![approval_digest,pre_evidence_json,to_i64(now_ms)?,id])?;
        if changed != 1 {
            return Err(StorageError::InvalidData("effect is not declared".into()));
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn start_prepared_effect(
        &self,
        id: &str,
        approval_digest: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let changed=conn.execute("UPDATE effects SET status='started',started_at_ms=?1 WHERE effect_id=?2 AND status='prepared' AND approval_digest=?3",params![to_i64(now_ms)?,id,approval_digest])?;
        if changed != 1 {
            return Err(StorageError::InvalidData(
                "effect preparation or approval digest mismatch".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn finish_effect(
        &self,
        authority: &EffectAuthority,
        success: bool,
        post_evidence_json: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(&authority.lease)?;
        serde_json::from_str::<serde_json::Value>(post_evidence_json).map_err(invalid_json)?;
        let status = if success {
            "observed_success"
        } else {
            "observed_failed"
        };
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE effects SET status=?1,post_evidence_json=?2,observed_at_ms=?3 WHERE effect_id=?4 AND turn_id=?5 AND status='started' AND attempt=?6 AND approval_digest=?7 AND EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?8 AND owner=?9 AND fencing_token=?10 AND expires_at_ms>?3) AND EXISTS(SELECT 1 FROM turns WHERE turn_id=?5 AND revision=?11 AND lease_token=?10)",params![status,post_evidence_json,to_i64(now_ms)?,authority.effect_id,authority.turn_id.to_string(),to_i64(authority.attempt)?,authority.digest,authority.lease.scope,authority.lease.owner,to_i64(authority.lease.fencing_token)?,to_i64(authority.expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::EffectFenced);
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn mark_effect_unknown(
        &self,
        authority: &EffectAuthority,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(&authority.lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE effects SET status='unknown',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='started' AND attempt=?4 AND approval_digest=?5 AND EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?6 AND owner=?7 AND fencing_token=?8 AND expires_at_ms>?1) AND EXISTS(SELECT 1 FROM turns WHERE turn_id=?3 AND revision=?9 AND lease_token=?8)",params![to_i64(now_ms)?,authority.effect_id,authority.turn_id.to_string(),to_i64(authority.attempt)?,authority.digest,authority.lease.scope,authority.lease.owner,to_i64(authority.lease.fencing_token)?,to_i64(authority.expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::EffectFenced);
        }
        tx.commit()?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn seed_unknown_for_recovery_test(&self, id: &str) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute(
            "UPDATE effects SET status='unknown' WHERE effect_id=?1 AND status='started'",
            [id],
        )?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn persist_permission(
        &self,
        effect_id: &str,
        turn_id: TurnId,
        turn_revision: u64,
        lease: &Lease,
        digest: &str,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute("INSERT INTO pending_permissions(effect_id,turn_id,turn_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![effect_id,turn_id.to_string(),to_i64(turn_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn create_prepared_permission(
        &self,
        effect_id: &str,
        turn_id: TurnId,
        expected_revision: u64,
        bound_revision: u64,
        attempt: u64,
        descriptor: &str,
        digest: &str,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(lease)?;
        serde_json::from_str::<serde_json::Value>(descriptor).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease l JOIN turns r ON r.turn_id=?1 WHERE l.scope=?2 AND l.owner=?3 AND l.fencing_token=?4 AND l.expires_at_ms>?5 AND r.revision=?6 AND r.lease_token=?4)",params![turn_id.to_string(),lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?,to_i64(expected_revision)?],|r|r.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        tx.execute("INSERT INTO effects(effect_id,turn_id,status,started_at_ms,attempt,descriptor_json,approval_digest,pre_evidence_json,prepared_at_ms) VALUES(?1,?2,'prepared',?3,?4,?5,?6,'{}',?3)",params![effect_id,turn_id.to_string(),to_i64(now_ms)?,to_i64(attempt)?,descriptor,digest])?;
        tx.execute("INSERT INTO pending_permissions(effect_id,turn_id,turn_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![effect_id,turn_id.to_string(),to_i64(bound_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn consume_permission_and_start(
        &self,
        effect_id: &str,
        turn_id: TurnId,
        turn_revision: u64,
        lease: &Lease,
        digest: &str,
        now_ms: u64,
    ) -> Result<EffectAuthority, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND turn_revision=?4 AND lease_owner=?5 AND lease_token=?6 AND approval_digest=?7 AND consumed_at_ms IS NULL AND EXISTS(SELECT 1 FROM turns WHERE turn_id=?3 AND revision=?4) AND EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?8 AND owner=?5 AND fencing_token=?6 AND expires_at_ms>?1)",params![to_i64(now_ms)?,effect_id,turn_id.to_string(),to_i64(turn_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest,lease.scope])?;
        if changed != 1 {
            return Err(StorageError::InvalidData(
                "permission is stale, mismatched, consumed, or lease-invalid".into(),
            ));
        }
        let started=tx.execute("UPDATE effects SET status='started',started_at_ms=?1 WHERE effect_id=?2 AND status='prepared' AND approval_digest=?3",params![to_i64(now_ms)?,effect_id,digest])?;
        if started != 1 {
            return Err(StorageError::InvalidData(
                "effect preparation or approval digest mismatch".into(),
            ));
        }
        let epoch_changed = tx.execute(
            "UPDATE turns SET effect_epoch=effect_epoch+1 WHERE turn_id=?1 AND revision=?2 AND lease_token=?3",
            params![turn_id.to_string(), to_i64(turn_revision)?, to_i64(lease.fencing_token)?],
        )?;
        if epoch_changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        let attempt = from_i64(tx.query_row(
            "SELECT attempt FROM effects WHERE effect_id=?1",
            [effect_id],
            |r| r.get(0),
        )?)?;
        tx.commit()?;
        Ok(EffectAuthority {
            turn_id,
            expected_revision: turn_revision,
            lease: lease.clone(),
            effect_id: effect_id.into(),
            digest: digest.into(),
            attempt,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_pending_effect(
        &self,
        old_effect_id: &str,
        new_effect_id: &str,
        turn_id: TurnId,
        turn_revision: u64,
        attempt: u64,
        descriptor_json: &str,
        digest: &str,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(lease)?;
        serde_json::from_str::<serde_json::Value>(descriptor_json).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expected_revision = turn_revision.saturating_sub(2);
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease l JOIN turns r ON r.turn_id=?1 JOIN pending_permissions p ON p.turn_id=r.turn_id JOIN effects e ON e.effect_id=p.effect_id AND e.turn_id=p.turn_id WHERE l.scope=?2 AND l.owner=?3 AND l.fencing_token=?4 AND l.expires_at_ms>?5 AND r.revision=?6 AND r.lease_token=?4 AND p.effect_id=?7 AND p.consumed_at_ms IS NULL AND e.status='prepared' AND e.approval_digest=p.approval_digest AND (p.lease_owner<>?3 OR p.lease_token<>?4))",params![turn_id.to_string(),lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?,to_i64(expected_revision)?,old_effect_id],|r|r.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let deleted = tx.execute("DELETE FROM pending_permissions WHERE effect_id=?1 AND turn_id=?2 AND consumed_at_ms IS NULL AND EXISTS(SELECT 1 FROM effects e WHERE e.effect_id=?1 AND e.turn_id=?2 AND e.status='prepared' AND e.approval_digest=pending_permissions.approval_digest)", params![old_effect_id,turn_id.to_string()])?;
        let abandoned = tx.execute("UPDATE effects SET status='observed_failed',post_evidence_json='{\"abandoned\":\"lease_changed\"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='prepared'",params![to_i64(now_ms)?,old_effect_id,turn_id.to_string()])?;
        if deleted != 1 || abandoned != 1 {
            return Err(StorageError::InvalidData(
                "old pending effect cannot be replaced".into(),
            ));
        }
        tx.execute("INSERT INTO effects(effect_id,turn_id,status,started_at_ms,attempt,descriptor_json,approval_digest,pre_evidence_json,prepared_at_ms) VALUES(?1,?2,'prepared',?3,?4,?5,?6,'{}',?3)",params![new_effect_id,turn_id.to_string(),to_i64(now_ms)?,to_i64(attempt)?,descriptor_json,digest])?;
        tx.execute("INSERT INTO pending_permissions(effect_id,turn_id,turn_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![new_effect_id,turn_id.to_string(),to_i64(turn_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn permission_matches(
        &self,
        effect_id: &str,
        turn_id: TurnId,
        expected_revision: u64,
        lease: &Lease,
        digest: &str,
        now_ms: u64,
    ) -> Result<bool, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM pending_permissions p JOIN turns r ON r.turn_id=p.turn_id JOIN runtime_lease l ON l.scope=?4 AND l.owner=?5 WHERE p.effect_id=?1 AND p.turn_id=?2 AND p.turn_revision=?3 AND p.lease_owner=?5 AND p.lease_token=?6 AND p.approval_digest=?7 AND p.consumed_at_ms IS NULL AND r.revision+1=?3 AND l.fencing_token=?6 AND l.expires_at_ms>?8)",params![effect_id,turn_id.to_string(),to_i64(expected_revision)?,lease.scope,lease.owner,to_i64(lease.fencing_token)?,digest,to_i64(now_ms)?],|r|r.get(0))?)
    }
    pub(crate) fn effect_status(&self, id: &str) -> Result<EffectStatus, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let status: String =
            conn.query_row("SELECT status FROM effects WHERE effect_id=?1", [id], |r| {
                r.get(0)
            })?;
        match status.as_str() {
            "declared" => Ok(EffectStatus::Declared),
            "prepared" => Ok(EffectStatus::Prepared),
            "started" => Ok(EffectStatus::Started),
            "observed" | "observed_success" => Ok(EffectStatus::ObservedSuccess),
            "observed_failed" => Ok(EffectStatus::ObservedFailed),
            "unknown" => Ok(EffectStatus::Unknown),
            _ => Err(StorageError::InvalidData(format!(
                "unknown effect status {status}"
            ))),
        }
    }
    pub(crate) fn session_effect_digest(&self, effect_id: &str) -> Result<String, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.query_row(
            "SELECT approval_digest FROM effects WHERE effect_id=?1",
            [effect_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }
    /// Returns an exact descriptor only to the engine module. The public
    /// session snapshot and all event/transcript readers use the independently
    /// redacted projection in `effects.descriptor_json` instead.
    pub(crate) fn session_effect_canonical_descriptor(
        &self,
        effect_id: &str,
        turn_id: TurnId,
    ) -> Result<crate::SessionEffectDescriptor, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let json: String = conn.query_row(
            "SELECT descriptor_json FROM session_effect_canonical WHERE effect_id=?1 AND turn_id=?2",
            params![effect_id, turn_id.to_string()],
            |row| row.get(0),
        )?;
        serde_json::from_str(&json).map_err(invalid_json)
    }
    pub(crate) fn unknown_effects_for_turn(
        &self,
        turn_id: TurnId,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT effect_id FROM effects WHERE turn_id=?1 AND status='unknown' ORDER BY effect_id",
        )?;
        Ok(stmt
            .query_map([turn_id.to_string()], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Fences a stale v2 coordinator and atomically leaves its linked child
    /// in the same conservative state used by startup recovery.  The caller
    /// must have just failed a renewal; no write here relies on that stale
    /// lease being authoritative.
    pub(crate) fn recover_session_after_lease_loss(
        &self,
        session_id: latte_core::SessionId,
        turn_id: TurnId,
        lost_lease: &Lease,
        expected_turn_revision: u64,
        now_ms: u64,
    ) -> Result<SessionLeaseLossRecovery, StorageError> {
        let expected_scope = session_lease_scope(session_id);
        require_lease_scope(lost_lease, &expected_scope)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let still_authoritative: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",
            params![
                expected_scope,
                lost_lease.owner,
                to_i64(lost_lease.fencing_token)?,
                to_i64(now_ms)?
            ],
            |row| row.get(0),
        )?;
        if still_authoritative {
            return Err(StorageError::InvalidData(
                "lease is still authoritative".into(),
            ));
        }
        let state_json: Option<String> = tx
            .query_row(
                "SELECT state_json FROM turns WHERE turn_id=?1",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state_json) = state_json else {
            return Err(StorageError::TurnNotFound(turn_id));
        };
        let state: TurnState = serde_json::from_str(&state_json).map_err(invalid_json)?;
        if matches!(
            state.status,
            TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
        ) {
            let snapshot =
                current_session_snapshot(&tx, session_id, SESSION_PROJECTION_TRANSCRIPT_LIMIT)?;
            tx.commit()?;
            return Ok(SessionLeaseLossRecovery::AlreadyTerminal(snapshot));
        }
        let response = recover_linked_session_turn(
            &tx,
            session_id,
            turn_id,
            lost_lease.fencing_token,
            Some(expected_turn_revision),
            now_ms,
        )?;
        let Some(response) = response else {
            tx.commit()?;
            return Ok(SessionLeaseLossRecovery::FencedNoop);
        };
        tx.commit()?;
        Ok(SessionLeaseLossRecovery::Recovered(response))
    }
    pub(crate) fn interrupt_after_lease_loss(
        &self,
        turn_id: TurnId,
        lost_lease: &Lease,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<LeaseLossRecovery, StorageError> {
        require_legacy_runtime_lease(lost_lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lost_lease.scope,lost_lease.owner,to_i64(lost_lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
        if valid {
            return Err(StorageError::InvalidData(
                "lease is still authoritative".into(),
            ));
        }
        let (json, last_seq, turn_token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut state: TurnState = serde_json::from_str(&json).map_err(invalid_json)?;
        if from_i64(turn_token)? != lost_lease.fencing_token || state.revision != expected_revision
        {
            return Ok(LeaseLossRecovery::FencedNoop);
        }
        if !matches!(state.status, TurnStatus::Running | TurnStatus::Cancelling) {
            return Ok(LeaseLossRecovery::AlreadyTerminal(state));
        }
        state.status = TurnStatus::Interrupted;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
        let seq = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("sequence overflow".into()))?;
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            turn_id,
            revision: state.revision,
            event: RuntimeEvent::StateChanged {
                status: TurnStatus::Interrupted,
            },
        };
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![turn_id.to_string(),to_i64(seq)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE turns SET state_json=?1,status='interrupted',revision=?2,last_seq=?3,updated_at_ms=?4 WHERE turn_id=?5",params![state_json,to_i64(state.revision)?,to_i64(seq)?,to_i64(now_ms)?,turn_id.to_string()])?;
        tx.execute(
            "UPDATE turn_read_model SET state_json=?1,revision=?2,last_seq=?3 WHERE turn_id=?4",
            params![
                serde_json::to_string(&state).map_err(invalid_json)?,
                to_i64(state.revision)?,
                to_i64(seq)?,
                turn_id.to_string()
            ],
        )?;
        tx.execute(
            "UPDATE effects SET status='unknown' WHERE turn_id=?1 AND status='started'",
            [turn_id.to_string()],
        )?;
        tx.commit()?;
        Ok(LeaseLossRecovery::Interrupted(state))
    }
    #[cfg(test)]
    pub(crate) fn reconcile_unknown_and_abort(
        &self,
        turn_id: TurnId,
        effect_id: &str,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<TurnState, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
        if !authoritative {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut state: TurnState = serde_json::from_str(&json).map_err(invalid_json)?;
        if state.revision != expected_revision || from_i64(token)? != lease.fencing_token {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: state.revision,
            });
        }
        let changed=tx.execute("UPDATE effects SET status='observed_failed',post_evidence_json='{\"reconciliation\":\"acknowledged_failed\"}',observed_at_ms=?1 WHERE effect_id=?2 AND turn_id=?3 AND status='unknown'",params![to_i64(now_ms)?,effect_id,turn_id.to_string()])?;
        if changed != 1 {
            return Err(StorageError::InvalidData(
                "unknown effect does not belong to run".into(),
            ));
        }
        state.status = TurnStatus::Failed;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
        state.failure = Some(TurnFailure {
            code: FailureCode::RuntimeFailed,
            message: format!("unknown effect {effect_id} acknowledged failed; turn aborted"),
            retryability: Retryability::Terminal,
        });
        state.pending_permission = None;
        state.pending_input = None;
        let seq = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("sequence overflow".into()))?;
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            turn_id,
            revision: state.revision,
            event: RuntimeEvent::StateChanged {
                status: TurnStatus::Failed,
            },
        };
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![turn_id.to_string(),to_i64(seq)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE turns SET state_json=?1,status='failed',revision=?2,last_seq=?3,updated_at_ms=?4 WHERE turn_id=?5",params![state_json,to_i64(state.revision)?,to_i64(seq)?,to_i64(now_ms)?,turn_id.to_string()])?;
        tx.execute(
            "UPDATE turn_read_model SET state_json=?1,revision=?2,last_seq=?3 WHERE turn_id=?4",
            params![
                serde_json::to_string(&state).map_err(invalid_json)?,
                to_i64(state.revision)?,
                to_i64(seq)?,
                turn_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(state)
    }

    pub(crate) fn record_verification_evidence(
        &self,
        turn_id: TurnId,
        expected_revision: u64,
        lease: &Lease,
        evidence: &VerificationEvidence<'_>,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        serde_json::from_str::<serde_json::Value>(evidence.metadata_json).map_err(invalid_json)?;
        let record = serde_json::from_str::<VerificationRecord>(evidence.metadata_json)
            .map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let linked_session: Option<String> = tx
            .query_row(
                "SELECT session_id FROM session_turns WHERE turn_id=?1",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let expected_scope = linked_session
            .as_deref()
            .map(parse_session_id)
            .transpose()?
            .map_or_else(
                || LEGACY_RUNTIME_LEASE_SCOPE.to_owned(),
                session_lease_scope,
            );
        require_lease_scope(lease, &expected_scope)?;
        let changed = tx.execute(
            "INSERT INTO evidence(id,turn_id,metadata_json,blob_ref) \
             SELECT ?1,?2,?3,?4 \
             WHERE EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?5 AND owner=?6 AND fencing_token=?7 AND expires_at_ms>?8) \
             AND EXISTS(SELECT 1 FROM turns WHERE turn_id=?2 AND revision=?9 AND effect_epoch=?10 AND lease_token=?7) \
             AND ((?11 IS NULL AND NOT EXISTS(SELECT 1 FROM session_turns WHERE turn_id=?2)) \
                  OR EXISTS(SELECT 1 FROM session_turns tr \
                            JOIN session_active_turns ar ON ar.session_id=tr.session_id AND ar.turn_id=tr.turn_id \
                            WHERE tr.turn_id=?2 AND tr.session_id=?11 AND ar.lease_token=?7))",
            params![
                evidence.id,
                turn_id.to_string(),
                evidence.metadata_json,
                evidence.blob_ref,
                expected_scope,
                lease.owner,
                to_i64(lease.fencing_token)?,
                to_i64(now_ms)?,
                to_i64(expected_revision)?,
                to_i64(record.effect_epoch)?,
                linked_session,
            ],
        )?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn effect_epoch(&self, turn_id: TurnId) -> Result<u64, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let value: i64 = conn.query_row(
            "SELECT effect_epoch FROM turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |row| row.get(0),
        )?;
        from_i64(value)
    }
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn complete_verified(
        &self,
        turn_id: TurnId,
        expected_revision: u64,
        lease: &Lease,
        summary: String,
        current_manifest: &std::collections::BTreeMap<String, String>,
        manifest_digest: &str,
        now_ms: u64,
    ) -> Result<(TurnState, StoredEvent), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",
            params![lease.scope, lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
            |row| row.get(0),
        )?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token, epoch): (String, i64, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token,effect_epoch FROM turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let current: TurnState = serde_json::from_str(&json).map_err(invalid_json)?;
        if current.revision != expected_revision {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        if from_i64(token)? > lease.fencing_token {
            return Err(StorageError::LeaseLost);
        }
        let epoch = from_i64(epoch)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT metadata_json FROM evidence WHERE turn_id=?1 ORDER BY rowid DESC LIMIT 1",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let record = raw
            .map(|raw| serde_json::from_str::<VerificationRecord>(&raw).map_err(invalid_json))
            .transpose()?
            .filter(|record| record.revision == expected_revision && record.effect_epoch == epoch)
            .ok_or_else(|| {
                StorageError::InvalidData("missing current verification evidence".into())
            })?;
        if !record.passed {
            return Err(StorageError::InvalidData("verification failed".into()));
        }
        if record.workspace_manifest_digest != manifest_digest {
            return Err(StorageError::InvalidData(format!(
                "workspace changed after verification: expected {}, actual {}",
                record.workspace_manifest_digest, manifest_digest
            )));
        }
        let baseline_json: Option<String> = tx
            .query_row(
                "SELECT manifest_json FROM turn_baselines WHERE turn_id=?1",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let baseline: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&baseline_json.ok_or_else(|| {
                StorageError::InvalidData("missing engine-owned run baseline".into())
            })?)
            .map_err(invalid_json)?;
        let mut changed = std::collections::BTreeSet::<String>::new();
        for key in baseline.keys().chain(current_manifest.keys()) {
            if baseline.get(key) != current_manifest.get(key) {
                changed.insert(key.clone());
            }
        }
        let mut displayed = std::collections::BTreeMap::<String, String>::new();
        for encoded in changed {
            let components: Vec<String> = serde_json::from_str(&encoded).map_err(invalid_json)?;
            if components.is_empty()
                || components.iter().any(|component| {
                    component.is_empty()
                        || component.contains('/')
                        || component
                            .chars()
                            .any(|value| value == '\0' || value.is_control())
                })
            {
                return Err(StorageError::InvalidData(
                    "invalid manifest component key".into(),
                ));
            }
            let display = components.join("/");
            if displayed.insert(display, encoded).is_some() {
                return Err(StorageError::InvalidData(
                    "manifest display path collision".into(),
                ));
            }
        }
        let handoff = Handoff {
            summary,
            files_changed: displayed.into_keys().collect(),
            evidence: vec![Evidence {
                name: format!("verification: {}", record.effect_id),
                status: VerificationStatus::Passed,
                summary: format!(
                    "{}; verified_manifest_sha256={manifest_digest}; verified_at_ms={now_ms}; change_source=manifest_v1",
                    record.summary
                ),
            }],
        };
        let next = current
            .transition(
                expected_revision,
                Transition::Complete {
                    handoff: handoff.clone(),
                    policy: CompletionPolicy::VerificationRequired,
                },
            )
            .map_err(|error| StorageError::InvalidData(error.to_string()))?;
        let sequence = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("sequence overflow".into()))?;
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            turn_id,
            revision: next.revision,
            event: RuntimeEvent::HandoffProduced { handoff },
        };
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![turn_id.to_string(),to_i64(sequence)?,envelope.event_id.to_string(),to_i64(next.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let state_json = serde_json::to_string(&next).map_err(invalid_json)?;
        let changed = tx.execute("UPDATE turns SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE turn_id=?7 AND revision=?8 AND effect_epoch=?9", params![state_json,status_name(next.status),to_i64(next.revision)?,to_i64(sequence)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,turn_id.to_string(),to_i64(expected_revision)?,to_i64(epoch)?])?;
        if changed != 1 {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        tx.execute(
            "UPDATE turn_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE turn_id=?4",
            params![
                to_i64(next.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(&next).map_err(invalid_json)?,
                turn_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok((next, StoredEvent { sequence, envelope }))
    }
    pub(crate) fn put_checkpoint(
        &self,
        turn_id: TurnId,
        expected_revision: u64,
        lease: &Lease,
        payload: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(lease)?;
        serde_json::from_str::<serde_json::Value>(payload).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("INSERT INTO runtime_checkpoints(turn_id,payload_json,updated_at_ms) SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?4 AND owner=?5 AND fencing_token=?6 AND expires_at_ms>?3) AND EXISTS(SELECT 1 FROM turns WHERE turn_id=?1 AND revision=?7 AND lease_token=?6) ON CONFLICT(turn_id) DO UPDATE SET payload_json=excluded.payload_json,updated_at_ms=excluded.updated_at_ms",params![turn_id.to_string(),payload,to_i64(now_ms)?,lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn checkpoint(&self, turn_id: TurnId) -> Result<Option<String>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT payload_json FROM runtime_checkpoints WHERE turn_id=?1",
                [turn_id.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }
    #[allow(clippy::too_many_lines)]
    pub(crate) fn cancel_waiting(
        &self,
        turn_id: TurnId,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
        denied: bool,
    ) -> Result<(TurnState, Option<StoredEvent>), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|row|row.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM turns WHERE turn_id=?1",
            [turn_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut state: TurnState = serde_json::from_str(&json).map_err(invalid_json)?;
        if state.revision != expected_revision {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: state.revision,
            });
        }
        if from_i64(token)? > lease.fencing_token {
            return Err(StorageError::LeaseLost);
        }
        if matches!(
            state.status,
            TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
        ) {
            tx.commit()?;
            return Ok((state, None));
        }
        if denied && state.status != TurnStatus::WaitingPermission {
            return Err(StorageError::InvalidData(
                "turn is not waiting for permission".into(),
            ));
        }
        if !matches!(
            state.status,
            TurnStatus::WaitingPermission | TurnStatus::WaitingInput
        ) {
            return Err(StorageError::InvalidData("turn is not waiting".into()));
        }
        if let Some(permission) = state.pending_permission.as_ref() {
            let removed=tx.execute("DELETE FROM pending_permissions WHERE effect_id=?1 AND turn_id=?2 AND approval_digest=?3 AND consumed_at_ms IS NULL",params![permission.request_id,turn_id.to_string(),permission.operation_digest])?;
            if removed != 1 {
                return Err(StorageError::InvalidData(
                    "waiting permission binding is not prepared".into(),
                ));
            }
            let evidence = if denied {
                "{\"denied\":true}"
            } else {
                "{\"cancelled\":true}"
            };
            let marked=tx.execute("UPDATE effects SET status='observed_failed',post_evidence_json=?1,observed_at_ms=?2 WHERE effect_id=?3 AND turn_id=?4 AND status='prepared'",params![evidence,to_i64(now_ms)?,permission.request_id,turn_id.to_string()])?;
            if marked != 1 {
                return Err(StorageError::InvalidData(
                    "prepared effect cancellation failed".into(),
                ));
            }
        }
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
        state.status = TurnStatus::Failed;
        state.pending_permission = None;
        state.pending_input = None;
        state.failure = Some(TurnFailure {
            code: if denied {
                FailureCode::PermissionDenied
            } else {
                FailureCode::Cancelled
            },
            message: if denied {
                "permission denied".into()
            } else {
                "run cancelled while waiting".into()
            },
            retryability: Retryability::Terminal,
        });
        let sequence = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("sequence overflow".into()))?;
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            turn_id,
            revision: state.revision,
            event: RuntimeEvent::StateChanged {
                status: TurnStatus::Failed,
            },
        };
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![turn_id.to_string(),to_i64(sequence)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE turns SET state_json=?1,status='failed',revision=?2,last_seq=?3,lease_token=?4,updated_at_ms=?5 WHERE turn_id=?6 AND revision=?7",params![state_json,to_i64(state.revision)?,to_i64(sequence)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,turn_id.to_string(),to_i64(expected_revision)?])?;
        tx.execute(
            "UPDATE turn_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE turn_id=?4",
            params![
                to_i64(state.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(&state).map_err(invalid_json)?,
                turn_id.to_string()
            ],
        )?;
        tx.execute(
            "DELETE FROM runtime_checkpoints WHERE turn_id=?1",
            [turn_id.to_string()],
        )?;
        tx.commit()?;
        Ok((state, Some(StoredEvent { sequence, envelope })))
    }

    /// Recovers all expired leases and returns the committed session responses
    /// for every recovered linked child, so the engine can broadcast their
    /// durable events and wake connected SSE clients.
    pub(crate) fn recover_at(
        &self,
        now_ms: u64,
    ) -> Result<Vec<SessionCommitResponse>, StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Linked children must recover their v1 run, effect ledger, session
        // projection, transcript, and session event together.  In particular
        // never let the legacy loop interrupt a v2 child by itself: doing so
        // leaves an active-row/lifecycle pair that no v2 command can safely
        // reconcile.
        let mut recovered = Vec::new();
        let linked_rows = {
            let mut stmt = tx.prepare(
                "SELECT ar.session_id,ar.turn_id,r.lease_token FROM session_active_turns ar \
                 JOIN turns r ON r.turn_id=ar.turn_id \
                 WHERE r.status IN ('queued','running','cancelling','waiting_permission','waiting_input') \
                 AND r.lease_token<>0 \
                 AND NOT EXISTS(SELECT 1 FROM runtime_lease l WHERE l.scope='session:'||ar.session_id AND l.fencing_token=r.lease_token AND l.expires_at_ms>?1)",
            )?;
            stmt.query_map([to_i64(now_ms)?], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for (session, run, token) in linked_rows {
            let session_id = parse_session_id(&session)?;
            let turn_id = uuid::Uuid::parse_str(&run)
                .map(TurnId::from_uuid)
                .map_err(|error| {
                    StorageError::InvalidData(format!("invalid stored run id: {error}"))
                })?;
            if let Some(response) = recover_linked_session_turn(
                &tx,
                session_id,
                turn_id,
                from_i64(token)?,
                None,
                now_ms,
            )? {
                recovered.push(response);
            }
        }
        let mut stmt = tx.prepare(
            "SELECT r.turn_id,r.state_json,r.last_seq FROM turns r WHERE r.status IN ('running','cancelling') AND NOT EXISTS(SELECT 1 FROM session_turns tr WHERE tr.turn_id=r.turn_id) AND NOT EXISTS(SELECT 1 FROM runtime_lease l WHERE l.scope='runtime' AND l.fencing_token=r.lease_token AND l.expires_at_ms>?1)",
        )?;
        let rows = stmt
            .query_map([to_i64(now_ms)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (id, json, last_seq) in rows {
            let mut state: TurnState = serde_json::from_str(&json).map_err(invalid_json)?;
            state.status = TurnStatus::Interrupted;
            state.revision = state.revision.checked_add(1).ok_or_else(|| {
                StorageError::InvalidData("revision overflow during recovery".into())
            })?;
            let json = serde_json::to_string(&state).map_err(invalid_json)?;
            let sequence = from_i64(last_seq)?.checked_add(1).ok_or_else(|| {
                StorageError::InvalidData("event sequence overflow during recovery".into())
            })?;
            let envelope = EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                event_id: EventId::from_uuid(Uuid::now_v7()),
                turn_id: state.turn_id,
                revision: state.revision,
                event: RuntimeEvent::StateChanged {
                    status: TurnStatus::Interrupted,
                },
            };
            tx.execute(
                "INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, to_i64(sequence)?, envelope.event_id.to_string(), to_i64(state.revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?],
            )?;
            tx.execute(
                "UPDATE turns SET state_json=?1,status='interrupted',revision=?2,last_seq=?3 WHERE turn_id=?4",
                params![json, to_i64(state.revision)?, to_i64(sequence)?, id],
            )?;
            tx.execute(
                "UPDATE turn_read_model SET state_json=?1,revision=?2,last_seq=?3 WHERE turn_id=?4",
                params![
                    serde_json::to_string(&state).map_err(invalid_json)?,
                    to_i64(state.revision)?,
                    to_i64(sequence)?,
                    id
                ],
            )?;
            tx.execute(
                "UPDATE effects SET status='unknown' WHERE turn_id=?1 AND status='started'",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(recovered)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn invalid_json(error: serde_json::Error) -> StorageError {
    StorageError::InvalidData(error.to_string())
}
fn to_i64(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value)
        .map_err(|_| StorageError::InvalidData("integer exceeds sqlite range".into()))
}
fn from_i64(value: i64) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::InvalidData("negative sqlite integer".into()))
}

/// The prefix of every session-list cursor. Cursors are opaque to clients;
/// the version prefix lets the keyset encoding evolve without ambiguity.
const SESSION_CURSOR_PREFIX: &str = "v1_";

/// Encodes a `(updated_at_ms, rowid)` keyset position as an opaque cursor.
fn encode_session_cursor(updated_at_ms: i64, rowid: i64) -> String {
    // Timestamps and rowids are non-negative by schema; the cursor only ever
    // encodes values read back from `sessions`.
    format!(
        "{SESSION_CURSOR_PREFIX}{:x}:{:x}",
        u64::try_from(updated_at_ms).expect("non-negative timestamp"),
        u64::try_from(rowid).expect("non-negative rowid")
    )
}

/// Decodes an opaque cursor back into its `(updated_at_ms, rowid)` keyset
/// position, failing closed on any malformed input.
fn decode_session_cursor(cursor: &str) -> Result<(i64, i64), StorageError> {
    let invalid = || StorageError::InvalidData("invalid session cursor".into());
    let body = cursor
        .strip_prefix(SESSION_CURSOR_PREFIX)
        .ok_or_else(invalid)?;
    let (updated, rowid) = body.split_once(':').ok_or_else(invalid)?;
    let parse = |hex: &str| -> Result<i64, StorageError> {
        let unsigned = u64::from_str_radix(hex, 16).map_err(|_| invalid())?;
        i64::try_from(unsigned).map_err(|_| invalid())
    };
    Ok((parse(updated)?, parse(rowid)?))
}

/// Maps a `sessions` catalog row (the eight summary columns followed by
/// `updated_at_ms` and `rowid`) to a [`SessionSummary`] plus the sort
/// keys needed for cursor encoding.
fn session_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(SessionSummary, i64, i64)> {
    let session_id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let workspace_root: String = row.get(2)?;
    let parent: Option<String> = row.get(3)?;
    let lifecycle: String = row.get(4)?;
    let binding_json: String = row.get(5)?;
    let created: i64 = row.get(6)?;
    let updated: i64 = row.get(7)?;
    let rowid: i64 = row.get(8)?;
    let binding: SessionProviderBinding = serde_json::from_str(&binding_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let summary = SessionSummary {
        session_id: parse_session_id(&session_id).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error.into())
        })?,
        title,
        workspace_root,
        parent_session_id: parent
            .as_deref()
            .map(parse_session_id)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    error.into(),
                )
            })?,
        lifecycle: parse_lifecycle(&lifecycle).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, error.into())
        })?,
        provider_name: binding.provider_name,
        model: binding.model,
        created_at_ms: from_i64(created).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                error.into(),
            )
        })?,
        updated_at_ms: from_i64(updated).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                error.into(),
            )
        })?,
    };
    Ok((summary, updated, rowid))
}

fn status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Queued => "queued",
        TurnStatus::Running => "running",
        TurnStatus::WaitingPermission => "waiting_permission",
        TurnStatus::WaitingInput => "waiting_input",
        TurnStatus::Cancelling => "cancelling",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
        TurnStatus::Completed => "completed",
    }
}

fn parse_session_id(value: &str) -> Result<latte_core::SessionId, StorageError> {
    uuid::Uuid::parse_str(value)
        .map(latte_core::SessionId::from_uuid)
        .map_err(|error| StorageError::InvalidData(format!("invalid stored session id: {error}")))
}

fn parse_turn_id(value: &str) -> Result<TurnId, StorageError> {
    uuid::Uuid::parse_str(value)
        .map(TurnId::from_uuid)
        .map_err(|error| StorageError::InvalidData(format!("invalid stored run id: {error}")))
}

fn session_lease_scope(session_id: latte_core::SessionId) -> String {
    format!("session:{session_id}")
}

fn require_lease_scope(lease: &Lease, expected: &str) -> Result<(), StorageError> {
    if lease.scope == expected {
        Ok(())
    } else {
        Err(StorageError::LeaseLost)
    }
}

fn require_legacy_runtime_lease(lease: &Lease) -> Result<(), StorageError> {
    require_lease_scope(lease, LEGACY_RUNTIME_LEASE_SCOPE)
}

fn validate_workspace_root(value: &str) -> Result<&str, StorageError> {
    if value.is_empty() || value.len() > 4 * 1024 || value.chars().any(char::is_control) {
        return Err(StorageError::InvalidData(
            "invalid canonical workspace root".into(),
        ));
    }
    Ok(value)
}

fn validate_catalog_key(value: &str, name: &str) -> Result<(), StorageError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
    {
        return Err(StorageError::InvalidData(format!("invalid {name}")));
    }
    Ok(())
}

fn session_title(prompt: &str) -> String {
    const LIMIT: usize = 120;
    // Redact before the length cap: truncating first could split a secret
    // across the boundary and leave a partial credential in the visible
    // title. Redaction here covers every title source — create prompts are
    // redacted by the caller too (harmless, redaction is idempotent), but
    // rename and fork titles are user-supplied raw and reach the durable
    // record through this one function.
    let prompt = redact_session_text(prompt);
    let first_line = prompt.lines().next().unwrap_or_default().trim();
    let mut title = String::with_capacity(first_line.len().min(LIMIT));
    for value in first_line.chars().filter(|value| !value.is_control()) {
        if title.len() + value.len_utf8() > LIMIT {
            title.push('…');
            break;
        }
        title.push(value);
    }
    if title.is_empty() {
        "Untitled session".into()
    } else {
        title
    }
}

fn parse_lifecycle(value: &str) -> Result<SessionLifecycle, StorageError> {
    match value {
        "ready" => Ok(SessionLifecycle::Ready),
        "running" => Ok(SessionLifecycle::Running),
        "waiting_permission" => Ok(SessionLifecycle::WaitingPermission),
        "waiting_input" => Ok(SessionLifecycle::WaitingInput),
        "interrupted" => Ok(SessionLifecycle::Interrupted),
        "failed" => Ok(SessionLifecycle::Failed),
        "reconciliation_required" => Ok(SessionLifecycle::ReconciliationRequired),
        _ => Err(StorageError::InvalidData(format!(
            "invalid session lifecycle {value}"
        ))),
    }
}

fn session_turn_status(status: TurnStatus) -> SessionTurnStatus {
    match status {
        TurnStatus::Queued => SessionTurnStatus::Queued,
        TurnStatus::Running => SessionTurnStatus::Running,
        TurnStatus::Cancelling => SessionTurnStatus::Cancelling,
        TurnStatus::WaitingPermission => SessionTurnStatus::WaitingPermission,
        TurnStatus::WaitingInput => SessionTurnStatus::WaitingInput,
        TurnStatus::Interrupted => SessionTurnStatus::Interrupted,
        TurnStatus::Failed => SessionTurnStatus::Failed,
        TurnStatus::Completed => SessionTurnStatus::Completed,
    }
}

fn transcript_kind_name(kind: TranscriptKind) -> &'static str {
    match kind {
        TranscriptKind::User => "user",
        TranscriptKind::Assistant => "assistant",
        TranscriptKind::ToolCall => "tool_call",
        TranscriptKind::ToolResult => "tool_result",
        TranscriptKind::Permission => "permission",
        TranscriptKind::Input => "input",
        TranscriptKind::Failure => "failure",
        TranscriptKind::Completion => "completion",
        TranscriptKind::System => "system",
        TranscriptKind::CompactSummary => "compact_summary",
    }
}

#[allow(clippy::too_many_lines)]
fn session_snapshot(
    connection: &Connection,
    session_id: latte_core::SessionId,
    after: Option<u64>,
    limit: usize,
) -> Result<SessionSnapshot, StorageError> {
    let (revision, sequence, lifecycle, binding_json, latest, focus): (i64, i64, String, String, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT revision,last_seq,lifecycle,binding_json,latest_turn_id,focus FROM sessions WHERE session_id=?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()?
        .ok_or(StorageError::SessionNotFound(session_id))?;
    let binding = serde_json::from_str(&binding_json).map_err(invalid_json)?;
    let latest_turn_id = latest
        .as_deref()
        .map(|value| uuid::Uuid::parse_str(value).map(TurnId::from_uuid))
        .transpose()
        .map_err(|error| StorageError::InvalidData(format!("invalid stored run id: {error}")))?;
    let active_turn_id: Option<String> = connection
        .query_row(
            "SELECT turn_id FROM session_active_turns WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let active_turn_id = active_turn_id
        .as_deref()
        .map(|value| uuid::Uuid::parse_str(value).map(TurnId::from_uuid))
        .transpose()
        .map_err(|error| StorageError::InvalidData(format!("invalid active turn id: {error}")))?;
    let mut statement = connection.prepare(
        "SELECT tr.turn_id,tr.parent_turn_id,tr.ordinal,tr.completed_at_ms,r.state_json
         FROM session_turns tr JOIN turns r ON r.turn_id=tr.turn_id
         WHERE tr.session_id=?1 ORDER BY tr.ordinal ASC",
    )?;
    let runs = statement
        .query_map([session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .map(|row| {
            let (run, parent, ordinal, completed, state_json) = row?;
            let state: TurnState = serde_json::from_str(&state_json).map_err(invalid_json)?;
            let turn_id = uuid::Uuid::parse_str(&run)
                .map(TurnId::from_uuid)
                .map_err(|error| {
                    StorageError::InvalidData(format!("invalid stored run id: {error}"))
                })?;
            let parent_turn_id = parent
                .as_deref()
                .map(|value| uuid::Uuid::parse_str(value).map(TurnId::from_uuid))
                .transpose()
                .map_err(|error| {
                    StorageError::InvalidData(format!("invalid parent run id: {error}"))
                })?;
            Ok(SessionTurnSummary {
                turn_id,
                parent_turn_id,
                ordinal: from_i64(ordinal)?,
                status: session_turn_status(state.status),
                turn_revision: state.revision,
                completed_at_ms: completed.map(from_i64).transpose()?,
                failure_code: state.failure.map(|failure| failure.code),
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    let bounded_limit = limit.clamp(1, 500);
    let query_after = after.unwrap_or(0);
    let mut entries = connection
        .prepare(
            "SELECT entry_json FROM conversation_outbox WHERE session_id=?1 AND seq>?2 ORDER BY seq ASC LIMIT ?3",
        )?
        .query_map(
            params![session_id.to_string(), to_i64(query_after)?, i64::try_from(bounded_limit + 1).map_err(|_| StorageError::InvalidData("page limit overflow".into()))?],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| row.map_err(StorageError::from).and_then(|json| serde_json::from_str(&json).map_err(invalid_json)))
        .collect::<Result<Vec<TranscriptEntry>, StorageError>>()?;
    let has_more = entries.len() > bounded_limit;
    entries.truncate(bounded_limit);
    let next_after = entries.last().map(|entry| entry.sequence);
    let pending = active_turn_id.and_then(|active| {
        runs.iter()
            .find(|run| run.turn_id == active)
            .and_then(|summary| {
                let state_json: Option<String> = connection
                    .query_row(
                        "SELECT state_json FROM turns WHERE turn_id=?1",
                        [active.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let state =
                    state_json.and_then(|json| serde_json::from_str::<TurnState>(&json).ok())?;
                if let Some(permission) = state.pending_permission {
                    Some(SessionPendingRequest::Permission {
                        turn_id: active,
                        request_id: redact_session_text(&permission.request_id),
                        description: redact_session_text(&permission.description),
                        expected_turn_revision: summary.turn_revision,
                    })
                } else {
                    state
                        .pending_input
                        .map(|input| SessionPendingRequest::Input {
                            turn_id: active,
                            request_id: redact_session_text(&input.request_id),
                            prompt: redact_session_text(&input.prompt),
                            expected_turn_revision: summary.turn_revision,
                        })
                }
            })
    });
    Ok(SessionSnapshot {
        session_id,
        revision: from_i64(revision)?,
        sequence: from_i64(sequence)?,
        lifecycle: parse_lifecycle(&lifecycle)?,
        binding,
        latest_turn_id,
        active_turn_id,
        pending,
        turns: runs,
        transcript: TranscriptPage {
            entries,
            next_after,
            has_more,
        },
        focus,
    })
}

fn current_session_snapshot(
    connection: &Connection,
    session_id: latte_core::SessionId,
    transcript_limit: usize,
) -> Result<SessionSnapshot, StorageError> {
    let mut snapshot = session_snapshot(connection, session_id, None, 1)?;
    snapshot.transcript = session_transcript_tail(connection, session_id, transcript_limit)?;
    Ok(snapshot)
}

/// Loads the newest bounded transcript page for a presentation projection.
///
/// `has_more` here means that older cards were deliberately omitted. The
/// normal forward `session_snapshot` paging API remains unchanged for durable
/// history/restart reconstruction. Presentation consumers must render an
/// explicit truncation notice whenever this returns `has_more`.
fn session_transcript_tail(
    connection: &Connection,
    session_id: latte_core::SessionId,
    limit: usize,
) -> Result<TranscriptPage, StorageError> {
    let bounded_limit = limit.clamp(1, SESSION_PROJECTION_TRANSCRIPT_LIMIT);
    let mut newest_first = connection
        .prepare(
            "SELECT entry_json FROM conversation_outbox WHERE session_id=?1 ORDER BY seq DESC LIMIT ?2",
        )?
        .query_map(
            params![
                session_id.to_string(),
                i64::try_from(bounded_limit + 1)
                    .map_err(|_| StorageError::InvalidData("page limit overflow".into()))?
            ],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| {
            row.map_err(StorageError::from)
                .and_then(|json| serde_json::from_str(&json).map_err(invalid_json))
        })
        .collect::<Result<Vec<TranscriptEntry>, StorageError>>()?;
    let has_more = newest_first.len() > bounded_limit;
    newest_first.truncate(bounded_limit);
    newest_first.reverse();
    let next_after = newest_first.last().map(|entry| entry.sequence);
    Ok(TranscriptPage {
        entries: newest_first,
        next_after,
        has_more,
    })
}

fn append_linked_turn_transition(
    tx: &rusqlite::Transaction<'_>,
    current: &TurnState,
    next: &TurnState,
    turn_last_seq: u64,
    lease: &Lease,
    now_ms: u64,
) -> Result<(), StorageError> {
    if next.revision <= current.revision {
        return Err(StorageError::InvalidData(
            "linked turn transition did not advance".into(),
        ));
    }
    // Cancellation is intentionally represented as the two v1 transitions so
    // a v1 reader never observes a revision jump without its cancelling event.
    let mut states = Vec::new();
    if next.revision == current.revision + 2 && next.status == TurnStatus::Interrupted {
        states.push(
            current
                .transition(current.revision, Transition::Cancel)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?,
        );
    }
    states.push(next.clone());
    let mut last_seq = turn_last_seq;
    for state in states {
        last_seq = last_seq
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("run event sequence overflow".into()))?;
        let event = if let Some(handoff) = state.handoff.clone() {
            RuntimeEvent::HandoffProduced { handoff }
        } else {
            RuntimeEvent::StateChanged {
                status: state.status,
            }
        };
        let envelope = EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            event_id: EventId::from_uuid(Uuid::now_v7()),
            turn_id: state.turn_id,
            revision: state.revision,
            event,
        };
        tx.execute("INSERT INTO events(turn_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![state.turn_id.to_string(),to_i64(last_seq)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE turns SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE turn_id=?7 AND revision<?3",params![serde_json::to_string(&state).map_err(invalid_json)?,status_name(state.status),to_i64(state.revision)?,to_i64(last_seq)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,state.turn_id.to_string()])?;
        tx.execute(
            "UPDATE turn_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE turn_id=?4",
            params![
                to_i64(state.revision)?,
                to_i64(last_seq)?,
                serde_json::to_string(&state).map_err(invalid_json)?,
                state.turn_id.to_string()
            ],
        )?;
    }
    Ok(())
}

/// Performs the v2 half of stale-run recovery inside the same immediate
/// transaction as the v1 interruption.  `expected_lease_token` is a stale
/// fencing token, not authority to mutate: it is used only to prove that this
/// active row and run still belong to the caller/restart being recovered.
#[allow(clippy::too_many_lines)]
fn recover_linked_session_turn(
    tx: &rusqlite::Transaction<'_>,
    session_id: latte_core::SessionId,
    turn_id: TurnId,
    expected_lease_token: u64,
    expected_turn_revision: Option<u64>,
    now_ms: u64,
) -> Result<Option<SessionCommitResponse>, StorageError> {
    let active: Option<(String, i64)> = tx
        .query_row(
            "SELECT turn_id,lease_token FROM session_active_turns WHERE session_id=?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((active_turn_id, active_token)) = active else {
        return Ok(None);
    };
    if active_turn_id != turn_id.to_string() || from_i64(active_token)? != expected_lease_token {
        return Ok(None);
    }
    let (state_json, turn_last_seq, turn_token): (String, i64, i64) = tx.query_row(
        "SELECT state_json,last_seq,lease_token FROM turns WHERE turn_id=?1",
        [turn_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if from_i64(turn_token)? != expected_lease_token {
        return Ok(None);
    }
    let current: TurnState = serde_json::from_str(&state_json).map_err(invalid_json)?;
    if expected_turn_revision.is_some_and(|expected| expected != current.revision)
        || matches!(
            current.status,
            TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
        )
    {
        return Ok(None);
    }
    let (session_revision, session_last_seq): (i64, i64) = tx
        .query_row(
            "SELECT revision,last_seq FROM sessions WHERE session_id=?1",
            [session_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(StorageError::SessionNotFound(session_id))?;

    let started_effect_ids = {
        let mut statement = tx.prepare(
            "SELECT effect_id FROM effects WHERE turn_id=?1 AND status='started' ORDER BY rowid ASC",
        )?;
        statement
            .query_map([turn_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let prepared_effect_ids = {
        let mut statement = tx.prepare(
            "SELECT effect_id FROM effects WHERE turn_id=?1 AND status='prepared' ORDER BY rowid ASC",
        )?;
        statement
            .query_map([turn_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };

    // A Prepared record has not crossed the external-effect boundary.  It is
    // terminalized as a non-execution, never labelled Unknown.  Started is
    // the sole proof that an external outcome may exist, so every such effect
    // becomes Unknown before the active child is cleared.
    tx.execute(
        "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE turn_id=?2 AND consumed_at_ms IS NULL",
        params![to_i64(now_ms)?, turn_id.to_string()],
    )?;
    tx.execute(
        r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"recovery":"not_started"}',observed_at_ms=?1 WHERE turn_id=?2 AND status='prepared'"#,
        params![to_i64(now_ms)?, turn_id.to_string()],
    )?;
    tx.execute(
        r#"UPDATE effects SET status='unknown',post_evidence_json='{"outcome":"lease_lost_after_start"}',observed_at_ms=?1 WHERE turn_id=?2 AND status='started'"#,
        params![to_i64(now_ms)?, turn_id.to_string()],
    )?;

    // Keep the checkpoint intact: it is the only restart evidence we may
    // retain without asserting an effect outcome.  The v1 interruption is
    // deliberately one event/revision, matching the legacy recovery path.
    let mut interrupted = current.clone();
    interrupted.status = TurnStatus::Interrupted;
    interrupted.revision = current
        .revision
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidData("revision overflow during recovery".into()))?;
    interrupted.pending_input = None;
    interrupted.pending_permission = None;
    let stale_fence = Lease {
        scope: session_lease_scope(session_id),
        owner: "recovery".into(),
        fencing_token: expected_lease_token,
        expires_at_ms: 0,
    };
    append_linked_turn_transition(
        tx,
        &current,
        &interrupted,
        from_i64(turn_last_seq)?,
        &stale_fence,
        now_ms,
    )?;
    tx.execute(
        "UPDATE session_turns SET completed_at_ms=?1 WHERE session_id=?2 AND turn_id=?3",
        params![to_i64(now_ms)?, session_id.to_string(), turn_id.to_string()],
    )?;
    tx.execute(
        "DELETE FROM session_active_turns WHERE session_id=?1 AND turn_id=?2 AND lease_token=?3",
        params![
            session_id.to_string(),
            turn_id.to_string(),
            to_i64(expected_lease_token)?
        ],
    )?;

    let next_session_revision = from_i64(session_revision)?.checked_add(1).ok_or_else(|| {
        StorageError::InvalidData("session revision overflow during recovery".into())
    })?;
    let mut next_sequence = from_i64(session_last_seq)?;
    let lifecycle = if started_effect_ids.is_empty() {
        "interrupted"
    } else {
        "reconciliation_required"
    };

    let mut append_card = |kind: TranscriptKind,
                           text: String,
                           payload: Option<serde_json::Value>,
                           source_key: String|
     -> Result<(), StorageError> {
        next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
            StorageError::InvalidData("session sequence overflow during recovery".into())
        })?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: next_sequence,
            turn_id: Some(turn_id),
            kind,
            text: redact_session_text(&text),
            payload: payload.map(redact_session_value),
            source_key,
            created_at_ms: now_ms,
        };
        tx.execute("INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![session_id.to_string(),to_i64(next_sequence)?,entry.entry_id.to_string(),turn_id.to_string(),transcript_kind_name(entry.kind),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let envelope = SessionEventEnvelope {
            protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
            event_id: SessionEventId::from_uuid(Uuid::now_v7()),
            session_id,
            revision: next_session_revision,
            sequence: next_sequence,
            event: SessionEvent::TranscriptAppended { entry },
        };
        tx.execute("INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![session_id.to_string(),to_i64(next_sequence)?,envelope.event_id.to_string(),to_i64(next_session_revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        Ok(())
    };
    append_card(
        TranscriptKind::System,
        "lease authority lost; linked turn interrupted".into(),
        Some(serde_json::json!({"recovery":"lease_lost"})),
        format!("recovery:{turn_id}:{}:interrupted", current.revision),
    )?;
    for (ordinal, effect_id) in started_effect_ids.iter().enumerate() {
        append_card(
            TranscriptKind::Failure,
            "effect outcome unknown; reconciliation required".into(),
            Some(serde_json::json!({
                "effect_id": redact_session_text(effect_id),
                "status":"unknown"
            })),
            format!("recovery:{turn_id}:{}:unknown:{ordinal}", current.revision),
        )?;
    }
    for (ordinal, effect_id) in prepared_effect_ids.iter().enumerate() {
        append_card(
            TranscriptKind::Failure,
            "prepared effect terminalized before external execution".into(),
            Some(serde_json::json!({
                "effect_id": redact_session_text(effect_id),
                "status":"not_started"
            })),
            format!("recovery:{turn_id}:{}:prepared:{ordinal}", current.revision),
        )?;
    }
    next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
        StorageError::InvalidData("session sequence overflow during recovery".into())
    })?;
    let final_event = if let Some(effect_id) = started_effect_ids.first() {
        SessionEvent::ReconciliationRequired {
            turn_id,
            effect_id: redact_session_text(effect_id),
        }
    } else {
        SessionEvent::LifecycleChanged {
            lifecycle: SessionLifecycle::Interrupted,
            turn_id: Some(turn_id),
        }
    };
    let envelope = SessionEventEnvelope {
        protocol_version: latte_core::SESSION_PROTOCOL_VERSION,
        event_id: SessionEventId::from_uuid(Uuid::now_v7()),
        session_id,
        revision: next_session_revision,
        sequence: next_sequence,
        event: final_event,
    };
    tx.execute("INSERT INTO session_events(session_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![session_id.to_string(),to_i64(next_sequence)?,envelope.event_id.to_string(),to_i64(next_session_revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
    tx.execute(
        "UPDATE sessions SET revision=?1,last_seq=?2,lifecycle=?3,latest_turn_id=?4,updated_at_ms=?5 WHERE session_id=?6",
        params![
            to_i64(next_session_revision)?,
            to_i64(next_sequence)?,
            lifecycle,
            turn_id.to_string(),
            to_i64(now_ms)?,
            session_id.to_string()
        ],
    )?;
    let snapshot = session_snapshot(tx, session_id, None, 100)?;
    Ok(Some(SessionCommitResponse {
        snapshot,
        session_event: StoredSessionEvent {
            sequence: next_sequence,
            envelope,
        },
    }))
}

fn redact_permission(value: &latte_core::PendingPermission) -> latte_core::PendingPermission {
    latte_core::PendingPermission {
        request_id: redact_session_text(&value.request_id),
        operation_digest: redact_session_text(&value.operation_digest),
        description: redact_session_text(&value.description),
    }
}
fn redact_input(value: &latte_core::PendingInput) -> latte_core::PendingInput {
    latte_core::PendingInput {
        request_id: redact_session_text(&value.request_id),
        prompt: redact_session_text(&value.prompt),
    }
}
fn redact_failure(value: &TurnFailure) -> TurnFailure {
    TurnFailure {
        code: value.code,
        message: redact_session_text(&value.message),
        retryability: value.retryability,
    }
}
fn redact_handoff(value: &Handoff) -> Handoff {
    Handoff {
        summary: redact_session_text(&value.summary),
        files_changed: value
            .files_changed
            .iter()
            .map(|path| redact_session_text(path))
            .collect(),
        evidence: value
            .evidence
            .iter()
            .map(|evidence| Evidence {
                name: redact_session_text(&evidence.name),
                status: evidence.status,
                summary: redact_session_text(&evidence.summary),
            })
            .collect(),
    }
}

fn validate_session_source(source: &str) -> Result<(), StorageError> {
    if source.is_empty() || source.len() > 256 || source.chars().any(char::is_control) {
        return Err(StorageError::InvalidData(
            "invalid session source key".into(),
        ));
    }
    Ok(())
}

/// Computes the durable digest that binds a session-create command to its
/// complete identity: operation kind, protocol version, workspace, session,
/// prompt, binding, and normalized focus. Two creates with the same
/// `command_id` but different digests fail with 422 `idempotency_mismatch`.
fn create_command_digest(
    session_id: latte_core::SessionId,
    workspace_root: &str,
    prompt: &str,
    binding: &SessionProviderBinding,
    focus: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::json!({
        "operation": "session.start",
        "protocol_version": latte_core::SESSION_PROTOCOL_VERSION,
        "workspace": workspace_root,
        "session_id": session_id.to_string(),
        "prompt": prompt,
        "binding": binding,
        "focus": focus.map(str::trim).filter(|value| !value.is_empty()),
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default())
    )
}

/// Stable digest of a follow-up command's complete identity. A same-command
/// retry must reproduce it byte-for-byte; any payload change fails with
/// 422 `idempotency_mismatch`.
fn follow_up_command_digest(
    session_id: latte_core::SessionId,
    expected_session_revision: u64,
    prompt: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::json!({
        "operation": "session.follow_up",
        "protocol_version": latte_core::SESSION_PROTOCOL_VERSION,
        "session_id": session_id.to_string(),
        "expected_session_revision": expected_session_revision,
        "prompt": prompt,
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default())
    )
}

/// Legacy digest reproducing the exact bytes a pre-rename (schema <13) binary
/// wrote for a session-create command. The binding serializes identically and
/// the protocol version value is unchanged, so only the operation namespace and
/// the id/revision key names differ. A durable accept made before the upgrade
/// stores this digest; on replay we accept either form so a retried request is
/// recognized as the same command instead of a false `idempotency_mismatch`.
///
/// Fixture-only: exposed so integration tests can reproduce a pre-rename
/// durable-accept digest byte-for-byte; production replay never calls it.
#[doc(hidden)]
#[must_use]
pub fn legacy_create_command_digest(
    session_id: latte_core::SessionId,
    workspace_root: &str,
    prompt: &str,
    binding: &SessionProviderBinding,
    focus: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::json!({
        "operation": "thread.start",
        "protocol_version": latte_core::SESSION_PROTOCOL_VERSION,
        "workspace": workspace_root,
        "thread_id": session_id.to_string(),
        "prompt": prompt,
        "binding": binding,
        "focus": focus.map(str::trim).filter(|value| !value.is_empty()),
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default())
    )
}

/// Legacy digest for a pre-rename follow-up command. See
/// [`legacy_create_command_digest`] for why both forms are accepted on replay.
/// Fixture-only; production replay never calls it directly.
#[doc(hidden)]
#[must_use]
pub fn legacy_follow_up_command_digest(
    session_id: latte_core::SessionId,
    expected_session_revision: u64,
    prompt: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::json!({
        "operation": "thread.follow_up",
        "protocol_version": latte_core::SESSION_PROTOCOL_VERSION,
        "thread_id": session_id.to_string(),
        "expected_thread_revision": expected_session_revision,
        "prompt": prompt,
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default())
    )
}

#[allow(clippy::too_many_lines)]
fn session_command_digest(request: &SessionCommitRequest) -> Result<String, StorageError> {
    use sha2::{Digest, Sha256};
    let update = match &request.update {
        CommitSessionTurnUpdate::Start { source_key } => {
            serde_json::json!({"kind":"start","source_key":redact_session_text(source_key)})
        }
        CommitSessionTurnUpdate::AppendTranscript {
            source_key,
            kind,
            text,
            payload,
        } => {
            serde_json::json!({"kind":"append_transcript","source_key":redact_session_text(source_key),"card":format!("{kind:?}"),"text":redact_session_text(text),"payload":payload.clone().map(redact_session_value)})
        }
        CommitSessionTurnUpdate::PrepareEffect {
            source_key,
            effect_id,
            operation_digest,
            descriptor_json,
            canonical_descriptor_json: _,
            policy,
            description,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"prepare_effect","source_key":redact_session_text(source_key),"effect_id":redact_session_text(effect_id),"operation_digest":redact_session_text(operation_digest),"descriptor":serde_json::from_str::<serde_json::Value>(descriptor_json).ok().map(redact_session_value),"policy":match policy { SessionEffectPolicy::Allow=>"allow", SessionEffectPolicy::Ask=>"ask"},"description":redact_session_text(description),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_session_value)})
        }
        CommitSessionTurnUpdate::StartEffect {
            source_key,
            effect_id,
            operation_digest,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"start_effect","source_key":redact_session_text(source_key),"effect_id":redact_session_text(effect_id),"operation_digest":redact_session_text(operation_digest),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_session_value)})
        }
        CommitSessionTurnUpdate::ObserveEffect {
            source_key,
            effect_id,
            operation_digest,
            success,
            result,
            payload,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"observe_effect","source_key":redact_session_text(source_key),"effect_id":redact_session_text(effect_id),"operation_digest":redact_session_text(operation_digest),"success":success,"result":redact_session_text(result),"payload":payload.clone().map(redact_session_value),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_session_value)})
        }
        CommitSessionTurnUpdate::UnknownEffect {
            source_key,
            effect_id,
            operation_digest,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"unknown_effect","source_key":redact_session_text(source_key),"effect_id":redact_session_text(effect_id),"operation_digest":redact_session_text(operation_digest),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_session_value)})
        }
        CommitSessionTurnUpdate::ReconcileUnknownEffect {
            source_key,
            effect_id,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"reconcile_unknown_effect","source_key":redact_session_text(source_key),"effect_id":redact_session_text(effect_id),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_session_value)})
        }
        CommitSessionTurnUpdate::RequestPermission {
            source_key,
            request,
        } => {
            serde_json::json!({"kind":"request_permission","source_key":redact_session_text(source_key),"request":redact_permission(request)})
        }
        CommitSessionTurnUpdate::ResolvePermission {
            source_key,
            request_id,
            allow,
            rebound_operation_digest,
        } => {
            serde_json::json!({"kind":"resolve_permission","source_key":redact_session_text(source_key),"request_id":redact_session_text(request_id),"allow":allow,"rebound_operation_digest":rebound_operation_digest})
        }
        CommitSessionTurnUpdate::RequestInput {
            source_key,
            request,
        } => {
            serde_json::json!({"kind":"request_input","source_key":redact_session_text(source_key),"request":redact_input(request)})
        }
        CommitSessionTurnUpdate::ProvideInput {
            source_key,
            request_id,
            value,
        } => {
            serde_json::json!({"kind":"provide_input","source_key":redact_session_text(source_key),"request_id":redact_session_text(request_id),"value":redact_session_text(value)})
        }
        CommitSessionTurnUpdate::Complete {
            source_key,
            handoff,
        } => {
            serde_json::json!({"kind":"complete","source_key":redact_session_text(source_key),"handoff":redact_handoff(handoff)})
        }
        CommitSessionTurnUpdate::CompleteVerified {
            source_key,
            summary,
            verification_effect_id,
            verified_manifest_digest,
            files_changed,
        } => {
            serde_json::json!({"kind":"complete_verified","source_key":redact_session_text(source_key),"summary":redact_session_text(summary),"verification_effect_id":redact_session_text(verification_effect_id),"verified_manifest_digest":redact_session_text(verified_manifest_digest),"files_changed":files_changed.iter().map(|path|redact_session_text(path)).collect::<Vec<_>>()})
        }
        CommitSessionTurnUpdate::Fail {
            source_key,
            failure,
        } => {
            serde_json::json!({"kind":"fail","source_key":redact_session_text(source_key),"failure":redact_failure(failure)})
        }
        CommitSessionTurnUpdate::Interrupt {
            source_key,
            reconciliation_effect_id,
        } => {
            serde_json::json!({"kind":"interrupt","source_key":redact_session_text(source_key),"effect_id":reconciliation_effect_id.as_deref().map(redact_session_text)})
        }
    };
    let canonical = serde_json::json!({
        "session_id":request.session_id.to_string(), "command_id":request.command_id.to_string(), "turn_id":request.turn_id.to_string(),
        "expected_session_revision":request.expected_session_revision, "expected_turn_revision":request.expected_turn_revision,
        "request_id":request.request_id.as_deref().map(redact_session_text), "effect_id":request.effect_id.as_deref().map(redact_session_text), "update":update
    });
    let bytes = serde_json::to_vec(&canonical).map_err(invalid_json)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_session_effect_id(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(StorageError::InvalidData(
            "invalid session effect id".into(),
        ));
    }
    Ok(())
}

fn validate_session_digest(value: &str) -> Result<(), StorageError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StorageError::InvalidData(
            "invalid session effect operation digest".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
/// Undoes migrations 13-15 on a fully-migrated database so a test fixture can
/// present a pre-13 layout (the historical v2 table names) to the legacy
/// importer or to the upgrade path. Mirrors the forward migrations: schema 15
/// (turn→run) first, then 14 (drop the counter), then 13 (session→thread);
/// columns back before tables. Test-only.
#[cfg(test)]
const REVERSE_SCHEMA_15: &str = r"
    PRAGMA legacy_alter_table=OFF;
    ALTER TABLE sessions RENAME COLUMN latest_turn_id TO latest_run_id;
    ALTER TABLE session_effect_canonical RENAME COLUMN turn_id TO run_id;
    ALTER TABLE conversation_outbox RENAME COLUMN turn_id TO run_id;
    ALTER TABLE session_active_turns RENAME COLUMN turn_id TO run_id;
    ALTER TABLE session_turns RENAME COLUMN parent_turn_id TO parent_run_id;
    ALTER TABLE session_turns RENAME COLUMN turn_id TO run_id;
    ALTER TABLE pending_permissions RENAME COLUMN turn_revision TO run_revision;
    ALTER TABLE pending_permissions RENAME COLUMN turn_id TO run_id;
    ALTER TABLE runtime_checkpoints RENAME COLUMN turn_id TO run_id;
    ALTER TABLE turn_read_model RENAME COLUMN turn_id TO run_id;
    ALTER TABLE turn_baselines RENAME COLUMN turn_id TO run_id;
    ALTER TABLE evidence RENAME COLUMN turn_id TO run_id;
    ALTER TABLE effects RENAME COLUMN turn_id TO run_id;
    ALTER TABLE events RENAME COLUMN turn_id TO run_id;
    ALTER TABLE turns RENAME COLUMN turn_id TO run_id;
    ALTER TABLE turn_read_model RENAME TO run_read_model;
    ALTER TABLE turn_baselines RENAME TO run_baselines;
    ALTER TABLE turns RENAME TO runs;
    ALTER TABLE session_active_turns RENAME TO session_active_runs;
    ALTER TABLE session_turns RENAME TO session_runs;
    DELETE FROM schema_migrations WHERE version=15;
";

#[cfg(test)]
const REVERSE_SCHEMA_13: &str = r"
    PRAGMA legacy_alter_table=OFF;
    ALTER TABLE session_runs DROP COLUMN tool_round_count;
    DROP INDEX IF EXISTS sessions_workspace_activity;
    DROP INDEX IF EXISTS sessions_parent;
    CREATE INDEX IF NOT EXISTS thread_sessions_workspace_activity
      ON sessions(workspace_root,updated_at_ms DESC);
    CREATE INDEX IF NOT EXISTS thread_sessions_parent
      ON sessions(parent_session_id,created_at_ms);
    ALTER TABLE sessions RENAME COLUMN session_id TO thread_id;
    ALTER TABLE sessions RENAME COLUMN parent_session_id TO parent_thread_id;
    ALTER TABLE session_runs RENAME COLUMN session_id TO thread_id;
    ALTER TABLE session_active_runs RENAME COLUMN session_id TO thread_id;
    ALTER TABLE session_events RENAME COLUMN session_id TO thread_id;
    ALTER TABLE session_commit_sources RENAME COLUMN session_id TO thread_id;
    ALTER TABLE conversation_outbox RENAME COLUMN session_id TO thread_id;
    ALTER TABLE session_effect_canonical RENAME TO thread_effect_canonical_v2;
    ALTER TABLE session_commit_sources RENAME TO thread_commit_sources_v2;
    ALTER TABLE session_command_dedup RENAME TO thread_command_dedup_v2;
    ALTER TABLE session_events RENAME TO thread_events_v2;
    ALTER TABLE session_active_runs RENAME TO thread_active_runs_v2;
    ALTER TABLE session_runs RENAME TO thread_runs_v2;
    ALTER TABLE sessions RENAME TO threads_v2;
    DELETE FROM schema_migrations WHERE version IN (13,14);
";

/// Rewrites a fully-migrated on-disk database to the pre-13 layout so a test
/// can feed it to [`Storage::import_legacy_database`] as a genuine legacy
/// source. Test-only.
#[cfg(test)]
pub(crate) fn downgrade_database_to_v12_for_legacy_fixture(path: &Path) {
    let connection = Connection::open(path).expect("open legacy fixture database");
    connection
        .execute_batch(&format!(
            "{REVERSE_SCHEMA_15}{REVERSE_SCHEMA_13} PRAGMA user_version=12;"
        ))
        .expect("downgrade fixture database to the pre-13 v2 layout");
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_core::{IdSource, SystemIdSource, Transition};
    use tempfile::TempDir;

    fn ids() -> (TurnId, EventId) {
        let source = SystemIdSource::default();
        (
            TurnId::from_uuid(source.next_uuid_v7()),
            EventId::from_uuid(source.next_uuid_v7()),
        )
    }
    fn db() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        (dir, path)
    }

    #[test]
    fn global_catalog_registers_projects_and_refreshes_workspace_observation() {
        let store = Storage::memory().unwrap();
        assert!(
            store
                .register_workspace(
                    "/workspace/one",
                    "invalid/project",
                    "/repo/.git",
                    "workspace-one-123",
                    None,
                    1,
                )
                .is_err()
        );
        assert!(
            store
                .register_workspace(
                    "/workspace/one",
                    "project-abc",
                    "",
                    "workspace-one-123",
                    None,
                    1,
                )
                .is_err()
        );
        assert!(
            store
                .register_workspace(
                    "/workspace/one",
                    "project-abc",
                    "/repo/.git",
                    "workspace-one-123",
                    Some("bad\ncommon-dir"),
                    1,
                )
                .is_err()
        );
        store
            .register_workspace(
                "/workspace/one",
                "project-abc",
                "/repo/.git",
                "workspace-one-123",
                Some("/repo/.git"),
                10,
            )
            .unwrap();
        store
            .register_workspace(
                "/workspace/one",
                "project-abc",
                "/repo/.git",
                "workspace-one-123",
                Some("/repo/.git"),
                20,
            )
            .unwrap();
        let connection = store.connection.lock().unwrap();
        let project_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
            .unwrap();
        let workspace: (String, String, i64, i64) = connection
            .query_row(
                "SELECT project_key,storage_key,first_seen_at_ms,last_seen_at_ms FROM workspaces WHERE workspace_root='/workspace/one'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(project_count, 1);
        assert_eq!(
            workspace,
            ("project-abc".into(), "workspace-one-123".into(), 10, 20)
        );
    }

    #[test]
    fn legacy_import_is_idempotent_preserves_source_and_rejects_foreign_sessions() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let (source_dir, source_path) = db();
        let source = Storage::open(&source_path).unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        source
            .create_session_v2(
                session_id,
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                source_dir.path().to_str().unwrap(),
                "imported conversation",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        drop(source);
        // Legacy import migrates a pre-schema-13 workspace database, so the
        // source must carry the historical v2 layout the importer reads from.
        {
            let connection = Connection::open(&source_path).unwrap();
            connection
                .execute_batch(&format!(
                    "{REVERSE_SCHEMA_15}{REVERSE_SCHEMA_13} PRAGMA user_version=12;"
                ))
                .unwrap();
        }
        let before = std::fs::read(&source_path).unwrap();

        let destination = Storage::memory().unwrap();
        assert!(
            destination
                .import_legacy_database(
                    &source_path,
                    source_path.to_str().unwrap(),
                    "sha256-valid",
                    source_dir.path().to_str().unwrap(),
                    2,
                )
                .unwrap()
        );
        assert_eq!(
            destination.list_sessions().unwrap()[0].session_id,
            session_id
        );
        assert!(
            !destination
                .import_legacy_database(
                    &source_path,
                    source_path.to_str().unwrap(),
                    "sha256-valid",
                    source_dir.path().to_str().unwrap(),
                    3,
                )
                .unwrap()
        );
        assert_eq!(std::fs::read(&source_path).unwrap(), before);

        let foreign = Storage::memory().unwrap();
        assert!(matches!(
            foreign.import_legacy_database(
                &source_path,
                source_path.to_str().unwrap(),
                "sha256-foreign",
                "/another/workspace",
                2,
            ),
            Err(StorageError::InvalidData(message))
                if message.contains("another workspace")
        ));
    }

    /// A failed DETACH leaves `legacy_import` attached to the shared
    /// connection, and every later import would then fail at ATTACH with
    /// "already in use" — one unlucky detach permanently breaking imports.
    /// The pre-attach best-effort detach makes such a retry self-heal: the
    /// stale alias is dropped and the import proceeds. Mutation anchor:
    /// deleting the pre-attach detach fails this test inside
    /// `import_legacy_database` with "already in use".
    #[test]
    fn import_legacy_database_self_heals_a_stale_alias_attachment() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let (source_dir, source_path) = db();
        let source = Storage::open(&source_path).unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        source
            .create_session_v2(
                session_id,
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                source_dir.path().to_str().unwrap(),
                "imported conversation",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        drop(source);
        {
            let connection = Connection::open(&source_path).unwrap();
            connection
                .execute_batch(&format!(
                    "{REVERSE_SCHEMA_15}{REVERSE_SCHEMA_13} PRAGMA user_version=12;"
                ))
                .unwrap();
        }

        let destination = Storage::memory().unwrap();
        // Poison the shared connection the way a failed DETACH would: leave
        // the reserved alias attached, pointing at the same file.
        {
            let conn = destination.connection.lock().unwrap();
            conn.execute(
                "ATTACH DATABASE ?1 AS legacy_import",
                [source_path.to_string_lossy().as_ref()],
            )
            .unwrap();
        }
        assert!(
            destination
                .import_legacy_database(
                    &source_path,
                    source_path.to_str().unwrap(),
                    "sha256-selfheal",
                    source_dir.path().to_str().unwrap(),
                    2,
                )
                .unwrap()
        );
        assert_eq!(
            destination.list_sessions().unwrap()[0].session_id,
            session_id
        );
    }

    #[test]
    fn legacy_import_adapts_a_pre_v10_source_without_parent_focus_or_outbox() {
        // The importer must tolerate the oldest supported v2 shape: a schema 9
        // database whose `threads_v2` predates `parent_thread_id` (v10) and
        // `focus` (v12), and whose transcript lives in the pre-v7
        // `thread_transcript_v2` table rather than `conversation_outbox`.
        // Build through the current engine (so every v1 table has its real
        // columns), then peel migrations 10-13 off: undo 13's renames, rebuild
        // `threads_v2` without the columns v10/v12 add, move the transcript to
        // the pre-v7 table name, and drop the v10/v11 workspace infrastructure.
        use latte_core::{SessionId, TurnId};
        let (source_dir, source_path) = db();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let workspace = source_dir.path().to_str().unwrap();
        {
            let source = Storage::open(&source_path).unwrap();
            source
                .create_session_v2(
                    session_id,
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    &session_binding(),
                    workspace,
                    "old conversation",
                    &std::collections::BTreeMap::new(),
                    1,
                )
                .unwrap();
        }
        {
            let conn = Connection::open(&source_path).unwrap();
            conn.execute_batch(
                &format!(
                    "PRAGMA foreign_keys=OFF;\n{REVERSE_SCHEMA_15}{}",
                    REVERSE_SCHEMA_13
                    .replace(
                        "    ALTER TABLE sessions RENAME TO threads_v2;\n",
                        // Recreate threads_v2 without parent_thread_id / focus.
                        "    ALTER TABLE sessions RENAME TO threads_full;\n    \
                         CREATE TABLE threads_v2(\n      \
                           thread_id TEXT PRIMARY KEY, revision INTEGER NOT NULL,\n      \
                           last_seq INTEGER NOT NULL DEFAULT 0, lifecycle TEXT NOT NULL,\n      \
                           binding_json TEXT NOT NULL, latest_run_id TEXT,\n      \
                           created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,\n      \
                           title TEXT NOT NULL DEFAULT '', workspace_root TEXT NOT NULL DEFAULT ''\n    \
                         );\n    \
                         INSERT INTO threads_v2(thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root)\n      \
                         SELECT thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root FROM threads_full;\n    \
                         DROP TABLE threads_full;\n",
                    )
                    // The pre-v7 transcript table takes conversation_outbox's place.
                    .replace(
                        "DELETE FROM schema_migrations WHERE version IN (13,14);",
                        "ALTER TABLE conversation_outbox RENAME TO thread_transcript_v2;\n    \
                         DROP TABLE legacy_imports;\n    DROP TABLE workspaces;\n    DROP TABLE projects;\n    \
                         DROP TABLE runtime_lease;\n    DROP TABLE runtime_lease_epoch;\n    \
                         CREATE TABLE runtime_lease(\n      \
                           singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL,\n      \
                           fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL\n    \
                         );\n    \
                         DELETE FROM schema_migrations WHERE version IN (10,11,12,13,14);\n    \
                         PRAGMA user_version=9;",
                    ),
                )
            )
            .unwrap();
        }

        let destination = Storage::memory().unwrap();
        assert!(
            destination
                .import_legacy_database(
                    &source_path,
                    source_path.to_str().unwrap(),
                    "sha256-old",
                    workspace,
                    2
                )
                .unwrap()
        );
        let sessions = destination
            .list_session_summaries_for_workspace(workspace, 10)
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        // Parent/focus were absent in the source and must come through as NULL.
        assert!(sessions[0].parent_session_id.is_none());
        // The imported session is readable after the pre-v7 transcript table
        // (present, carrying the opening user card) was copied into
        // conversation_outbox.
        let page = destination.session_snapshot_tail_v2(session_id, 1).unwrap();
        assert_eq!(page.session_id, session_id);
        assert!(
            page.transcript
                .entries
                .iter()
                .any(|entry| entry.text.contains("old conversation"))
        );
    }

    #[test]
    fn legacy_import_rejects_same_unsupported_and_colliding_databases() {
        use latte_core::SessionId;

        let (source_dir, source_path) = db();
        let source = Storage::open(&source_path).unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        source
            .create_session_v2(
                session_id,
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                source_dir.path().to_str().unwrap(),
                "source",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        assert!(
            !source
                .import_legacy_database(
                    &source_path,
                    source_path.to_str().unwrap(),
                    "sha256-same",
                    source_dir.path().to_str().unwrap(),
                    2,
                )
                .unwrap()
        );
        drop(source);
        // The importer reads the historical pre-13 layout, so downgrade the
        // source to that shape before another database imports it. (The
        // self-import above returned `false` via the same-file guard and never
        // read any table.)
        {
            let connection = Connection::open(&source_path).unwrap();
            connection
                .execute_batch(&format!(
                    "{REVERSE_SCHEMA_15}{REVERSE_SCHEMA_13} PRAGMA user_version=12;"
                ))
                .unwrap();
        }

        let unsupported_dir = tempfile::tempdir().unwrap();
        let unsupported_path = unsupported_dir.path().join("v8.db");
        let unsupported = Connection::open(&unsupported_path).unwrap();
        unsupported.pragma_update(None, "user_version", 8).unwrap();
        drop(unsupported);
        let destination = Storage::memory().unwrap();
        assert!(matches!(
            destination.import_legacy_database(
                &unsupported_path,
                unsupported_path.to_str().unwrap(),
                "sha256-v8",
                source_dir.path().to_str().unwrap(),
                3,
            ),
            Err(StorageError::InvalidData(message)) if message.contains("expected 9 through 12")
        ));

        destination
            .create_session_v2(
                session_id,
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                source_dir.path().to_str().unwrap(),
                "collision",
                &std::collections::BTreeMap::new(),
                4,
            )
            .unwrap();
        assert!(matches!(
            destination.import_legacy_database(
                &source_path,
                source_path.to_str().unwrap(),
                "sha256-collision",
                source_dir.path().to_str().unwrap(),
                5,
            ),
            Err(StorageError::InvalidData(message)) if message.contains("collides")
        ));
    }

    #[test]
    fn checkpoint_write_is_fenced_atomically_after_takeover() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let queued = TurnState::queued(run);
        store.create_turn(&queued, 1).unwrap();
        let a = store.acquire_lease("a", 2, 100).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                3,
                &a,
            )
            .unwrap();
        store
            .put_checkpoint(run, 1, &a, r#"{"owner":"a"}"#, 4)
            .unwrap();
        store.release_lease(&a).unwrap();
        let b = store.acquire_lease("b", 5, 100).unwrap();
        let interrupted = running.transition(1, Transition::Interrupt).unwrap();
        store
            .append_event(
                &interrupted,
                1,
                ids().1,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Interrupted,
                },
                6,
                &b,
            )
            .unwrap();
        store
            .put_checkpoint(run, 2, &b, r#"{"owner":"b"}"#, 7)
            .unwrap();
        assert!(matches!(
            store.put_checkpoint(run, 1, &a, r#"{"owner":"stale"}"#, 8),
            Err(StorageError::LeaseLost)
        ));
        assert_eq!(
            store.checkpoint(run).unwrap().as_deref(),
            Some(r#"{"owner":"b"}"#)
        );
    }
    #[test]
    fn prepared_permission_rejects_stale_expired_and_wrong_revision_without_partial_ledger() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let q = TurnState::queued(run);
        store.create_turn(&q, 1).unwrap();
        let a = store.acquire_lease("a", 2, 5).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                3,
                &a,
            )
            .unwrap();
        let b = store.acquire_lease("b", 8, 100).unwrap();
        assert!(
            store
                .create_prepared_permission("stale", run, 1, 3, 1, "{}", "d", &a, 9)
                .is_err()
        );
        assert!(store.effect_status("stale").is_err());
        assert!(
            store
                .create_prepared_permission("wrong-rev", run, 9, 11, 1, "{}", "d", &b, 9)
                .is_err()
        );
        assert!(store.effect_status("wrong-rev").is_err());
        assert!(
            store
                .create_prepared_permission("valid", run, 1, 3, 1, "{}", "d", &b, 9)
                .is_err(),
            "run remains bound to token a until owner b appends"
        );
        assert!(store.effect_status("valid").is_err());
    }
    #[test]
    fn verification_evidence_is_fenced_and_json_checked() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let q = TurnState::queued(run);
        store.create_turn(&q, 1).unwrap();
        let lease = store.acquire_lease("owner", 2, 100).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                3,
                &lease,
            )
            .unwrap();
        assert!(
            store
                .record_verification_evidence(
                    run,
                    99,
                    &lease,
                    &VerificationEvidence {
                        id: "wrong",
                        metadata_json: "{}",
                        blob_ref: None
                    },
                    4
                )
                .is_err()
        );
        assert!(
            store
                .record_verification_evidence(
                    run,
                    1,
                    &lease,
                    &VerificationEvidence {
                        id: "bad-json",
                        metadata_json: "{",
                        blob_ref: None
                    },
                    4
                )
                .is_err()
        );
        store
            .record_verification_evidence(
                run,
                1,
                &lease,
                &VerificationEvidence {
                    id: "ok",
                    metadata_json: "{\"revision\":1,\"effect_epoch\":0,\"effect_id\":\"ok\",\"passed\":true,\"workspace_manifest_digest\":\"digest\",\"summary\":\"ok\"}",
                    blob_ref: Some("blob"),
                },
                4,
            )
            .unwrap();
        assert_eq!(lease.owner(), "owner");
        assert_eq!(lease.fencing_token(), 1);
        assert_eq!(lease.expires_at_ms(), 102);
    }
    #[test]
    fn stale_effect_authority_cannot_overwrite_privileged_unknown_recovery() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let q = TurnState::queued(run);
        store.create_turn(&q, 1).unwrap();
        let a = store.acquire_lease("a", 2, 10).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                3,
                &a,
            )
            .unwrap();
        store
            .create_prepared_permission("long", run, 1, 1, 1, "{}", "digest", &a, 4)
            .unwrap();
        let authority = store
            .consume_permission_and_start("long", run, 1, &a, "digest", 5)
            .unwrap();
        let _b = store.acquire_lease("b", 13, 100).unwrap();
        assert!(matches!(
            store.interrupt_after_lease_loss(run, &a, 1, 14).unwrap(),
            LeaseLossRecovery::Interrupted(_)
        ));
        assert_eq!(store.effect_status("long").unwrap(), EffectStatus::Unknown);
        assert!(matches!(
            store.finish_effect(&authority, true, "{}", 15),
            Err(StorageError::EffectFenced)
        ));
        assert_eq!(store.effect_status("long").unwrap(), EffectStatus::Unknown);
        assert_eq!(
            store.load_turn(run).unwrap().status,
            TurnStatus::Interrupted
        );
    }
    #[test]
    fn second_open_is_read_only_while_live_then_recovers_orphan_once() {
        let (_dir, path) = db();
        let now = crate::wall_now_ms();
        let first = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let q = TurnState::queued(run);
        first.create_turn(&q, now).unwrap();
        // The TTL must outlast any scheduling starvation of this test thread:
        // `Storage::open` below runs an orphan sweep at the real wall clock,
        // and under a fully loaded test binary (180+ cases in parallel) a
        // short TTL has repeatedly expired before `open`, flipping the turn
        // to `Interrupted` and failing the read-only assertion. Recovery is
        // driven deterministically via `recover_at`, so a long TTL here does
        // not weaken any assertion — it only decouples the test from
        // wall-clock starvation.
        let lease = first.acquire_lease("live", now, 3_600_000).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        first
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                now,
                &lease,
            )
            .unwrap();
        first
            .declare_effect("live-effect", run, 1, "{}", now)
            .unwrap();
        first.prepare_effect("live-effect", "d", "{}", now).unwrap();
        first
            .start_prepared_effect("live-effect", "d", now)
            .unwrap();
        let second = Storage::open(&path).unwrap();
        assert_eq!(second.load_turn(run).unwrap(), running);
        assert_eq!(
            second.effect_status("live-effect").unwrap(),
            EffectStatus::Started
        );
        second.recover_at(lease.expires_at_ms() + 1).unwrap();
        let recovered = second.load_turn(run).unwrap();
        assert_eq!(recovered.status, TurnStatus::Interrupted);
        assert_eq!(recovered.revision, 2);
        assert_eq!(
            second.effect_status("live-effect").unwrap(),
            EffectStatus::Unknown
        );
        second.recover_at(lease.expires_at_ms() + 2).unwrap();
        assert_eq!(second.load_turn(run).unwrap(), recovered);
    }

    #[test]
    fn unknown_reconcile_rejects_cross_run_without_mutation() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let lease = store.acquire_lease("owner", 1, 100).unwrap();
        let (a, ea) = ids();
        let (b, eb) = ids();
        for (run, event) in [(a, ea), (b, eb)] {
            let q = TurnState::queued(run);
            store.create_turn(&q, 1).unwrap();
            let r = q.transition(0, Transition::Start).unwrap();
            store
                .append_event(
                    &r,
                    0,
                    event,
                    &RuntimeEvent::StateChanged {
                        status: TurnStatus::Running,
                    },
                    2,
                    &lease,
                )
                .unwrap();
        }
        store.start_effect("effect-a", a, 3).unwrap();
        store.seed_unknown_for_recovery_test("effect-a").unwrap();
        assert!(
            store
                .reconcile_unknown_and_abort(b, "effect-a", 1, &lease, 4)
                .is_err()
        );
        assert_eq!(
            store.effect_status("effect-a").unwrap(),
            EffectStatus::Unknown
        );
        assert_eq!(store.load_turn(a).unwrap().status, TurnStatus::Running);
        assert_eq!(store.load_turn(b).unwrap().status, TurnStatus::Running);
    }

    #[test]
    fn exact_unknown_reconcile_atomically_aborts_own_run() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let lease = store.acquire_lease("owner", 1, 100).unwrap();
        let (run, event) = ids();
        let queued = TurnState::queued(run);
        store.create_turn(&queued, 1).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                2,
                &lease,
            )
            .unwrap();
        store.start_effect("unknown", run, 3).unwrap();
        store.seed_unknown_for_recovery_test("unknown").unwrap();
        let failed = store
            .reconcile_unknown_and_abort(run, "unknown", 1, &lease, 4)
            .unwrap();
        assert_eq!(failed.status, TurnStatus::Failed);
        assert_eq!(failed.revision, 2);
        assert_eq!(
            store.effect_status("unknown").unwrap(),
            EffectStatus::ObservedFailed
        );
        assert_eq!(store.load_turn(run).unwrap(), failed);
    }

    #[test]
    fn lease_loss_recovery_interrupts_only_matching_stored_token() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let queued = TurnState::queued(run);
        store.create_turn(&queued, 1).unwrap();
        let lease = store.acquire_lease("lost", 1, 2).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                2,
                &lease,
            )
            .unwrap();
        store.start_effect("started", run, 2).unwrap();
        assert!(
            matches!(store.interrupt_after_lease_loss(run,&lease,1,4).unwrap(),LeaseLossRecovery::Interrupted(state) if state.status==TurnStatus::Interrupted)
        );
        assert_eq!(
            store.effect_status("started").unwrap(),
            EffectStatus::Unknown
        );
        assert!(matches!(
            store.interrupt_after_lease_loss(run, &lease, 1, 5).unwrap(),
            LeaseLossRecovery::FencedNoop
        ));
    }

    #[test]
    fn bootstrap_append_projection_reopen_and_stale_revision() {
        let (_dir, path) = db();
        let (run, event) = ids();
        let store = Storage::open(&path).unwrap();
        let lease = store.acquire_lease("owner", 1, 100).unwrap();
        let queued = TurnState::queued(run);
        store.create_turn(&queued, 1).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        assert_eq!(
            store
                .append_event(
                    &running,
                    0,
                    event,
                    &RuntimeEvent::StateChanged {
                        status: TurnStatus::Running
                    },
                    2,
                    &lease
                )
                .unwrap()
                .sequence,
            1
        );
        assert!(matches!(
            store.append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running
                },
                3,
                &lease
            ),
            Err(StorageError::StaleRevision { .. })
        ));
        assert_eq!(store.load_turn(run).unwrap(), running);
        drop(store);
        let reopened = Storage::open(&path).unwrap();
        let recovered = reopened.load_turn(run).unwrap();
        assert_eq!(recovered.status, TurnStatus::Interrupted);
        assert_eq!(recovered.revision, 2);
        assert_eq!(reopened.list_runs().unwrap(), vec![recovered]);
    }

    #[test]
    fn lease_renewal_takeover_and_stale_writer() {
        let store = Storage::memory().unwrap();
        let first = store.acquire_lease("a", 10, 10).unwrap();
        assert_eq!(first.fencing_token, 1);
        assert!(matches!(
            store.acquire_lease("b", 15, 10),
            Err(StorageError::EngineUnavailable)
        ));
        let first = store.renew_lease(&first, 15, 10).unwrap();
        let second = store.acquire_lease("b", 25, 10).unwrap();
        assert_eq!(second.fencing_token, 2);
        assert!(matches!(
            store.renew_lease(&first, 26, 10),
            Err(StorageError::LeaseLost)
        ));
        let (run, event) = ids();
        let queued = TurnState::queued(run);
        store.create_turn(&queued, 1).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        assert!(matches!(
            store.append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                26,
                &first
            ),
            Err(StorageError::LeaseLost)
        ));
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                26,
                &second,
            )
            .unwrap();
        store.release_lease(&second).unwrap();
    }

    #[test]
    fn same_owner_reacquire_after_expiry_starts_new_fencing_epoch() {
        let store = Storage::memory().unwrap();
        let first = store.acquire_lease("owner", 10, 10).unwrap();
        let second = store.acquire_lease("owner", 20, 10).unwrap();
        assert_eq!(second.fencing_token, first.fencing_token + 1);
    }

    #[test]
    fn session_leases_are_concurrent_but_same_session_takeover_fences_stale_authority() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let first_session = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let second_session = latte_core::SessionId::from_uuid(ids.next_uuid_v7());

        let first = store.acquire_session_lease(first_session, 10, 100).unwrap();
        let second = store
            .acquire_session_lease(second_session, 11, 100)
            .unwrap();
        assert_eq!(first.scope(), format!("session:{first_session}"));
        assert_ne!(first.scope, second.scope);
        assert!(second.fencing_token > first.fencing_token);
        assert!(matches!(
            store.acquire_session_lease(first_session, 12, 100),
            Err(StorageError::EngineUnavailable)
        ));
        assert_eq!(
            store.renew_lease(&first, 12, 100).unwrap().fencing_token,
            first.fencing_token
        );

        store.release_lease(&first).unwrap();
        let takeover = store.acquire_session_lease(first_session, 13, 100).unwrap();
        assert!(takeover.fencing_token > second.fencing_token);
        assert!(matches!(
            store.renew_lease(&first, 14, 100),
            Err(StorageError::LeaseLost)
        ));

        let (_, linked_turn, _) = create_linked_fixture(&store, &ids, "linked authority", 15);
        assert!(matches!(
            store.acquire_turn_lease(linked_turn, "legacy-linked", 16, 100),
            Err(StorageError::LinkedTurnRequiresSessionCommit)
        ));
        let missing_turn = TurnId::from_uuid(ids.next_uuid_v7());
        assert!(matches!(
            store.acquire_turn_lease(missing_turn, "legacy-missing", 17, 100),
            Err(StorageError::TurnNotFound(id)) if id == missing_turn
        ));

        store.release_lease(&takeover).unwrap();
        store.release_lease(&second).unwrap();
    }

    #[test]
    fn releasing_a_running_session_lease_recovers_immediately() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_session_lease(session_id, 1, 100).unwrap();
        let started = match store
            .create_started_session_v2(
                None,
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "accepted",
                &std::collections::BTreeMap::new(),
                &lease,
                2,
                None,
            )
            .unwrap()
        {
            latte_core::CreateOutcome::Created(s) | latte_core::CreateOutcome::Replayed(s) => s,
        };
        assert_eq!(started.lifecycle, SessionLifecycle::Running);

        let recovered = store
            .release_lease(&lease)
            .unwrap()
            .expect("running release must recover");
        assert_eq!(recovered.snapshot.lifecycle, SessionLifecycle::Interrupted);
        assert!(recovered.snapshot.active_turn_id.is_none());
        assert_eq!(
            store.session_snapshot_v2(session_id, None, 100).unwrap(),
            recovered.snapshot
        );
    }

    #[test]
    fn ready_session_switches_binding_durably_under_exact_session_authority() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (session_id, turn_id, created) =
            create_linked_fixture(&store, &ids, "switch model", 10);
        let lease = store.acquire_session_lease(session_id, 11, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &created,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "switch:start".into(),
            },
            12,
        )
        .snapshot;
        let ready = commit_linked(
            &store,
            &ids,
            &lease,
            &running,
            turn_id,
            CommitSessionTurnUpdate::Fail {
                source_key: "switch:retryable".into(),
                failure: TurnFailure {
                    code: FailureCode::RuntimeFailed,
                    message: "retry with another model".into(),
                    retryability: Retryability::Retryable,
                },
            },
            13,
        )
        .snapshot;
        assert_eq!(ready.lifecycle, SessionLifecycle::Ready);

        let mut next = session_binding();
        next.provider_name = "other-provider".into();
        next.model = "other-model".into();
        next.config_fingerprint = "other-config".into();
        let switched = store
            .switch_session_binding_v2(session_id, ready.revision, &next, &lease, 14)
            .unwrap();
        assert_eq!(switched.snapshot.binding, next);
        assert_eq!(switched.snapshot.lifecycle, SessionLifecycle::Ready);
        assert!(matches!(
            switched.session_event.envelope.event,
            SessionEvent::BindingChanged {
                ref provider_name,
                ref model
            } if provider_name == "other-provider" && model == "other-model"
        ));
        let card = switched.snapshot.transcript.entries.last().unwrap();
        assert_eq!(card.kind, TranscriptKind::System);
        assert_eq!(card.text, "Model switched to other-provider/other-model");

        let foreign_session = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let foreign = store
            .acquire_session_lease(foreign_session, 15, 100)
            .unwrap();
        assert!(matches!(
            store.switch_session_binding_v2(
                session_id,
                switched.snapshot.revision,
                &session_binding(),
                &foreign,
                16
            ),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn session_snapshot_projects_failure_code_from_durable_run_state() {
        // The CLI exit-code contract needs to distinguish a permission-denied
        // run from any other terminal failure. failure_code must be projected
        // from the durable TurnState.failure.code, not hardcoded to None.
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();

        // A terminal permission-denied run projects PermissionDenied.
        let (denied_session, denied_turn, created) =
            create_linked_fixture(&store, &ids, "denied run", 10);
        let lease = store
            .acquire_session_lease(denied_session, 11, 100)
            .unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &created,
            denied_turn,
            CommitSessionTurnUpdate::Start {
                source_key: "denied:start".into(),
            },
            12,
        )
        .snapshot;
        // A run still executing has no failure code yet.
        assert_eq!(
            running
                .turns
                .iter()
                .find(|run| run.turn_id == denied_turn)
                .unwrap()
                .failure_code,
            None,
            "an active turn has no failure code"
        );
        let denied = commit_linked(
            &store,
            &ids,
            &lease,
            &running,
            denied_turn,
            CommitSessionTurnUpdate::Fail {
                source_key: "denied:fail".into(),
                failure: TurnFailure {
                    code: FailureCode::PermissionDenied,
                    message: "permission was denied".into(),
                    retryability: Retryability::Terminal,
                },
            },
            13,
        )
        .snapshot;
        assert_eq!(
            denied
                .turns
                .iter()
                .find(|run| run.turn_id == denied_turn)
                .unwrap()
                .failure_code,
            Some(FailureCode::PermissionDenied),
            "a permission-denied run projects PermissionDenied"
        );
        // The projection survives a fresh read (not just the in-memory commit).
        let reread = store
            .session_snapshot_v2(denied_session, None, 100)
            .unwrap();
        assert_eq!(
            reread
                .turns
                .iter()
                .find(|run| run.turn_id == denied_turn)
                .unwrap()
                .failure_code,
            Some(FailureCode::PermissionDenied),
        );

        // An ordinary terminal failure projects RuntimeFailed, so exit-code
        // logic can tell it apart from the permission-denied case.
        let (failed_session, failed_turn, created) =
            create_linked_fixture(&store, &ids, "failed run", 20);
        let lease = store
            .acquire_session_lease(failed_session, 21, 100)
            .unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &created,
            failed_turn,
            CommitSessionTurnUpdate::Start {
                source_key: "failed:start".into(),
            },
            22,
        )
        .snapshot;
        let failed = commit_linked(
            &store,
            &ids,
            &lease,
            &running,
            failed_turn,
            CommitSessionTurnUpdate::Fail {
                source_key: "failed:fail".into(),
                failure: TurnFailure {
                    code: FailureCode::RuntimeFailed,
                    message: "runtime blew up".into(),
                    retryability: Retryability::Terminal,
                },
            },
            23,
        )
        .snapshot;
        assert_eq!(
            failed
                .turns
                .iter()
                .find(|run| run.turn_id == failed_turn)
                .unwrap()
                .failure_code,
            Some(FailureCode::RuntimeFailed),
            "an ordinary terminal failure projects RuntimeFailed, not PermissionDenied"
        );
    }

    #[test]
    fn runtime_and_session_lease_scopes_are_bidirectional_authority_boundaries() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let legacy_turn = TurnId::from_uuid(ids.next_uuid_v7());
        store
            .create_turn(&TurnState::queued(legacy_turn), 1)
            .unwrap();
        let runtime_lease = store
            .acquire_turn_lease(legacy_turn, "legacy", 2, 1_000)
            .unwrap();

        let (session_id, linked_turn, queued_session) =
            create_linked_fixture(&store, &ids, "scoped", 3);
        let session_lease = store.acquire_session_lease(session_id, 4, 1_000).unwrap();
        assert!(session_lease.fencing_token > runtime_lease.fencing_token);

        assert!(matches!(
            store.apply_transition(legacy_turn, 0, Transition::Start, 5, &session_lease),
            Err(StorageError::LeaseLost)
        ));
        assert_eq!(
            store.load_turn(legacy_turn).unwrap().status,
            TurnStatus::Queued
        );

        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id: linked_turn,
                    expected_session_revision: queued_session.revision,
                    expected_turn_revision: 0,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitSessionTurnUpdate::Start {
                        source_key: "wrong-runtime-scope".into(),
                    },
                },
                &runtime_lease,
                5,
            ),
            Err(StorageError::LeaseLost)
        ));
        assert_eq!(
            store.load_turn(linked_turn).unwrap().status,
            TurnStatus::Queued
        );

        store
            .apply_transition(legacy_turn, 0, Transition::Start, 6, &runtime_lease)
            .unwrap();
        commit_linked(
            &store,
            &ids,
            &session_lease,
            &queued_session,
            linked_turn,
            CommitSessionTurnUpdate::Start {
                source_key: "correct-session-scope".into(),
            },
            6,
        );
    }

    #[test]
    fn atomic_session_acceptance_rolls_back_every_record_when_start_write_fails() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_session_lease(session_id, 10, 1_000).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER inject_atomic_start_failure \
                 BEFORE INSERT ON session_events \
                 BEGIN SELECT RAISE(ABORT, 'injected atomic start failure'); END;",
            )
            .unwrap();

        let error = store
            .create_started_session_v2(
                None,
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "accepted once",
                &std::collections::BTreeMap::new(),
                &lease,
                11,
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected atomic start failure"));
        assert!(matches!(
            store.load_turn(turn_id),
            Err(StorageError::TurnNotFound(id)) if id == turn_id
        ));
        assert!(matches!(
            store.session_snapshot_v2(session_id, None, 10),
            Err(StorageError::SessionNotFound(id)) if id == session_id
        ));
        let orphan_rows: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM conversation_outbox WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphan_rows, 0);
    }

    #[test]
    fn recovery_marks_cancelling_and_started_effect_unknown() {
        let (_dir, path) = db();
        let (run, event) = ids();
        let store = Storage::open(&path).unwrap();
        let lease = store.acquire_lease("owner", 1, 100).unwrap();
        let queued = TurnState::queued(run);
        store.create_turn(&queued, 1).unwrap();
        let cancelling = queued.transition(0, Transition::Cancel).unwrap();
        store
            .append_event(
                &cancelling,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Cancelling,
                },
                2,
                &lease,
            )
            .unwrap();
        store.start_effect("e", run, 2).unwrap();
        assert_eq!(store.effect_status("e").unwrap(), EffectStatus::Started);
        drop(store);
        let store = Storage::open(&path).unwrap();
        assert_eq!(
            store.load_turn(run).unwrap().status,
            TurnStatus::Interrupted
        );
        assert_eq!(store.effect_status("e").unwrap(), EffectStatus::Unknown);
    }

    #[test]
    fn effect_phases_bind_approval_and_preserve_unknown_without_retry() {
        let (_dir, path) = db();
        let (run, _) = ids();
        {
            let store = Storage::open(&path).unwrap();
            let queued = TurnState::queued(run);
            store.create_turn(&queued, 1).unwrap();
            let lease = store.acquire_lease("owner", 1, 2).unwrap();
            let running = queued.transition(0, Transition::Start).unwrap();
            store
                .append_event(
                    &running,
                    0,
                    ids().1,
                    &RuntimeEvent::StateChanged {
                        status: TurnStatus::Running,
                    },
                    2,
                    &lease,
                )
                .unwrap();
            store
                .declare_effect("phase", run, 1, r#"{"tool":"write_file"}"#, 2)
                .unwrap();
            assert_eq!(
                store.effect_status("phase").unwrap(),
                EffectStatus::Declared
            );
            store
                .prepare_effect("phase", "exact", r#"{"pre":"hash"}"#, 3)
                .unwrap();
            assert_eq!(
                store.effect_status("phase").unwrap(),
                EffectStatus::Prepared
            );
            assert!(store.start_prepared_effect("phase", "wrong", 4).is_err());
            store.start_prepared_effect("phase", "exact", 4).unwrap();
        }
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(
            reopened.effect_status("phase").unwrap(),
            EffectStatus::Unknown
        );
        assert!(reopened.declare_effect("phase", run, 2, "{}", 5).is_err());
    }

    #[test]
    fn effect_success_and_failure_store_terminal_observations() {
        let store = Storage::memory().unwrap();
        let (run, _) = ids();
        store.create_turn(&TurnState::queued(run), 1).unwrap();
        let lease = store.acquire_lease("owner", 2, 100).unwrap();
        for (id, success, status) in [
            ("ok", true, EffectStatus::ObservedSuccess),
            ("fail", false, EffectStatus::ObservedFailed),
        ] {
            store.declare_effect(id, run, 1, "{}", 2).unwrap();
            store.prepare_effect(id, "d", "{}", 3).unwrap();
            store.start_prepared_effect(id, "d", 4).unwrap();
            let authority = EffectAuthority {
                turn_id: run,
                expected_revision: 0,
                lease: lease.clone(),
                effect_id: id.into(),
                digest: "d".into(),
                attempt: 1,
            };
            store
                .finish_effect(&authority, success, r#"{"post":"hash"}"#, 5)
                .unwrap();
            assert_eq!(store.effect_status(id).unwrap(), status);
        }
    }

    #[test]
    fn permission_consumption_rolls_back_when_started_transition_fails() {
        let store = Storage::memory().unwrap();
        let (run, _) = ids();
        store.create_turn(&TurnState::queued(run), 1).unwrap();
        let lease = store.acquire_lease("owner", 2, 100).unwrap();
        store.declare_effect("atomic", run, 1, "{}", 3).unwrap();
        store.prepare_effect("atomic", "digest", "{}", 3).unwrap();
        store
            .persist_permission("atomic", run, 0, &lease, "digest")
            .unwrap();
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE effects SET status='declared' WHERE effect_id='atomic'",
                [],
            )
            .unwrap();
        }
        assert!(
            store
                .consume_permission_and_start("atomic", run, 0, &lease, "digest", 4)
                .is_err()
        );
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE effects SET status='prepared' WHERE effect_id='atomic'",
                [],
            )
            .unwrap();
        }
        store
            .consume_permission_and_start("atomic", run, 0, &lease, "digest", 5)
            .unwrap();
        assert_eq!(
            store.effect_status("atomic").unwrap(),
            EffectStatus::Started
        );
    }

    #[test]
    fn refuses_newer_schema() {
        let (_dir, path) = db();
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(matches!(
            Storage::open(&path),
            Err(StorageError::NewerSchema { found: 99, .. })
        ));
    }

    #[test]
    fn v7_database_upgrades_to_private_canonical_descriptor_boundary() {
        let (_dir, path) = db();
        drop(Storage::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "{REVERSE_SCHEMA_15}{REVERSE_SCHEMA_13}\
                 DROP TABLE thread_effect_canonical_v2; \
                 DROP TABLE legacy_imports; \
                 DROP TABLE workspaces; \
                 DROP TABLE projects; \
                 DROP TABLE runtime_lease; \
                 DROP TABLE runtime_lease_epoch; \
                 CREATE TABLE runtime_lease( \
                   singleton INTEGER PRIMARY KEY CHECK(singleton=1), \
                   owner TEXT NOT NULL, \
                   fencing_token INTEGER NOT NULL, \
                   expires_at_ms INTEGER NOT NULL \
                 ); \
                 INSERT INTO runtime_lease(singleton,owner,fencing_token,expires_at_ms) \
                   VALUES(1,'legacy-owner',7,9999999999999); \
                 DELETE FROM schema_migrations WHERE version IN (8,9,10,11,12); \
                 PRAGMA user_version=7;",
            ))
            .unwrap();
        drop(connection);

        drop(Storage::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let descriptor_table: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_effect_canonical')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // A single open must complete the entire chain to the current schema,
        // not stop partway. The focus column (added in v11->12) proves the
        // final migration step ran in the same open, guarding against the
        // "user_version written but local `version` not advanced" regression
        // that would leave the DB stalled at 11 until a second open.
        let has_focus: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name='focus')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(descriptor_table);
        assert!(
            has_focus,
            "v11->12 focus migration must run in the same open"
        );
        let migrated_lease: (String, String, i64) = connection
            .query_row(
                "SELECT scope,owner,fencing_token FROM runtime_lease",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(migrated_lease, ("runtime".into(), "legacy-owner".into(), 7));
        assert_eq!(
            connection
                .query_row(
                    "SELECT last_token FROM runtime_lease_epoch WHERE singleton=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            7
        );
    }

    #[test]
    fn v8_session_migration_requires_safe_workspace_adoption_and_backfills_catalog() {
        let (_dir, path) = db();
        let ids = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let store = Storage::open(&path).unwrap();
        store
            .create_session_v2(
                session_id,
                turn_id,
                &session_binding(),
                "/old/workspace",
                "legacy title",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "{REVERSE_SCHEMA_15}{REVERSE_SCHEMA_13}\
                 UPDATE threads_v2 SET title='',workspace_root=''; \
                 DROP TABLE legacy_imports; \
                 DROP TABLE workspaces; \
                 DROP TABLE projects; \
                 DROP TABLE runtime_lease; \
                 DROP TABLE runtime_lease_epoch; \
                 CREATE TABLE runtime_lease( \
                   singleton INTEGER PRIMARY KEY CHECK(singleton=1), \
                   owner TEXT NOT NULL, \
                   fencing_token INTEGER NOT NULL, \
                   expires_at_ms INTEGER NOT NULL \
                 ); \
                 DELETE FROM schema_migrations WHERE version IN (9,10,11,12); \
                 PRAGMA user_version=8;",
            ))
            .unwrap();
        drop(connection);

        assert!(matches!(
            Storage::open(&path),
            Err(StorageError::InvalidData(message))
                if message.contains("legacy Sessions have no workspace identity")
        ));
        let migrated = Storage::open_in_workspace(&path, "/adopted/workspace").unwrap();
        let metadata = migrated.session_v2(session_id).unwrap().unwrap();
        assert_eq!(metadata.workspace_root, "/adopted/workspace");
        assert_eq!(metadata.title, "legacy title");
        assert_eq!(
            migrated
                .list_session_summaries_for_workspace("/adopted/workspace", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn required_connection_pragmas_are_active() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let conn = store.connection.lock().unwrap();
        let journal: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        let foreign_keys: i64 = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        let synchronous: i64 = conn
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        let timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 2);
        assert_eq!(timeout, 5_000);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn v2_session_child_is_fenced_idempotent_and_parent_is_immutable() {
        use latte_core::{SessionCommandId, SessionId, SessionProviderBinding};
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session = SessionId::from_uuid(ids.next_uuid_v7());
        let first = TurnId::from_uuid(ids.next_uuid_v7());
        let binding = SessionProviderBinding {
            version: 1,
            provider_name: "p".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "m".into(),
            config_fingerprint: "c".into(),
            tools_fingerprint: "t".into(),
            aliases: std::collections::BTreeMap::default(),
            credential_ref_id: "env:KEY".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        let initial = store
            .create_session_v2(
                session,
                first,
                &binding,
                "/workspace",
                "hello sk-this-is-a-secret-123456789",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        assert_eq!(initial.lifecycle, SessionLifecycle::Running);
        assert_eq!(initial.sequence, 1);
        assert_eq!(initial.transcript.entries[0].sequence, 1);
        assert!(!initial.transcript.entries[0].text.contains("sk-this"));
        assert!(store.is_session_linked_turn(first).unwrap());
        let lease = store.acquire_session_lease(session, 2, 100).unwrap();
        let start = SessionCommitRequest {
            session_id: session,
            turn_id: first,
            expected_session_revision: 0,
            expected_turn_revision: 0,
            command_id: SessionCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: CommitSessionTurnUpdate::Start {
                source_key: "start".into(),
            },
        };
        let started = store.commit_session_turn_update(&start, &lease, 3).unwrap();
        assert_eq!(started.snapshot.turns[0].turn_revision, 1);
        let replay = store.commit_session_turn_update(&start, &lease, 4).unwrap();
        assert_eq!(replay, started);
        let changed = SessionCommitRequest {
            update: CommitSessionTurnUpdate::AppendTranscript {
                source_key: "other".into(),
                kind: TranscriptKind::Assistant,
                text: "different".into(),
                payload: None,
            },
            ..start.clone()
        };
        assert!(matches!(
            store.commit_session_turn_update(&changed, &lease, 5),
            Err(StorageError::SessionCommandReplayMismatch)
        ));
        let completed = SessionCommitRequest {
            session_id: session,
            turn_id: first,
            expected_session_revision: 1,
            expected_turn_revision: 1,
            command_id: SessionCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: CommitSessionTurnUpdate::Complete {
                source_key: "complete".into(),
                handoff: Handoff {
                    summary: "done".into(),
                    files_changed: vec![],
                    evidence: vec![],
                },
            },
        };
        let completed = store
            .commit_session_turn_update(&completed, &lease, 6)
            .unwrap();
        assert_eq!(completed.snapshot.lifecycle, SessionLifecycle::Ready);
        let parent = store.load_turn(first).unwrap();
        assert_eq!(parent.status, TurnStatus::Completed);
        let child = TurnId::from_uuid(ids.next_uuid_v7());
        let followup = store
            .create_session_follow_up_v2(
                session,
                child,
                completed.snapshot.revision,
                "next",
                &std::collections::BTreeMap::new(),
                7,
            )
            .unwrap();
        assert_eq!(followup.turns.len(), 2);
        assert_eq!(followup.turns[1].parent_turn_id, Some(first));
        assert_eq!(store.load_turn(first).unwrap(), parent);
    }

    #[test]
    fn initial_session_cursor_allows_a_queued_run_to_fail_with_a_durable_card() {
        use latte_core::{SessionCommandId, SessionId, SessionProviderBinding};
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let binding = SessionProviderBinding {
            version: 1,
            provider_name: "p".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "m".into(),
            config_fingerprint: "c".into(),
            tools_fingerprint: "t".into(),
            aliases: std::collections::BTreeMap::default(),
            credential_ref_id: "env:KEY".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        let initial = store
            .create_session_v2(
                session_id,
                turn_id,
                &binding,
                "/workspace",
                "durable prompt",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        assert_eq!(initial.sequence, 1);
        assert_eq!(initial.transcript.entries[0].sequence, 1);

        let lease = store.acquire_session_lease(session_id, 2, 100).unwrap();
        let failed = store
            .commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: 0,
                    expected_turn_revision: 0,
                    command_id: SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitSessionTurnUpdate::Fail {
                        source_key: "provider-configuration-failure".into(),
                        failure: TurnFailure {
                            code: FailureCode::RuntimeFailed,
                            message: "provider configuration failed".into(),
                            retryability: Retryability::Terminal,
                        },
                    },
                },
                &lease,
                3,
            )
            .unwrap();

        assert_eq!(failed.snapshot.lifecycle, SessionLifecycle::Failed);
        assert_eq!(failed.snapshot.sequence, 2);
        assert_eq!(failed.snapshot.transcript.entries.len(), 2);
        assert_eq!(failed.snapshot.transcript.entries[0].sequence, 1);
        assert_eq!(failed.snapshot.transcript.entries[1].sequence, 2);
        assert_eq!(
            failed.snapshot.transcript.entries[1].kind,
            TranscriptKind::Failure
        );
    }

    #[test]
    fn v2_session_projection_uses_current_tail_and_marks_bounded_history() {
        use latte_core::{SessionId, SessionProviderBinding};
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let binding = SessionProviderBinding {
            version: 1,
            provider_name: "p".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "m".into(),
            config_fingerprint: "c".into(),
            tools_fingerprint: "t".into(),
            aliases: std::collections::BTreeMap::default(),
            credential_ref_id: "env:KEY".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        };
        store
            .create_session_v2(
                session_id,
                turn_id,
                &binding,
                "/workspace",
                "oldest prompt",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();

        // Insert enough durable cards to exceed the presentation bound. This
        // models a long lived conversation without using private UI state.
        let conn = store.connection.lock().unwrap();
        for sequence in 2..=502_u64 {
            let entry = TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
                sequence,
                turn_id: Some(turn_id),
                kind: TranscriptKind::Assistant,
                text: format!("card-{sequence}"),
                payload: None,
                source_key: format!("fixture:{sequence}"),
                created_at_ms: sequence,
            };
            conn.execute(
                "INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,'assistant',?5,?6,?7)",
                params![
                    session_id.to_string(),
                    to_i64(sequence).unwrap(),
                    entry.entry_id.to_string(),
                    turn_id.to_string(),
                    entry.source_key,
                    serde_json::to_string(&entry).unwrap(),
                    to_i64(sequence).unwrap(),
                ],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE sessions SET last_seq=502,updated_at_ms=502 WHERE session_id=?1",
            [session_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        let transcript = &sessions[0].transcript;
        assert_eq!(
            transcript.entries.len(),
            SESSION_PROJECTION_TRANSCRIPT_LIMIT
        );
        assert!(transcript.has_more, "the bounded tail must be explicit");
        assert_eq!(transcript.entries.first().unwrap().sequence, 3);
        assert_eq!(transcript.entries.last().unwrap().text, "card-502");
        assert!(
            transcript
                .entries
                .iter()
                .all(|entry| entry.text != "oldest prompt"),
            "a truncated current view must not misleadingly start at the oldest card"
        );
    }

    #[test]
    fn session_catalog_reads_bounded_metadata_without_deserializing_transcripts() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        store
            .create_session_v2(
                session_id,
                turn_id,
                &session_binding(),
                "/workspace/catalog",
                "First session title\nwith more context",
                &std::collections::BTreeMap::new(),
                42,
            )
            .unwrap();
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE conversation_outbox SET entry_json='{' WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
        }

        let catalog = store
            .list_session_summaries_for_workspace("/workspace/catalog", 1)
            .unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].session_id, session_id);
        assert_eq!(catalog[0].title, "First session title");
        assert_eq!(catalog[0].workspace_root, "/workspace/catalog");
        assert_eq!(catalog[0].model, "model");
        assert_eq!(catalog[0].created_at_ms, 42);
        assert_eq!(catalog[0].updated_at_ms, 42);
        assert_eq!(store.session_v2(session_id).unwrap().unwrap(), catalog[0]);
        assert_eq!(
            store
                .list_session_summaries_for_workspace("/workspace/catalog", 10)
                .unwrap(),
            catalog
        );
        assert!(
            store
                .search_sessions("/workspace/catalog", "title", 0,)
                .unwrap()
                .is_empty()
        );
        assert!(store.rename_session(session_id, "  ").is_err());
        let missing = SessionId::from_uuid(ids.next_uuid_v7());
        assert!(store.rename_session(missing, "missing").is_err());
    }

    /// The rename title is user-supplied and reaches the durable record
    /// raw — unlike the create prompt, which the caller redacts before the
    /// title is derived. Mutation anchor: removing the redaction folded
    /// into `session_title` leaves the secret readable in the stored title.
    #[test]
    fn rename_session_redacts_the_durable_title() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        store
            .create_session_v2(
                session_id,
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                "/workspace/rename",
                "first prompt",
                &std::collections::BTreeMap::new(),
                42,
            )
            .unwrap();
        let secret = "sk-live-fedcba9876543210";
        store
            .rename_session(session_id, &format!("investigate {secret} leak"))
            .unwrap();
        let title = store.session_v2(session_id).unwrap().unwrap().title;
        assert!(!title.contains(secret), "{title}");
        assert!(title.contains("[REDACTED]"), "{title}");
    }

    /// Forked history is replayed to the provider, so the copy re-applies
    /// redaction to text and payload: entries persisted by older binaries —
    /// before any redaction hardening — must not survive into the new
    /// session unredacted. Mutation anchor: deleting the copy-time
    /// redaction leaves the secret readable in both fields of the forked
    /// entry.
    #[test]
    fn create_session_fork_redacts_copied_history() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let source = SessionId::from_uuid(ids.next_uuid_v7());
        store
            .create_session_v2(
                source,
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                "/workspace/fork",
                "first prompt",
                &std::collections::BTreeMap::new(),
                42,
            )
            .unwrap();
        let secret = "sk-live-9988776655443322";
        let history = vec![TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
            sequence: 1,
            turn_id: None,
            kind: TranscriptKind::Assistant,
            text: format!("summary with api_key={secret} inside"),
            payload: Some(serde_json::json!({"note": format!("bearer {secret}")})),
            source_key: "assistant-final".into(),
            created_at_ms: 42,
        }];
        let fork = SessionId::from_uuid(ids.next_uuid_v7());
        store
            .create_session_fork(source, fork, &history, None, 43)
            .unwrap();
        let forked = store
            .list_sessions_for_workspace("/workspace/fork")
            .unwrap();
        let summary = forked
            .iter()
            .find(|session| session.session_id == fork)
            .expect("the fork is listed under the source workspace");
        let entry = &summary.transcript.entries[0];
        assert!(!entry.text.contains(secret), "{}", entry.text);
        assert!(entry.text.contains("[REDACTED]"), "{}", entry.text);
        let payload = serde_json::to_string(entry.payload.as_ref().unwrap()).unwrap();
        assert!(!payload.contains(secret), "{payload}");
        assert!(payload.contains("[REDACTED]"), "{payload}");
    }

    #[test]
    fn workspace_scoped_session_projection_never_matches_an_identical_foreign_prompt() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let local_session = SessionId::from_uuid(ids.next_uuid_v7());
        let foreign_session = SessionId::from_uuid(ids.next_uuid_v7());
        for (session_id, workspace_root, now_ms) in [
            (local_session, "/workspace/local", 1),
            (foreign_session, "/workspace/foreign", 2),
        ] {
            store
                .create_session_v2(
                    session_id,
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    &session_binding(),
                    workspace_root,
                    "identical prompt",
                    &std::collections::BTreeMap::new(),
                    now_ms,
                )
                .unwrap();
        }

        let local = store
            .list_sessions_for_workspace("/workspace/local")
            .unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].session_id, local_session);
        assert_eq!(local[0].transcript.entries[0].text, "identical prompt");
        assert_eq!(
            store
                .list_session_summaries_for_workspace("/workspace/local", 10)
                .unwrap()
                .into_iter()
                .map(|session| session.session_id)
                .collect::<Vec<_>>(),
            vec![local_session]
        );
    }

    #[test]
    fn session_cursor_pagination_pages_through_newest_first() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let workspace = "/workspace/paged";
        let mut created = Vec::new();
        for index in 0..5u64 {
            let session_id = SessionId::from_uuid(ids.next_uuid_v7());
            store
                .create_session_v2(
                    session_id,
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    &session_binding(),
                    workspace,
                    &format!("session {index}"),
                    &std::collections::BTreeMap::new(),
                    1_000 + index,
                )
                .unwrap();
            created.push(session_id);
        }

        // First page: newest two.
        let page1 = store
            .list_sessions_for_workspace_paged(workspace, None, 2)
            .unwrap();
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items[0].session_id, created[4]);
        assert_eq!(page1.items[1].session_id, created[3]);
        let cursor = page1.next_cursor.expect("more pages exist");

        // Second page: next two, continuing from the cursor.
        let page2 = store
            .list_sessions_for_workspace_paged(workspace, Some(&cursor), 2)
            .unwrap();
        assert_eq!(page2.items.len(), 2);
        assert_eq!(page2.items[0].session_id, created[2]);
        assert_eq!(page2.items[1].session_id, created[1]);
        let cursor = page2.next_cursor.expect("more pages exist");

        // Final page: one item, no further cursor.
        let page3 = store
            .list_sessions_for_workspace_paged(workspace, Some(&cursor), 2)
            .unwrap();
        assert_eq!(page3.items.len(), 1);
        assert_eq!(page3.items[0].session_id, created[0]);
        assert!(page3.next_cursor.is_none());

        // The cursor is opaque: clients cannot infer positions from it.
        assert!(!cursor.contains("session"));
    }

    #[test]
    fn session_cursor_pagination_excludes_foreign_workspace() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        for (workspace, now_ms) in [("/workspace/local", 1u64), ("/workspace/foreign", 2)] {
            store
                .create_session_v2(
                    SessionId::from_uuid(ids.next_uuid_v7()),
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    &session_binding(),
                    workspace,
                    "identical prompt",
                    &std::collections::BTreeMap::new(),
                    now_ms,
                )
                .unwrap();
        }

        let page = store
            .list_sessions_for_workspace_paged("/workspace/local", None, 50)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.next_cursor.is_none());
        assert_eq!(page.items[0].transcript.entries[0].text, "identical prompt");
    }

    #[test]
    fn session_cursor_pagination_limit_zero_is_empty_page() {
        let store = Storage::memory().unwrap();
        let page = store
            .list_sessions_for_workspace_paged("/workspace/local", None, 0)
            .unwrap();
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
        assert!(
            store
                .search_sessions_paged("/workspace/local", "anything", None, 0)
                .unwrap()
                .items
                .is_empty()
        );
        assert!(
            store
                .find_sessions_by_exact_title_for_workspace_paged(
                    "/workspace/local",
                    "anything",
                    None,
                    0
                )
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn session_cursor_pagination_rejects_invalid_cursor() {
        let store = Storage::memory().unwrap();
        for cursor in ["", "v1_", "v1_xyz", "v1_1:2:3", "garbage", "v2_1:2"] {
            let result =
                store.list_sessions_for_workspace_paged("/workspace/local", Some(cursor), 10);
            assert!(
                matches!(result, Err(StorageError::InvalidData(message)) if message.contains("invalid session cursor")),
                "cursor {cursor:?} should be rejected"
            );
        }
    }

    #[test]
    fn search_and_exact_title_paged_filters_and_pages() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let workspace = "/workspace/search";
        let mut matching = Vec::new();
        for index in 0..4u64 {
            let session_id = SessionId::from_uuid(ids.next_uuid_v7());
            store
                .create_session_v2(
                    session_id,
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    &session_binding(),
                    workspace,
                    &format!("matching session {index}"),
                    &std::collections::BTreeMap::new(),
                    100 + index,
                )
                .unwrap();
            matching.push(session_id);
        }
        // A non-matching session and a foreign-workspace exact-title decoy.
        store
            .create_session_v2(
                SessionId::from_uuid(ids.next_uuid_v7()),
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                workspace,
                "unrelated",
                &std::collections::BTreeMap::new(),
                500,
            )
            .unwrap();
        store
            .create_session_v2(
                SessionId::from_uuid(ids.next_uuid_v7()),
                TurnId::from_uuid(ids.next_uuid_v7()),
                &session_binding(),
                "/workspace/foreign",
                "matching session 0",
                &std::collections::BTreeMap::new(),
                600,
            )
            .unwrap();

        // Substring search pages through matches only, newest first.
        let page1 = store
            .search_sessions_paged(workspace, "matching", None, 2)
            .unwrap();
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items[0].session_id, matching[3]);
        let page2 = store
            .search_sessions_paged(workspace, "matching", page1.next_cursor.as_deref(), 2)
            .unwrap();
        assert_eq!(page2.items.len(), 2);
        assert_eq!(page2.items[0].session_id, matching[1]);
        assert!(page2.next_cursor.is_none());

        // Exact-title lookup returns only exact matches in this workspace.
        let exact = store
            .find_sessions_by_exact_title_for_workspace_paged(
                workspace,
                "matching session 0",
                None,
                10,
            )
            .unwrap();
        assert_eq!(exact.items.len(), 1);
        assert_eq!(exact.items[0].session_id, matching[0]);
        assert!(exact.next_cursor.is_none());
    }

    #[test]
    fn exact_title_paged_pages_through_multiple_matches() {
        use latte_core::{SessionId, SystemIdSource, TurnId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let workspace = "/workspace/exact";
        let mut matching = Vec::new();
        for index in 0..4u64 {
            let session_id = SessionId::from_uuid(ids.next_uuid_v7());
            store
                .create_session_v2(
                    session_id,
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    &session_binding(),
                    workspace,
                    "shared title",
                    &std::collections::BTreeMap::new(),
                    100 + index,
                )
                .unwrap();
            matching.push(session_id);
        }

        // Page 1: two matches + cursor.
        let page1 = store
            .find_sessions_by_exact_title_for_workspace_paged(workspace, "shared title", None, 2)
            .unwrap();
        assert_eq!(page1.items.len(), 2);
        assert_eq!(page1.items[0].session_id, matching[3]);
        let cursor = page1.next_cursor.expect("cursor present");

        // Page 2: remaining two matches, no further cursor.
        let page2 = store
            .find_sessions_by_exact_title_for_workspace_paged(
                workspace,
                "shared title",
                Some(&cursor),
                2,
            )
            .unwrap();
        assert_eq!(page2.items.len(), 2);
        assert_eq!(page2.items[0].session_id, matching[1]);
        assert!(page2.next_cursor.is_none());
    }

    #[test]
    fn scalar_status_and_lifecycle_conversions_are_total_and_fail_closed() {
        assert_eq!(to_i64(i64::MAX as u64).unwrap(), i64::MAX);
        assert!(matches!(
            to_i64(u64::MAX),
            Err(StorageError::InvalidData(message)) if message.contains("sqlite range")
        ));
        assert_eq!(from_i64(0).unwrap(), 0);
        assert!(matches!(
            from_i64(-1),
            Err(StorageError::InvalidData(message)) if message.contains("negative sqlite integer")
        ));

        for (status, stored, projected) in [
            (TurnStatus::Queued, "queued", SessionTurnStatus::Queued),
            (TurnStatus::Running, "running", SessionTurnStatus::Running),
            (
                TurnStatus::WaitingPermission,
                "waiting_permission",
                SessionTurnStatus::WaitingPermission,
            ),
            (
                TurnStatus::WaitingInput,
                "waiting_input",
                SessionTurnStatus::WaitingInput,
            ),
            (
                TurnStatus::Cancelling,
                "cancelling",
                SessionTurnStatus::Cancelling,
            ),
            (
                TurnStatus::Interrupted,
                "interrupted",
                SessionTurnStatus::Interrupted,
            ),
            (TurnStatus::Failed, "failed", SessionTurnStatus::Failed),
            (
                TurnStatus::Completed,
                "completed",
                SessionTurnStatus::Completed,
            ),
        ] {
            assert_eq!(status_name(status), stored);
            assert_eq!(session_turn_status(status), projected);
        }

        for (stored, lifecycle) in [
            ("ready", SessionLifecycle::Ready),
            ("running", SessionLifecycle::Running),
            ("waiting_permission", SessionLifecycle::WaitingPermission),
            ("waiting_input", SessionLifecycle::WaitingInput),
            ("interrupted", SessionLifecycle::Interrupted),
            ("failed", SessionLifecycle::Failed),
            (
                "reconciliation_required",
                SessionLifecycle::ReconciliationRequired,
            ),
        ] {
            assert_eq!(parse_lifecycle(stored).unwrap(), lifecycle);
        }
        assert!(matches!(
            parse_lifecycle("completed"),
            Err(StorageError::InvalidData(message)) if message.contains("invalid session lifecycle")
        ));

        let id = SystemIdSource::default().next_uuid_v7();
        assert_eq!(
            parse_session_id(&id.to_string()).unwrap().to_string(),
            id.to_string()
        );
        assert!(matches!(
            parse_session_id("not-a-uuid"),
            Err(StorageError::InvalidData(message)) if message.contains("invalid stored session id")
        ));

        for (kind, stored) in [
            (TranscriptKind::User, "user"),
            (TranscriptKind::Assistant, "assistant"),
            (TranscriptKind::ToolCall, "tool_call"),
            (TranscriptKind::ToolResult, "tool_result"),
            (TranscriptKind::Permission, "permission"),
            (TranscriptKind::Input, "input"),
            (TranscriptKind::Failure, "failure"),
            (TranscriptKind::Completion, "completion"),
            (TranscriptKind::System, "system"),
        ] {
            assert_eq!(transcript_kind_name(kind), stored);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn session_command_digest_covers_every_variant_and_excludes_private_descriptor_secrets() {
        use latte_core::{PendingInput, PendingPermission, SessionCommandId, SessionId};
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let command_id = SessionCommandId::from_uuid(ids.next_uuid_v7());
        let source_key = "source".to_owned();
        let updates = vec![
            CommitSessionTurnUpdate::Start {
                source_key: source_key.clone(),
            },
            CommitSessionTurnUpdate::AppendTranscript {
                source_key: source_key.clone(),
                kind: TranscriptKind::Assistant,
                text: "assistant text".into(),
                payload: Some(serde_json::json!({"value":"payload"})),
            },
            CommitSessionTurnUpdate::PrepareEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                descriptor_json: r#"{"name":"write_file"}"#.into(),
                canonical_descriptor_json: r#"{"api_key":"sk-private-secret-123456789"}"#.into(),
                policy: SessionEffectPolicy::Ask,
                description: "prepare".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
            CommitSessionTurnUpdate::StartEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                checkpoint_json: r#"{"phase":"started"}"#.into(),
            },
            CommitSessionTurnUpdate::ObserveEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                success: true,
                result: "observed".into(),
                payload: Some(serde_json::json!({"result":"ok"})),
                checkpoint_json: r#"{"phase":"observed"}"#.into(),
            },
            CommitSessionTurnUpdate::UnknownEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                checkpoint_json: r#"{"phase":"unknown"}"#.into(),
            },
            CommitSessionTurnUpdate::ReconcileUnknownEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                checkpoint_json: r#"{"phase":"reconciled"}"#.into(),
            },
            CommitSessionTurnUpdate::RequestPermission {
                source_key: source_key.clone(),
                request: PendingPermission {
                    request_id: "permission".into(),
                    operation_digest: "b".repeat(64),
                    description: "allow write".into(),
                },
            },
            CommitSessionTurnUpdate::ResolvePermission {
                source_key: source_key.clone(),
                request_id: "permission".into(),
                allow: true,
                rebound_operation_digest: None,
            },
            CommitSessionTurnUpdate::RequestInput {
                source_key: source_key.clone(),
                request: PendingInput {
                    request_id: "input".into(),
                    prompt: "value?".into(),
                },
            },
            CommitSessionTurnUpdate::ProvideInput {
                source_key: source_key.clone(),
                request_id: "input".into(),
                value: "answer".into(),
            },
            CommitSessionTurnUpdate::Complete {
                source_key: source_key.clone(),
                handoff: Handoff {
                    summary: "done".into(),
                    files_changed: vec!["a.txt".into()],
                    evidence: vec![Evidence {
                        name: "test".into(),
                        status: VerificationStatus::Passed,
                        summary: "passed".into(),
                    }],
                },
            },
            CommitSessionTurnUpdate::CompleteVerified {
                source_key: source_key.clone(),
                summary: "verified".into(),
                verification_effect_id: "verification".into(),
                verified_manifest_digest: "c".repeat(64),
                files_changed: vec!["a.txt".into()],
            },
            CommitSessionTurnUpdate::Fail {
                source_key: source_key.clone(),
                failure: TurnFailure {
                    code: FailureCode::RuntimeFailed,
                    message: "failed".into(),
                    retryability: Retryability::Terminal,
                },
            },
            CommitSessionTurnUpdate::Interrupt {
                source_key: source_key.clone(),
                reconciliation_effect_id: Some("effect".into()),
            },
        ];

        let mut digests = std::collections::BTreeSet::new();
        for update in &updates {
            assert_eq!(update.source_key(), source_key);
            let request = SessionCommitRequest {
                session_id,
                turn_id,
                expected_session_revision: 2,
                expected_turn_revision: 3,
                command_id,
                request_id: Some("request".into()),
                effect_id: Some("effect".into()),
                update: update.clone(),
            };
            let digest = session_command_digest(&request).unwrap();
            assert_eq!(digest.len(), 64);
            assert!(
                digests.insert(digest),
                "variant digest collision: {update:?}"
            );
        }

        let CommitSessionTurnUpdate::PrepareEffect { .. } = &updates[2] else {
            unreachable!()
        };
        let mut changed_private = updates[2].clone();
        let CommitSessionTurnUpdate::PrepareEffect {
            canonical_descriptor_json,
            ..
        } = &mut changed_private
        else {
            unreachable!()
        };
        *canonical_descriptor_json = r#"{"api_key":"sk-a-different-private-secret"}"#.into();
        let request = |update| SessionCommitRequest {
            session_id,
            turn_id,
            expected_session_revision: 2,
            expected_turn_revision: 3,
            command_id,
            request_id: Some("request".into()),
            effect_id: Some("effect".into()),
            update,
        };
        assert_eq!(
            session_command_digest(&request(updates[2].clone())).unwrap(),
            session_command_digest(&request(changed_private)).unwrap(),
            "engine-private canonical content must not enter the replay digest"
        );
    }

    #[test]
    fn session_identifiers_sources_and_redaction_helpers_reject_unsafe_durable_values() {
        use latte_core::{PendingInput, PendingPermission};
        for invalid in ["", "line\nbreak"] {
            assert!(validate_session_source(invalid).is_err());
            assert!(validate_session_effect_id(invalid).is_err());
        }
        assert!(validate_session_source(&"s".repeat(257)).is_err());
        assert!(validate_session_effect_id(&"e".repeat(513)).is_err());
        validate_session_source(&"s".repeat(256)).unwrap();
        validate_session_effect_id(&"e".repeat(512)).unwrap();

        for invalid in ["a".repeat(63), "g".repeat(64), "a".repeat(65)] {
            assert!(validate_session_digest(&invalid).is_err());
        }
        validate_session_digest(&"aB09".repeat(16)).unwrap();

        let secret = "sk-this-is-a-secret-123456789";
        let permission = redact_permission(&PendingPermission {
            request_id: secret.into(),
            operation_digest: secret.into(),
            description: secret.into(),
        });
        let input = redact_input(&PendingInput {
            request_id: secret.into(),
            prompt: secret.into(),
        });
        let failure = redact_failure(&TurnFailure {
            code: FailureCode::RuntimeFailed,
            message: secret.into(),
            retryability: Retryability::Retryable,
        });
        let handoff = redact_handoff(&Handoff {
            summary: secret.into(),
            files_changed: vec![secret.into()],
            evidence: vec![Evidence {
                name: secret.into(),
                status: VerificationStatus::Failed,
                summary: secret.into(),
            }],
        });
        let durable = serde_json::to_string(&(permission, input, failure, handoff)).unwrap();
        assert!(!durable.contains(secret));
        assert!(durable.contains("[REDACTED]"));
    }

    fn session_binding() -> SessionProviderBinding {
        SessionProviderBinding {
            version: 1,
            provider_name: "provider".into(),
            provider_type: "openai-chat".into(),
            protocol: "chat".into(),
            model: "model".into(),
            config_fingerprint: "config".into(),
            tools_fingerprint: "tools".into(),
            aliases: std::collections::BTreeMap::new(),
            credential_ref_id: "env:PROVIDER_KEY".into(),
            data_scope_id: "workspace".into(),
            credential_generation: 1,
        }
    }

    fn create_linked_fixture(
        store: &Storage,
        ids: &SystemIdSource,
        prompt: &str,
        now_ms: u64,
    ) -> (latte_core::SessionId, TurnId, SessionSnapshot) {
        let session_id = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let snapshot = store
            .create_session_v2(
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                prompt,
                &std::collections::BTreeMap::new(),
                now_ms,
            )
            .unwrap();
        (session_id, turn_id, snapshot)
    }

    fn commit_linked(
        store: &Storage,
        ids: &SystemIdSource,
        lease: &Lease,
        snapshot: &SessionSnapshot,
        turn_id: TurnId,
        update: CommitSessionTurnUpdate,
        now_ms: u64,
    ) -> SessionCommitResponse {
        let turn_revision = snapshot
            .turns
            .iter()
            .find(|run| run.turn_id == turn_id)
            .unwrap()
            .turn_revision;
        store
            .commit_session_turn_update(
                &SessionCommitRequest {
                    session_id: snapshot.session_id,
                    turn_id,
                    expected_session_revision: snapshot.revision,
                    expected_turn_revision: turn_revision,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update,
                },
                lease,
                now_ms,
            )
            .unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn queue_audit_append_is_exempt_only_for_the_latest_terminal_turn() {
        let (_dir, path) = db();
        let ids = SystemIdSource::default();
        let store = Storage::open(&path).unwrap();
        // A failed turn clears its active row; the queue-audit exemption must
        // let exactly one AppendTranscript class land on that just-terminalized
        // latest turn — and nothing else, nowhere else (issue #22).
        let (session_id, turn_id, queued) = create_linked_fixture(&store, &ids, "queue audit", 10);
        let lease = store.acquire_session_lease(session_id, 11, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &queued,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "audit:start".into(),
            },
            12,
        )
        .snapshot;
        let failed = commit_linked(
            &store,
            &ids,
            &lease,
            &running,
            turn_id,
            CommitSessionTurnUpdate::Fail {
                source_key: "audit:fail".into(),
                failure: TurnFailure {
                    code: FailureCode::RuntimeFailed,
                    message: "provider failed".into(),
                    retryability: Retryability::Terminal,
                },
            },
            13,
        )
        .snapshot;
        assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
        let audited = commit_linked(
            &store,
            &ids,
            &lease,
            &failed,
            turn_id,
            CommitSessionTurnUpdate::AppendTranscript {
                source_key: format!("{turn_id}:queue-audit"),
                kind: TranscriptKind::System,
                text: "1 queued follow-up prompt(s) were discarded".into(),
                payload: None,
            },
            14,
        );
        assert_eq!(audited.snapshot.lifecycle, SessionLifecycle::Failed);
        assert!(
            audited
                .snapshot
                .transcript
                .entries
                .iter()
                .any(|entry| entry.kind == TranscriptKind::System)
        );
        // The exemption stays fenced by the session revision: an append at
        // the stale pre-audit revision is rejected like any other commit.
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: failed.revision,
                    expected_turn_revision: failed.turns[0].turn_revision,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitSessionTurnUpdate::AppendTranscript {
                        source_key: format!("{turn_id}:queue-audit-2"),
                        kind: TranscriptKind::System,
                        text: "stale".into(),
                        payload: None,
                    },
                },
                &lease,
                15,
            ),
            Err(StorageError::StaleSessionRevision { .. })
        ));
        // A ready session's completed turn also cleared the active row, but
        // "ready" is not a terminal lifecycle: the exemption must not leak.
        let (ready_session, ready_turn, ready_queued) =
            create_linked_fixture(&store, &ids, "ready audit negative", 20);
        let ready_lease = store.acquire_session_lease(ready_session, 21, 100).unwrap();
        let ready_running = commit_linked(
            &store,
            &ids,
            &ready_lease,
            &ready_queued,
            ready_turn,
            CommitSessionTurnUpdate::Start {
                source_key: "audit2:start".into(),
            },
            22,
        )
        .snapshot;
        let completed = commit_linked(
            &store,
            &ids,
            &ready_lease,
            &ready_running,
            ready_turn,
            CommitSessionTurnUpdate::Complete {
                source_key: "audit2:complete".into(),
                handoff: latte_core::Handoff {
                    summary: "done".into(),
                    files_changed: Vec::new(),
                    evidence: Vec::new(),
                },
            },
            23,
        )
        .snapshot;
        assert_eq!(completed.lifecycle, SessionLifecycle::Ready);
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id: ready_session,
                    turn_id: ready_turn,
                    expected_session_revision: completed.revision,
                    expected_turn_revision: completed.turns[0].turn_revision,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitSessionTurnUpdate::AppendTranscript {
                        source_key: format!("{ready_turn}:queue-audit"),
                        kind: TranscriptKind::System,
                        text: "must not land on a ready session".into(),
                        payload: None,
                    },
                },
                &ready_lease,
                24,
            ),
            Err(StorageError::SessionActiveTurnMismatch)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn clean_wait_reopen_requires_atomic_permission_rebind_to_the_new_epoch() {
        let (dir, path) = db();
        let ids = SystemIdSource::default();
        let store = Storage::open(&path).unwrap();
        let (session_id, turn_id, queued) =
            create_linked_fixture(&store, &ids, "rebind permission", 10);
        let first = store.acquire_session_lease(session_id, 11, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &first,
            &queued,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "rebind:start".into(),
            },
            12,
        )
        .snapshot;
        let descriptor = crate::SessionEffectDescriptor {
            effect_id: "rebind-effect".into(),
            tool_call_id: "rebind-call".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path":"rebound.txt",
                "content":"rebound",
                "create_intent":true
            }),
            attempt: 1,
        };
        let old_digest = "a".repeat(64);
        let waiting = commit_linked(
            &store,
            &ids,
            &first,
            &running,
            turn_id,
            CommitSessionTurnUpdate::PrepareEffect {
                source_key: "rebind:prepare".into(),
                effect_id: descriptor.effect_id.clone(),
                operation_digest: old_digest.clone(),
                descriptor_json: serde_json::to_string(&descriptor).unwrap(),
                canonical_descriptor_json: serde_json::to_string(&descriptor).unwrap(),
                policy: SessionEffectPolicy::Ask,
                description: "write rebound.txt".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
            13,
        )
        .snapshot;
        assert_eq!(waiting.lifecycle, SessionLifecycle::WaitingPermission);

        store.release_lease(&first).unwrap();
        drop(store);
        let reopened = Storage::open(&path).unwrap();
        let preserved = reopened.session_snapshot_v2(session_id, None, 100).unwrap();
        assert_eq!(preserved.lifecycle, SessionLifecycle::WaitingPermission);
        assert_eq!(preserved.active_turn_id, Some(turn_id));
        let second = reopened.acquire_session_lease(session_id, 14, 100).unwrap();
        assert!(second.fencing_token > first.fencing_token);

        let mut mismatched_descriptor = descriptor.clone();
        mismatched_descriptor.effect_id = "different-effect".into();
        reopened
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE session_effect_canonical SET descriptor_json=?1 WHERE effect_id=?2",
                params![
                    serde_json::to_string(&mismatched_descriptor).unwrap(),
                    descriptor.effect_id
                ],
            )
            .unwrap();
        let engine = crate::EngineBuilder::new()
            .workspace_root(dir.path())
            .database_path(&path)
            .build()
            .unwrap();
        let identifier_error = engine
            .resolve_session_effect_permission(
                session_id,
                turn_id,
                preserved.revision,
                preserved.turns[0].turn_revision,
                descriptor.effect_id.clone(),
                "rebind:bad-identifier".into(),
                true,
                latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                &second,
                15,
            )
            .unwrap_err();
        assert!(
            identifier_error
                .to_string()
                .contains("canonical session effect identifier mismatch")
        );
        reopened
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE session_effect_canonical SET descriptor_json=?1 WHERE effect_id=?2",
                params![
                    serde_json::to_string(&descriptor).unwrap(),
                    descriptor.effect_id
                ],
            )
            .unwrap();
        let overflow = engine
            .resolve_session_effect_permission(
                session_id,
                turn_id,
                preserved.revision,
                u64::MAX,
                descriptor.effect_id.clone(),
                "rebind:overflow".into(),
                true,
                latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                &second,
                15,
            )
            .unwrap_err();
        assert!(overflow.to_string().contains("run revision overflow"));
        drop(engine);

        let resolve = |digest: Option<String>| {
            reopened.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: preserved.revision,
                    expected_turn_revision: preserved.turns[0].turn_revision,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: Some(descriptor.effect_id.clone()),
                    effect_id: Some(descriptor.effect_id.clone()),
                    update: CommitSessionTurnUpdate::ResolvePermission {
                        source_key: "rebind:allow".into(),
                        request_id: descriptor.effect_id.clone(),
                        allow: true,
                        rebound_operation_digest: digest,
                    },
                },
                &second,
                15,
            )
        };
        reopened
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE pending_permissions SET approval_digest=?1 WHERE effect_id=?2",
                params!["c".repeat(64), descriptor.effect_id],
            )
            .unwrap();
        assert!(
            resolve(Some("d".repeat(64)))
                .unwrap_err()
                .to_string()
                .contains("binding is corrupt or stale")
        );
        reopened
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE pending_permissions SET approval_digest=?1 WHERE effect_id=?2",
                params![old_digest, descriptor.effect_id],
            )
            .unwrap();
        assert!(matches!(resolve(None), Err(StorageError::LeaseLost)));
        assert!(matches!(
            resolve(Some("invalid".into())),
            Err(StorageError::InvalidData(_))
        ));
        reopened
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TEMP TRIGGER ignore_effect_digest_rebind \
                 BEFORE UPDATE OF approval_digest ON effects \
                 WHEN OLD.effect_id='rebind-effect' \
                 BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();
        assert!(matches!(
            resolve(Some("e".repeat(64))),
            Err(StorageError::EffectFenced)
        ));
        reopened
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER ignore_effect_digest_rebind;")
            .unwrap();
        assert_eq!(
            reopened
                .session_snapshot_v2(session_id, None, 100)
                .unwrap()
                .lifecycle,
            SessionLifecycle::WaitingPermission
        );

        let rebound_digest = "b".repeat(64);
        let allowed = resolve(Some(rebound_digest.clone())).unwrap().snapshot;
        assert_eq!(allowed.lifecycle, SessionLifecycle::Running);
        assert_eq!(
            reopened
                .session_effect_digest(&descriptor.effect_id)
                .unwrap(),
            rebound_digest
        );
        let started = commit_linked(
            &reopened,
            &ids,
            &second,
            &allowed,
            turn_id,
            CommitSessionTurnUpdate::StartEffect {
                source_key: "rebind:started".into(),
                effect_id: descriptor.effect_id.clone(),
                operation_digest: rebound_digest,
                checkpoint_json: r#"{"phase":"started"}"#.into(),
            },
            16,
        );
        assert_eq!(started.snapshot.lifecycle, SessionLifecycle::Running);
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            EffectStatus::Started
        );

        let (generic_session, generic_turn, generic_queued) =
            create_linked_fixture(&reopened, &ids, "generic permission", 20);
        let generic_lease = reopened
            .acquire_session_lease(generic_session, 20, 100)
            .unwrap();
        let generic_running = commit_linked(
            &reopened,
            &ids,
            &generic_lease,
            &generic_queued,
            generic_turn,
            CommitSessionTurnUpdate::Start {
                source_key: "generic:start".into(),
            },
            21,
        )
        .snapshot;
        let generic_waiting = commit_linked(
            &reopened,
            &ids,
            &generic_lease,
            &generic_running,
            generic_turn,
            CommitSessionTurnUpdate::RequestPermission {
                source_key: "generic:request".into(),
                request: latte_core::PendingPermission {
                    request_id: "generic-permission".into(),
                    operation_digest: "f".repeat(64),
                    description: "generic permission".into(),
                },
            },
            22,
        )
        .snapshot;
        let missing_capability = reopened
            .commit_session_turn_update(
                &SessionCommitRequest {
                    session_id: generic_session,
                    turn_id: generic_turn,
                    expected_session_revision: generic_waiting.revision,
                    expected_turn_revision: generic_waiting.turns[0].turn_revision,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: Some("generic-permission".into()),
                    effect_id: Some("generic-permission".into()),
                    update: CommitSessionTurnUpdate::ResolvePermission {
                        source_key: "generic:allow".into(),
                        request_id: "generic-permission".into(),
                        allow: true,
                        rebound_operation_digest: Some("f".repeat(64)),
                    },
                },
                &generic_lease,
                23,
            )
            .unwrap_err();
        assert!(
            missing_capability
                .to_string()
                .contains("permission capability is missing")
        );
        reopened.release_lease(&generic_lease).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn linked_session_waits_replays_and_terminal_resolutions_are_atomic() {
        use latte_core::{PendingInput, PendingPermission, SessionCommandId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (session_id, turn_id, mut snapshot) =
            create_linked_fixture(&store, &ids, "initial", 11);
        let lease = store.acquire_session_lease(session_id, 10, 10_000).unwrap();

        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "start".into(),
            },
            12,
        )
        .snapshot;
        assert_eq!(snapshot.turns[0].status, SessionTurnStatus::Running);

        let append = SessionCommitRequest {
            session_id,
            turn_id,
            expected_session_revision: snapshot.revision,
            expected_turn_revision: snapshot.turns[0].turn_revision,
            command_id: SessionCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: CommitSessionTurnUpdate::AppendTranscript {
                source_key: "assistant-card".into(),
                kind: TranscriptKind::Assistant,
                text: "safe card".into(),
                payload: Some(serde_json::json!({"status":"ok"})),
            },
        };
        let appended = store
            .commit_session_turn_update(&append, &lease, 13)
            .unwrap();
        // The source ledger is the second durable idempotency key. Simulate a
        // lost command-index row and prove that the source record still
        // returns the exact committed projection without applying twice.
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM session_command_dedup WHERE command_id=?1",
                [append.command_id.to_string()],
            )
            .unwrap();
        let source_replay = append.clone();
        assert_eq!(
            store
                .commit_session_turn_update(&source_replay, &lease, 14)
                .unwrap(),
            appended,
            "the source ledger must replay the exact committed result"
        );
        let mut mismatched_source = source_replay;
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM session_command_dedup WHERE command_id=?1",
                [mismatched_source.command_id.to_string()],
            )
            .unwrap();
        let CommitSessionTurnUpdate::AppendTranscript { text, .. } = &mut mismatched_source.update
        else {
            unreachable!()
        };
        *text = "different card".into();
        assert!(matches!(
            store.commit_session_turn_update(&mismatched_source, &lease, 15),
            Err(StorageError::SessionCommandReplayMismatch)
        ));
        snapshot = appended.snapshot;

        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::RequestInput {
                source_key: "request-input".into(),
                request: PendingInput {
                    request_id: "input-1".into(),
                    prompt: "value?".into(),
                },
            },
            16,
        )
        .snapshot;
        assert!(matches!(
            snapshot.pending,
            Some(SessionPendingRequest::Input { .. })
        ));
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::ProvideInput {
                source_key: "provide-input".into(),
                request_id: "input-1".into(),
                value: "answer".into(),
            },
            17,
        )
        .snapshot;
        assert_eq!(snapshot.lifecycle, SessionLifecycle::Running);

        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::RequestPermission {
                source_key: "request-permission".into(),
                request: PendingPermission {
                    request_id: "permission-1".into(),
                    operation_digest: "a".repeat(64),
                    description: "continue?".into(),
                },
            },
            18,
        )
        .snapshot;
        assert!(matches!(
            snapshot.pending,
            Some(SessionPendingRequest::Permission { .. })
        ));
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::ResolvePermission {
                source_key: "allow-permission".into(),
                request_id: "permission-1".into(),
                allow: true,
                rebound_operation_digest: None,
            },
            19,
        )
        .snapshot;
        let completed = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::Complete {
                source_key: "complete".into(),
                handoff: Handoff {
                    summary: "done".into(),
                    files_changed: vec!["a.txt".into()],
                    evidence: vec![],
                },
            },
            20,
        );
        assert_eq!(completed.snapshot.lifecycle, SessionLifecycle::Ready);
        assert_eq!(completed.snapshot.active_turn_id, None);

        let (denied_session, denied_turn, mut denied) =
            create_linked_fixture(&store, &ids, "deny", 21);
        let denied_lease = store
            .acquire_session_lease(denied_session, 21, 10_000)
            .unwrap();
        denied = commit_linked(
            &store,
            &ids,
            &denied_lease,
            &denied,
            denied_turn,
            CommitSessionTurnUpdate::Start {
                source_key: "deny:start".into(),
            },
            22,
        )
        .snapshot;
        denied = commit_linked(
            &store,
            &ids,
            &denied_lease,
            &denied,
            denied_turn,
            CommitSessionTurnUpdate::RequestPermission {
                source_key: "deny:request".into(),
                request: PendingPermission {
                    request_id: "permission-denied".into(),
                    operation_digest: "b".repeat(64),
                    description: "deny this".into(),
                },
            },
            23,
        )
        .snapshot;
        let denied = commit_linked(
            &store,
            &ids,
            &denied_lease,
            &denied,
            denied_turn,
            CommitSessionTurnUpdate::ResolvePermission {
                source_key: "deny:resolve".into(),
                request_id: "permission-denied".into(),
                allow: false,
                rebound_operation_digest: None,
            },
            24,
        );
        assert_eq!(denied.snapshot.lifecycle, SessionLifecycle::Ready);
        assert!(denied.snapshot.active_turn_id.is_none());
        assert!(denied.snapshot.pending.is_none());
        assert_eq!(
            store.load_turn(denied_turn).unwrap().status,
            TurnStatus::Failed
        );
        let retry_run = TurnId::from_uuid(ids.next_uuid_v7());
        let retry = store
            .create_session_follow_up_v2(
                denied_session,
                retry_run,
                denied.snapshot.revision,
                "continue after denial",
                &std::collections::BTreeMap::new(),
                25,
            )
            .unwrap();
        assert_eq!(retry.lifecycle, SessionLifecycle::Running);
        assert_eq!(retry.active_turn_id, Some(retry_run));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn linked_effect_started_interrupt_requires_exact_reconciliation() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (session_id, turn_id, mut snapshot) =
            create_linked_fixture(&store, &ids, "effect", 101);
        let lease = store
            .acquire_session_lease(session_id, 100, 10_000)
            .unwrap();
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "effect:start-run".into(),
            },
            102,
        )
        .snapshot;

        let effect_id = "effect-1";
        let digest = "c".repeat(64);
        let canonical_descriptor = crate::SessionEffectDescriptor {
            effect_id: effect_id.into(),
            tool_call_id: "provider-call-1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
            attempt: 1,
        };
        let canonical = serde_json::to_string(&canonical_descriptor).unwrap();
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::PrepareEffect {
                source_key: "effect:prepare".into(),
                effect_id: effect_id.into(),
                operation_digest: digest.clone(),
                descriptor_json: r#"{"name":"read_file","input":{"path":"a.txt"}}"#.into(),
                canonical_descriptor_json: canonical.clone(),
                policy: SessionEffectPolicy::Allow,
                description: "read a.txt".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
            103,
        )
        .snapshot;
        assert_eq!(
            store.effect_status(effect_id).unwrap(),
            EffectStatus::Prepared
        );
        assert_eq!(
            store
                .session_effect_canonical_descriptor(effect_id, turn_id)
                .unwrap(),
            canonical_descriptor
        );
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::StartEffect {
                source_key: "effect:start".into(),
                effect_id: effect_id.into(),
                operation_digest: digest,
                checkpoint_json: r#"{"phase":"started"}"#.into(),
            },
            104,
        )
        .snapshot;
        assert_eq!(
            store.effect_status(effect_id).unwrap(),
            EffectStatus::Started
        );

        let interrupted = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::Interrupt {
                source_key: "effect:interrupt".into(),
                reconciliation_effect_id: None,
            },
            105,
        );
        assert_eq!(
            interrupted.snapshot.lifecycle,
            SessionLifecycle::ReconciliationRequired
        );
        assert_eq!(
            store.effect_status(effect_id).unwrap(),
            EffectStatus::Unknown
        );
        assert_eq!(
            store.unknown_effects_for_turn(turn_id).unwrap(),
            vec![effect_id.to_owned()]
        );
        let reconciled = commit_linked(
            &store,
            &ids,
            &lease,
            &interrupted.snapshot,
            turn_id,
            CommitSessionTurnUpdate::ReconcileUnknownEffect {
                source_key: "effect:reconcile".into(),
                effect_id: effect_id.into(),
                checkpoint_json: r#"{"phase":"reconciled"}"#.into(),
            },
            106,
        );
        assert_eq!(reconciled.snapshot.lifecycle, SessionLifecycle::Failed);
        assert_eq!(
            store.effect_status(effect_id).unwrap(),
            EffectStatus::ObservedFailed
        );
        assert!(store.unknown_effects_for_turn(turn_id).unwrap().is_empty());

        for (suffix, success, expected) in [
            ("success", true, EffectStatus::ObservedSuccess),
            ("failure", false, EffectStatus::ObservedFailed),
        ] {
            let (observed_session, observed_turn, mut observed) =
                create_linked_fixture(&store, &ids, suffix, 110);
            let observed_lease = store
                .acquire_session_lease(observed_session, 110, 10_000)
                .unwrap();
            observed = commit_linked(
                &store,
                &ids,
                &observed_lease,
                &observed,
                observed_turn,
                CommitSessionTurnUpdate::Start {
                    source_key: format!("{suffix}:start-run"),
                },
                111,
            )
            .snapshot;
            let observed_effect = format!("effect-{suffix}");
            let observed_digest = if success {
                "d".repeat(64)
            } else {
                "e".repeat(64)
            };
            let observed_canonical = serde_json::to_string(&crate::SessionEffectDescriptor {
                effect_id: observed_effect.clone(),
                tool_call_id: format!("call-{suffix}"),
                name: "read_file".into(),
                input: serde_json::json!({"path":"a.txt"}),
                attempt: 1,
            })
            .unwrap();
            observed = commit_linked(
                &store,
                &ids,
                &observed_lease,
                &observed,
                observed_turn,
                CommitSessionTurnUpdate::PrepareEffect {
                    source_key: format!("{suffix}:prepare"),
                    effect_id: observed_effect.clone(),
                    operation_digest: observed_digest.clone(),
                    descriptor_json: r#"{"name":"read_file"}"#.into(),
                    canonical_descriptor_json: observed_canonical,
                    policy: SessionEffectPolicy::Allow,
                    description: "read".into(),
                    checkpoint_json: r#"{"phase":"prepared"}"#.into(),
                },
                112,
            )
            .snapshot;
            observed = commit_linked(
                &store,
                &ids,
                &observed_lease,
                &observed,
                observed_turn,
                CommitSessionTurnUpdate::StartEffect {
                    source_key: format!("{suffix}:start-effect"),
                    effect_id: observed_effect.clone(),
                    operation_digest: observed_digest.clone(),
                    checkpoint_json: r#"{"phase":"started"}"#.into(),
                },
                113,
            )
            .snapshot;
            let observed = commit_linked(
                &store,
                &ids,
                &observed_lease,
                &observed,
                observed_turn,
                CommitSessionTurnUpdate::ObserveEffect {
                    source_key: format!("{suffix}:observe"),
                    effect_id: observed_effect.clone(),
                    operation_digest: observed_digest,
                    success,
                    result: suffix.into(),
                    payload: Some(serde_json::json!({"case":suffix})),
                    checkpoint_json: r#"{"phase":"observed"}"#.into(),
                },
                114,
            );
            assert_eq!(store.effect_status(&observed_effect).unwrap(), expected);
            assert_eq!(observed.snapshot.lifecycle, SessionLifecycle::Running);
            assert_eq!(
                observed.snapshot.transcript.entries.last().unwrap().kind,
                TranscriptKind::ToolResult
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    #[rustfmt::skip]
    fn linked_creation_and_effect_commit_error_matrix_is_atomic() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let baseline = std::collections::BTreeMap::new();
        let empty_session = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let empty_run = TurnId::from_uuid(ids.next_uuid_v7());
        assert!(
            store
                .create_session_v2(
                    empty_session,
                    empty_run,
                    &session_binding(),
                    "/workspace",
                    " \n ",
                    &baseline,
                    1,
                )
                .unwrap_err()
                .to_string()
                .contains("prompt must not be empty")
        );

        let (session_id, turn_id, queued) = create_linked_fixture(&store, &ids, "initial", 11);
        let lease = store.acquire_session_lease(session_id, 10, 10_000).unwrap();
        let follow_up = TurnId::from_uuid(ids.next_uuid_v7());
        assert!(
            store
                .create_session_follow_up_v2(
                    session_id,
                    follow_up,
                    queued.revision,
                    " ",
                    &baseline,
                    12,
                )
                .unwrap_err()
                .to_string()
                .contains("follow-up must not be empty")
        );
        assert!(matches!(
            store.create_session_follow_up_v2(
                session_id,
                follow_up,
                queued.revision + 1,
                "next",
                &baseline,
                12,
            ),
            Err(StorageError::StaleSessionRevision { .. })
        ));
        assert!(
            store
                .create_session_follow_up_v2(
                    session_id,
                    follow_up,
                    queued.revision,
                    "next",
                    &baseline,
                    12,
                )
                .unwrap_err()
                .to_string()
                .contains("ready session")
        );

        let start = |command_id, session_revision, turn_revision, turn_id| SessionCommitRequest {
            session_id,
            turn_id,
            expected_session_revision: session_revision,
            expected_turn_revision: turn_revision,
            command_id,
            request_id: None,
            effect_id: None,
            update: CommitSessionTurnUpdate::Start {
                source_key: format!("start:{command_id}"),
            },
        };
        let fenced = Lease {
            scope: lease.scope.clone(),
            owner: "fenced".into(),
            fencing_token: lease.fencing_token + 1,
            expires_at_ms: lease.expires_at_ms,
        };
        assert!(matches!(
            store.commit_session_turn_update(
                &start(
                    latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision,
                    0,
                    turn_id,
                ),
                &fenced,
                13,
            ),
            Err(StorageError::LeaseLost)
        ));
        assert!(matches!(
            store.commit_session_turn_update(
                &start(
                    latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision + 1,
                    0,
                    turn_id,
                ),
                &lease,
                13,
            ),
            Err(StorageError::StaleSessionRevision { .. })
        ));
        let other_turn = TurnId::from_uuid(ids.next_uuid_v7());
        assert!(matches!(
            store.commit_session_turn_update(
                &start(
                    latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision,
                    0,
                    other_turn,
                ),
                &lease,
                13,
            ),
            Err(StorageError::SessionActiveTurnMismatch)
        ));
        assert!(matches!(
            store.commit_session_turn_update(
                &start(
                    latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision,
                    1,
                    turn_id,
                ),
                &lease,
                13,
            ),
            Err(StorageError::StaleRevision { .. })
        ));

        let canonical = crate::SessionEffectDescriptor {
            effect_id: "effect-matrix".into(),
            tool_call_id: "call_matrix".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
            attempt: 1,
        };
        let prepare = |snapshot: &SessionSnapshot,
                       command_id: latte_core::SessionCommandId,
                       source: &str| SessionCommitRequest {
            session_id,
            turn_id,
            expected_session_revision: snapshot.revision,
            expected_turn_revision: snapshot.turns[0].turn_revision,
            command_id,
            request_id: None,
            effect_id: Some("effect-matrix".into()),
            update: CommitSessionTurnUpdate::PrepareEffect {
                source_key: source.into(),
                effect_id: "effect-matrix".into(),
                operation_digest: "a".repeat(64),
                descriptor_json: r#"{"name":"read_file","input":{"path":"a.txt"}}"#.into(),
                canonical_descriptor_json: serde_json::to_string(&canonical).unwrap(),
                policy: SessionEffectPolicy::Allow,
                description: "read a.txt".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
        };
        assert!(
            store
                .commit_session_turn_update(
                    &prepare(
                        &queued,
                        latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        "prepare-queued",
                    ),
                    &lease,
                    14,
                )
                .unwrap_err()
                .to_string()
                .contains("only a running linked child")
        );
        let mut running = commit_linked(
            &store,
            &ids,
            &lease,
            &queued,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "matrix:start".into(),
            },
            15,
        )
        .snapshot;
        assert!(
            store
                .commit_session_turn_update(
                    &SessionCommitRequest {
                        session_id,
                        turn_id,
                        expected_session_revision: running.revision,
                        expected_turn_revision: running.turns[0].turn_revision,
                        command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: Some("missing-effect".into()),
                        update: CommitSessionTurnUpdate::StartEffect {
                            source_key: "missing:start".into(),
                            effect_id: "missing-effect".into(),
                            operation_digest: "b".repeat(64),
                            checkpoint_json: "{}".into(),
                        },
                    },
                    &lease,
                    16,
                )
                .unwrap_err()
                .to_string()
                .contains("not a prepared linked effect")
        );
        running = store
            .commit_session_turn_update(
                &prepare(
                    &running,
                    latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    "matrix:prepare",
                ),
                &lease,
                17,
            )
            .unwrap()
            .snapshot;
        let start_effect =
            |snapshot: &SessionSnapshot, digest: String, source: &str| SessionCommitRequest {
                session_id,
                turn_id,
                expected_session_revision: snapshot.revision,
                expected_turn_revision: snapshot.turns[0].turn_revision,
                command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: Some("effect-matrix".into()),
                effect_id: Some("effect-matrix".into()),
                update: CommitSessionTurnUpdate::StartEffect {
                    source_key: source.into(),
                    effect_id: "effect-matrix".into(),
                    operation_digest: digest,
                    checkpoint_json: r#"{"phase":"started"}"#.into(),
                },
            };
        assert!(
            store
                .commit_session_turn_update(
                    &start_effect(&running, "b".repeat(64), "matrix:wrong-digest"),
                    &lease,
                    18,
                )
                .unwrap_err()
                .to_string()
                .contains("digest mismatch")
        );
        running = store
            .commit_session_turn_update(
                &start_effect(&running, "a".repeat(64), "matrix:start-effect"),
                &lease,
                19,
            )
            .unwrap()
            .snapshot;
        let observe = |snapshot: &SessionSnapshot, effect: &str, digest: String, source: &str| {
            SessionCommitRequest {
                session_id,
                turn_id,
                expected_session_revision: snapshot.revision,
                expected_turn_revision: snapshot.turns[0].turn_revision,
                command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: Some(effect.into()),
                effect_id: Some(effect.into()),
                update: CommitSessionTurnUpdate::ObserveEffect {
                    source_key: source.into(),
                    effect_id: effect.into(),
                    operation_digest: digest,
                    success: true,
                    result: "ok".into(),
                    payload: Some(serde_json::json!({"safe":true})),
                    checkpoint_json: r#"{"phase":"observed"}"#.into(),
                },
            }
        };
        assert!(matches!(
            store.commit_session_turn_update(
                &observe(
                    &running,
                    "effect-matrix",
                    "b".repeat(64),
                    "matrix:observe-wrong",
                ),
                &lease,
                20,
            ),
            Err(StorageError::EffectFenced)
        ));
        let observed = store
            .commit_session_turn_update(
                &observe(&running, "effect-matrix", "a".repeat(64), "matrix:observe"),
                &lease,
                21,
            )
            .unwrap();
        assert_eq!(
            store.effect_status("effect-matrix").unwrap(),
            EffectStatus::ObservedSuccess
        );
        assert!(matches!(
            store.commit_session_turn_update(
                &observe(
                    &observed.snapshot,
                    "effect-matrix",
                    "a".repeat(64),
                    "matrix:observe-twice",
                ),
                &lease,
                22,
            ),
            Err(StorageError::EffectFenced)
        ));
        assert!(matches!(
            store.session_snapshot_v2(
                latte_core::SessionId::from_uuid(ids.next_uuid_v7()),
                None,
                10,
            ),
            Err(StorageError::SessionNotFound(_))
        ));
        assert!(
            store
                .list_sessions()
                .unwrap()
                .iter()
                .any(|s| s.session_id == session_id)
        );
        let boundary = |snapshot: &SessionSnapshot, update: CommitSessionTurnUpdate| store.commit_session_turn_update(&SessionCommitRequest { session_id, turn_id, expected_session_revision: snapshot.revision, expected_turn_revision: snapshot.turns[0].turn_revision, command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()), request_id: None, effect_id: None, update }, &lease, 23); let running_state = store.load_turn(turn_id).unwrap(); let mut failed_state = running_state.clone(); failed_state.status = TurnStatus::Failed; store.connection.lock().unwrap().execute("UPDATE turns SET state_json=?1 WHERE turn_id=?2", params![serde_json::to_string(&failed_state).unwrap(), turn_id.to_string()]).unwrap(); assert!(boundary(&observed.snapshot, CommitSessionTurnUpdate::ObserveEffect { source_key: "matrix:observe-non-running".into(), effect_id: "effect-matrix".into(), operation_digest: "a".repeat(64), success: true, result: "late".into(), payload: None, checkpoint_json: "{}".into() }).unwrap_err().to_string().contains("requires a running linked child")); store.connection.lock().unwrap().execute("UPDATE turns SET state_json=?1 WHERE turn_id=?2", params![serde_json::to_string(&running_state).unwrap(), turn_id.to_string()]).unwrap(); assert!(boundary(&observed.snapshot, CommitSessionTurnUpdate::CompleteVerified { source_key: "matrix:verify-without-evidence".into(), summary: "not verified".into(), verification_effect_id: "missing-verification".into(), verified_manifest_digest: "missing-manifest".into(), files_changed: vec![] }).is_err()); assert!(matches!(boundary(&observed.snapshot, CommitSessionTurnUpdate::UnknownEffect { source_key: "matrix:unknown-missing".into(), effect_id: "missing-effect".into(), operation_digest: "a".repeat(64), checkpoint_json: "{}".into() }), Err(StorageError::EffectFenced))); store.connection.lock().unwrap().execute("UPDATE effects SET status='unknown' WHERE effect_id='effect-matrix'", []).unwrap(); assert_eq!(boundary(&observed.snapshot, CommitSessionTurnUpdate::ReconcileUnknownEffect { source_key: "matrix:reconcile-running".into(), effect_id: "effect-matrix".into(), checkpoint_json: "{}".into() }).unwrap().snapshot.lifecycle, SessionLifecycle::Failed); let (ask_session, ask_turn, ask) = create_linked_fixture(&store, &ids, "ask", 30); let ask_lease = store.acquire_session_lease(ask_session, 30, 10_000).unwrap(); let ask = commit_linked(&store, &ids, &ask_lease, &ask, ask_turn, CommitSessionTurnUpdate::Start { source_key: "ask:start".into() }, 31).snapshot; let ask_digest = "d".repeat(64); let ask_descriptor = crate::SessionEffectDescriptor { effect_id: "ask-matrix".into(), tool_call_id: "ask-call".into(), name: "read_file".into(), input: serde_json::json!({"path":"a.txt"}), attempt: 1 };
        let ask = commit_linked(&store, &ids, &ask_lease, &ask, ask_turn, CommitSessionTurnUpdate::PrepareEffect { source_key: "ask:prepare".into(), effect_id: "ask-matrix".into(), operation_digest: ask_digest.clone(), descriptor_json: "{}".into(), canonical_descriptor_json: serde_json::to_string(&ask_descriptor).unwrap(), policy: SessionEffectPolicy::Ask, description: "ask".into(), checkpoint_json: "{}".into() }, 32).snapshot;
        let start_ask = |snapshot: &SessionSnapshot, source: &str| store.commit_session_turn_update(&SessionCommitRequest { session_id: ask_session, turn_id: ask_turn, expected_session_revision: snapshot.revision, expected_turn_revision: snapshot.turns[0].turn_revision, command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()), request_id: None, effect_id: Some("ask-matrix".into()), update: CommitSessionTurnUpdate::StartEffect { source_key: source.into(), effect_id: "ask-matrix".into(), operation_digest: ask_digest.clone(), checkpoint_json: "{}".into() } }, &ask_lease, 33);
        assert!(start_ask(&ask, "ask:while-pending").unwrap_err().to_string().contains("without a pending request"));
        let allowed = commit_linked(&store, &ids, &ask_lease, &ask, ask_turn, CommitSessionTurnUpdate::ResolvePermission { source_key: "ask:allow".into(), request_id: "ask-matrix".into(), allow: true, rebound_operation_digest: None }, 34).snapshot;
        store.connection.lock().unwrap().execute("UPDATE pending_permissions SET turn_revision=999 WHERE effect_id='ask-matrix'", []).unwrap(); assert!(start_ask(&allowed, "ask:stale").unwrap_err().to_string().contains("stale, mismatched, or consumed"));
        store.connection.lock().unwrap().execute("DELETE FROM pending_permissions WHERE effect_id='ask-matrix'", []).unwrap(); assert!(start_ask(&allowed, "ask:missing-auth").unwrap_err().to_string().contains("no durable allow authorization")); { let conn = store.connection.lock().unwrap(); conn.execute("UPDATE effects SET status='unknown' WHERE effect_id='ask-matrix'", []).unwrap(); conn.execute("DELETE FROM session_active_turns WHERE session_id=?1", [ask_session.to_string()]).unwrap(); conn.execute("UPDATE sessions SET lifecycle='reconciliation_required',latest_turn_id=?1 WHERE session_id=?2", params![ask_turn.to_string(), ask_session.to_string()]).unwrap(); } assert!(store.commit_session_turn_update(&SessionCommitRequest { session_id: ask_session, turn_id: ask_turn, expected_session_revision: allowed.revision, expected_turn_revision: allowed.turns[0].turn_revision, command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()), request_id: None, effect_id: None, update: CommitSessionTurnUpdate::ReconcileUnknownEffect { source_key: "ask:invalid-recovered".into(), effect_id: "ask-matrix".into(), checkpoint_json: "{}".into() } }, &ask_lease, 36).unwrap_err().to_string().contains("requires an interrupted child"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    #[rustfmt::skip]
    fn projections_and_manifest_boundaries_fail_closed_on_corrupt_durable_rows() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let valid_key = serde_json::to_string(&vec!["src", "lib.rs"]).unwrap();
        let baseline = std::collections::BTreeMap::from([(valid_key.clone(), "old".into())]);
        let queued = TurnState::queued(turn_id);
        store
            .create_session_v2(
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "inspect projection",
                &baseline,
                1,
            )
            .unwrap();
        { let conn = store.connection.lock().unwrap(); conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap(); conn.execute("UPDATE sessions SET latest_turn_id='bad' WHERE session_id=?1", [session_id.to_string()]).unwrap(); }
        assert!(store.session_snapshot_v2(session_id, None, 10).unwrap_err().to_string().contains("invalid stored run id"));
        { let conn = store.connection.lock().unwrap(); conn.execute("UPDATE sessions SET latest_turn_id=?1 WHERE session_id=?2", params![turn_id.to_string(), session_id.to_string()]).unwrap(); conn.execute("UPDATE session_active_turns SET turn_id='bad' WHERE session_id=?1", [session_id.to_string()]).unwrap(); }
        assert!(store.session_snapshot_v2(session_id, None, 10).unwrap_err().to_string().contains("invalid active turn id"));
        { let conn = store.connection.lock().unwrap(); conn.execute("UPDATE session_active_turns SET turn_id=?1 WHERE session_id=?2", params![turn_id.to_string(), session_id.to_string()]).unwrap(); conn.execute("UPDATE session_turns SET parent_turn_id='bad' WHERE turn_id=?1", [turn_id.to_string()]).unwrap(); }
        assert!(store.session_snapshot_v2(session_id, None, 10).unwrap_err().to_string().contains("invalid parent run id"));
        store.connection.lock().unwrap().execute("UPDATE session_turns SET parent_turn_id=NULL WHERE turn_id=?1", [turn_id.to_string()]).unwrap();

        assert!(
            store
                .session_changed_files(turn_id, &baseline)
                .unwrap()
                .is_empty()
        );
        let current = std::collections::BTreeMap::from([(valid_key, "new".into())]);
        assert_eq!(
            store.session_changed_files(turn_id, &current).unwrap(),
            vec!["src/lib.rs"]
        );
        let missing = TurnId::from_uuid(ids.next_uuid_v7());
        assert!(
            store
                .session_changed_files(missing, &std::collections::BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("no engine-owned baseline")
        );

        for key in [
            "not-json".to_owned(),
            serde_json::to_string(&Vec::<String>::new()).unwrap(),
            serde_json::to_string(&vec![""]).unwrap(),
            serde_json::to_string(&vec!["a/b"]).unwrap(),
            serde_json::to_string(&vec!["line\nbreak"]).unwrap(),
        ] {
            let manifest =
                serde_json::to_string(&std::collections::BTreeMap::from([(key, "digest")]))
                    .unwrap();
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "UPDATE turn_baselines SET manifest_json=?1 WHERE turn_id=?2",
                    params![manifest, turn_id.to_string()],
                )
                .unwrap();
            assert!(matches!(
                store.session_changed_files(turn_id, &std::collections::BTreeMap::new()),
                Err(StorageError::InvalidData(_))
            ));
        }
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE turn_baselines SET manifest_json='{' WHERE turn_id=?1",
                [turn_id.to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.session_changed_files(turn_id, &std::collections::BTreeMap::new()),
            Err(StorageError::InvalidData(_))
        ));

        let binding_json = serde_json::to_string(&session_binding()).unwrap();
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET binding_json='{' WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.session_snapshot_v2(session_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET binding_json=?1,lifecycle='invalid' WHERE session_id=?2",
                params![binding_json, session_id.to_string()],
            )
            .unwrap();
        }
        assert!(
            store
                .session_snapshot_v2(session_id, None, 10)
                .unwrap_err()
                .to_string()
                .contains("invalid session lifecycle")
        );
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET lifecycle='running' WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE turns SET state_json='{' WHERE turn_id=?1",
                [turn_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.session_snapshot_v2(session_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE turns SET state_json=?1 WHERE turn_id=?2",
                params![serde_json::to_string(&queued).unwrap(), turn_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE session_turns SET ordinal=-1 WHERE turn_id=?1",
                [turn_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.session_snapshot_v2(session_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE session_turns SET ordinal=0,completed_at_ms=-1 WHERE turn_id=?1",
                [turn_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.session_snapshot_v2(session_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE session_turns SET completed_at_ms=NULL WHERE turn_id=?1",
                [turn_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE conversation_outbox SET entry_json='{' WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.session_snapshot_v2(session_id, Some(0), 0),
            Err(StorageError::InvalidData(_))
        ));

        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "DELETE FROM session_active_turns WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE sessions SET lifecycle='ready',latest_turn_id=NULL WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
        }
        assert!(
            store
                .create_session_follow_up_v2(
                    session_id,
                    missing,
                    0,
                    "next",
                    &std::collections::BTreeMap::new(),
                    2,
                )
                .unwrap_err()
                .to_string()
                .contains("no completed child")
        );
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET latest_turn_id=?1 WHERE session_id=?2",
                params![turn_id.to_string(), session_id.to_string()],
            )
            .unwrap();
        }
        assert!(
            store
                .create_session_follow_up_v2(
                    session_id,
                    missing,
                    0,
                    "next",
                    &std::collections::BTreeMap::new(),
                    2,
                )
                .unwrap_err()
                .to_string()
                .contains("parent must be completed")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn verified_completion_requires_current_passing_evidence_and_exact_manifest() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let lease = store.acquire_lease("verify", 1, 1_000).unwrap();
        let valid_key = serde_json::to_string(&vec!["src", "main.rs"]).unwrap();
        let baseline = std::collections::BTreeMap::from([(valid_key.clone(), "old".into())]);

        let start = |baseline: Option<&std::collections::BTreeMap<String, String>>, now_ms| {
            let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
            let queued = TurnState::queued(turn_id);
            store
                .create_run_with_baseline(&queued, now_ms, baseline)
                .unwrap();
            let running = queued.transition(0, Transition::Start).unwrap();
            store
                .append_event(
                    &running,
                    0,
                    EventId::from_uuid(ids.next_uuid_v7()),
                    &RuntimeEvent::StateChanged {
                        status: TurnStatus::Running,
                    },
                    now_ms + 1,
                    &lease,
                )
                .unwrap();
            running
        };
        let record = |run: TurnId, id: &str, passed: bool, manifest_digest: &str, now_ms: u64| {
            let metadata = serde_json::to_string(&VerificationRecord {
                revision: 1,
                effect_epoch: 0,
                effect_id: id.into(),
                passed,
                workspace_manifest_digest: manifest_digest.into(),
                summary: format!("{id} summary"),
            })
            .unwrap();
            store
                .record_verification_evidence(
                    run,
                    1,
                    &lease,
                    &VerificationEvidence {
                        id,
                        metadata_json: &metadata,
                        blob_ref: None,
                    },
                    now_ms,
                )
                .unwrap();
        };

        let running = start(Some(&baseline), 10);
        let wrong_lease = Lease {
            scope: lease.scope.clone(),
            owner: "other".into(),
            fencing_token: lease.fencing_token,
            expires_at_ms: lease.expires_at_ms,
        };
        assert!(matches!(
            store.complete_verified(
                running.turn_id,
                running.revision,
                &wrong_lease,
                "summary".into(),
                &baseline,
                "manifest",
                20,
            ),
            Err(StorageError::LeaseLost)
        ));
        assert!(matches!(
            store.complete_verified(
                running.turn_id,
                99,
                &lease,
                "summary".into(),
                &baseline,
                "manifest",
                20,
            ),
            Err(StorageError::StaleRevision { .. })
        ));
        assert!(
            store
                .complete_verified(
                    running.turn_id,
                    1,
                    &lease,
                    "summary".into(),
                    &baseline,
                    "manifest",
                    20,
                )
                .unwrap_err()
                .to_string()
                .contains("missing current verification evidence")
        );

        record(running.turn_id, "failed", false, "manifest", 21);
        assert!(
            store
                .complete_verified(
                    running.turn_id,
                    1,
                    &lease,
                    "summary".into(),
                    &baseline,
                    "manifest",
                    22,
                )
                .unwrap_err()
                .to_string()
                .contains("verification failed")
        );
        record(running.turn_id, "stale-workspace", true, "before", 23);
        assert!(
            store
                .complete_verified(
                    running.turn_id,
                    1,
                    &lease,
                    "summary".into(),
                    &baseline,
                    "after",
                    24,
                )
                .unwrap_err()
                .to_string()
                .contains("workspace changed after verification")
        );
        record(running.turn_id, "passing", true, "manifest", 25);
        let current = std::collections::BTreeMap::from([(valid_key.clone(), "new".into())]);
        let (completed, event) = store
            .complete_verified(
                running.turn_id,
                1,
                &lease,
                "verified summary".into(),
                &current,
                "manifest",
                26,
            )
            .unwrap();
        assert_eq!(completed.status, TurnStatus::Completed);
        assert_eq!(event.sequence, 2);
        let handoff = completed.handoff.unwrap();
        assert_eq!(handoff.summary, "verified summary");
        assert_eq!(handoff.files_changed, vec!["src/main.rs"]);
        assert_eq!(handoff.evidence[0].status, VerificationStatus::Passed);

        let without_baseline = start(None, 30);
        record(
            without_baseline.turn_id,
            "no-baseline",
            true,
            "manifest",
            32,
        );
        assert!(
            store
                .complete_verified(
                    without_baseline.turn_id,
                    1,
                    &lease,
                    "summary".into(),
                    &std::collections::BTreeMap::new(),
                    "manifest",
                    33,
                )
                .unwrap_err()
                .to_string()
                .contains("missing engine-owned run baseline")
        );

        let invalid_key = serde_json::to_string(&vec!["bad/path"]).unwrap();
        let invalid_baseline = std::collections::BTreeMap::from([(invalid_key, "digest".into())]);
        let invalid = start(Some(&invalid_baseline), 40);
        record(invalid.turn_id, "invalid-path", true, "manifest", 42);
        assert!(
            store
                .complete_verified(
                    invalid.turn_id,
                    1,
                    &lease,
                    "summary".into(),
                    &std::collections::BTreeMap::new(),
                    "manifest",
                    43,
                )
                .unwrap_err()
                .to_string()
                .contains("invalid manifest component key")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    #[rustfmt::skip]
    fn lease_loss_checkpoint_and_waiting_cancellation_matrix_is_fail_closed() {
        use latte_core::{PendingInput, PendingPermission};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        assert!(
            store
                .acquire_lease("overflow", u64::MAX, 1)
                .unwrap_err()
                .to_string()
                .contains("lease expiry overflow")
        );
        let live = store.acquire_lease("live", 10, 10).unwrap();
        assert!(
            store
                .renew_lease(&live, u64::MAX, 1)
                .unwrap_err()
                .to_string()
                .contains("lease expiry overflow")
        );
        let forged = Lease {
            scope: live.scope.clone(),
            owner: "forged".into(),
            fencing_token: live.fencing_token,
            expires_at_ms: live.expires_at_ms,
        };
        assert!(matches!(
            store.release_lease(&forged),
            Err(StorageError::LeaseLost)
        ));

        let (session_id, turn_id, queued) = create_linked_fixture(&store, &ids, "recover", 11);
        let linked_lease = store.acquire_session_lease(session_id, 10, 10).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &linked_lease,
            &queued,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "recover:start".into(),
            },
            12,
        )
        .snapshot;
        assert!(
            store
                .recover_session_after_lease_loss(
                    session_id,
                    turn_id,
                    &linked_lease,
                    running.turns[0].turn_revision,
                    13,
                )
                .unwrap_err()
                .to_string()
                .contains("still authoritative")
        );
        assert!(matches!(
            store.put_checkpoint(turn_id, 1, &linked_lease, "{}", 13),
            Err(StorageError::LeaseLost)
        ));
        assert!(
            store
                .put_checkpoint(
                    TurnId::from_uuid(ids.next_uuid_v7()),
                    1,
                    &live,
                    "{",
                    13,
                )
                .unwrap_err()
                .to_string()
                .contains("EOF")
        );
        assert_eq!(
            store
                .checkpoint(TurnId::from_uuid(ids.next_uuid_v7()))
                .unwrap(),
            None
        );

        let missing = TurnId::from_uuid(ids.next_uuid_v7());
        assert!(matches!(
            store.recover_session_after_lease_loss(session_id, missing, &linked_lease, 0, 21),
            Err(StorageError::TurnNotFound(id)) if id == missing
        ));
        assert!(matches!(
            store
                .recover_session_after_lease_loss(
                    session_id,
                    turn_id,
                    &linked_lease,
                    running.turns[0].turn_revision + 1,
                    21,
                )
                .unwrap(),
            SessionLeaseLossRecovery::FencedNoop
        ));
        let recovered = store
            .recover_session_after_lease_loss(
                session_id,
                turn_id,
                &linked_lease,
                running.turns[0].turn_revision,
                21,
            )
            .unwrap();
        assert!(matches!(
            recovered,
            SessionLeaseLossRecovery::Recovered(response)
                if response.snapshot.lifecycle == SessionLifecycle::Interrupted
        ));
        let recovered_turn = store.load_turn(turn_id).unwrap(); assert!(matches!(store.recover_session_after_lease_loss(session_id, turn_id, &linked_lease, recovered_turn.revision, 22).unwrap(), SessionLeaseLossRecovery::AlreadyTerminal(_)));
        assert!(matches!(
            store
                .interrupt_after_lease_loss(turn_id, &linked_lease, recovered_turn.revision, 22),
            Err(StorageError::LeaseLost)
        ));
        assert!(matches!(
            store
                .interrupt_after_lease_loss(turn_id, &linked_lease, recovered_turn.revision + 1, 22),
            Err(StorageError::LeaseLost)
        ));

        let next = store.acquire_lease("next", 22, 100).unwrap();
        let legacy = TurnId::from_uuid(ids.next_uuid_v7());
        let queued = TurnState::queued(legacy);
        store.create_turn(&queued, 23).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                EventId::from_uuid(ids.next_uuid_v7()),
                &RuntimeEvent::StateChanged {
                    status: TurnStatus::Running,
                },
                24,
                &next,
            )
            .unwrap();
        let (waiting, _) = store
            .apply_transition(
                legacy,
                1,
                Transition::RequestPermission(PendingPermission {
                    request_id: "unprepared".into(),
                    operation_digest: "digest".into(),
                    description: "needs permission".into(),
                }),
                25,
                &next,
            )
            .unwrap();
        assert!(
            store
                .cancel_waiting(legacy, waiting.revision, &next, 26, true)
                .unwrap_err()
                .to_string()
                .contains("binding is not prepared")
        );

        let input_turn = TurnId::from_uuid(ids.next_uuid_v7());
        let input_queued = TurnState::queued(input_turn);
        store.create_turn(&input_queued, 30).unwrap();
        let (input_running, _) = store
            .apply_transition(input_turn, 0, Transition::Start, 31, &next)
            .unwrap();
        let (waiting_input, _) = store
            .apply_transition(
                input_turn,
                input_running.revision,
                Transition::RequestInput(PendingInput {
                    request_id: "input".into(),
                    prompt: "value?".into(),
                }),
                32,
                &next,
            )
            .unwrap();
        assert!(
            store
                .cancel_waiting(input_turn, waiting_input.revision, &next, 33, true)
                .unwrap_err()
                .to_string()
                .contains("not waiting for permission")
        );
        let (cancelled, event) = store
            .cancel_waiting(input_turn, waiting_input.revision, &next, 34, false)
            .unwrap();
        assert_eq!(cancelled.status, TurnStatus::Failed);
        assert_eq!(cancelled.failure.unwrap().code, FailureCode::Cancelled);
        assert!(event.is_some());
        let (terminal, duplicate) = store
            .cancel_waiting(input_turn, cancelled.revision, &next, 35, false)
            .unwrap();
        assert_eq!(terminal.status, TurnStatus::Failed);
        assert!(duplicate.is_none());

        let running_only = TurnId::from_uuid(ids.next_uuid_v7());
        let queued = TurnState::queued(running_only);
        store.create_turn(&queued, 40).unwrap();
        let (running, _) = store
            .apply_transition(running_only, 0, Transition::Start, 41, &next)
            .unwrap();
        assert!(
            store
                .cancel_waiting(running_only, running.revision, &next, 42, false)
                .unwrap_err()
                .to_string()
                .contains("turn is not waiting")
        );
        assert!(store.append_event(&running, running.revision, EventId::from_uuid(ids.next_uuid_v7()), &RuntimeEvent::StateChanged { status: TurnStatus::Running }, 43, &next).unwrap_err().to_string().contains("must increment once"));
        assert!(matches!(store.apply_transition(running_only, 0, Transition::Cancel, 43, &next), Err(StorageError::StaleRevision { .. }))); assert!(matches!(store.apply_transition(running_only, running.revision, Transition::Cancel, 43, &forged), Err(StorageError::LeaseLost)));
        store.connection.lock().unwrap().execute("UPDATE turns SET lease_token=?1 WHERE turn_id=?2", params![to_i64(next.fencing_token + 1).unwrap(), running_only.to_string()]).unwrap(); assert!(matches!(store.apply_transition(running_only, running.revision, Transition::Cancel, 43, &next), Err(StorageError::LeaseLost))); let cancelling = running.transition(running.revision, Transition::Cancel).unwrap(); assert!(matches!(store.append_event(&cancelling, running.revision, EventId::from_uuid(ids.next_uuid_v7()), &RuntimeEvent::StateChanged { status: TurnStatus::Cancelling }, 43, &next), Err(StorageError::LeaseLost))); store.connection.lock().unwrap().execute("UPDATE turns SET lease_token=?1 WHERE turn_id=?2", params![to_i64(next.fencing_token).unwrap(), running_only.to_string()]).unwrap();
        store.start_effect("invalid-status", running_only, 44).unwrap(); store.connection.lock().unwrap().execute("UPDATE effects SET status='invalid' WHERE effect_id='invalid-status'", []).unwrap(); assert!(matches!(store.effect_status("invalid-status"), Err(StorageError::InvalidData(_)))); assert!(store.prepare_effect("missing", "digest", "{}", 45).is_err()); assert!(store.start_prepared_effect("missing", "digest", 45).is_err()); let invalid_authority = EffectAuthority { turn_id: running_only, expected_revision: running.revision, lease: next.clone(), effect_id: "invalid-status".into(), digest: String::new(), attempt: 0 }; assert!(matches!(store.mark_effect_unknown(&invalid_authority, 45), Err(StorageError::EffectFenced))); assert!(matches!(store.replace_pending_effect("missing", "replacement", running_only, running.revision, 1, "{}", "digest", &next, 45), Err(StorageError::LeaseLost))); assert!(store.apply_transition(running_only, running.revision, Transition::Complete { handoff: Handoff { summary: "done".into(), files_changed: vec![], evidence: vec![] }, policy: CompletionPolicy::VerificationNotRequired }, 46, &next).is_ok()); store.release_lease(&next).unwrap();
    }

    #[test]
    fn focus_is_persisted_and_returned_in_snapshot() {
        use latte_core::SessionId;
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_session_lease(session_id, 1, 100).unwrap();
        let outcome = store
            .create_started_session_v2(
                None,
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "hello",
                &std::collections::BTreeMap::new(),
                &lease,
                2,
                Some("src/main.rs"),
            )
            .unwrap();
        let snapshot = match outcome {
            latte_core::CreateOutcome::Created(s) | latte_core::CreateOutcome::Replayed(s) => s,
        };
        assert_eq!(snapshot.focus.as_deref(), Some("src/main.rs"));

        // Read back
        let loaded = store.session_snapshot_v2(session_id, None, 500).unwrap();
        assert_eq!(loaded.focus.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn create_started_session_v2_is_idempotent() {
        use latte_core::SessionId;
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_session_lease(session_id, 1, 100).unwrap();

        // First create
        let first = store
            .create_started_session_v2(
                None,
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "hello",
                &std::collections::BTreeMap::new(),
                &lease,
                2,
                None,
            )
            .unwrap();
        assert!(matches!(first, latte_core::CreateOutcome::Created(_)));

        // Second create with same session_id should replay
        let second = store
            .create_started_session_v2(
                None,
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "hello",
                &std::collections::BTreeMap::new(),
                &lease,
                3,
                None,
            )
            .unwrap();
        assert!(matches!(second, latte_core::CreateOutcome::Replayed(_)));
    }

    #[test]
    fn durable_digest_distinguishes_raw_payloads_that_collapse_under_redaction() {
        use latte_core::SessionId;
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let command_id = latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_session_lease(session_id, 1, 100).unwrap();

        // First create with a secret-bearing prompt.
        let first = store
            .create_started_session_v2(
                Some(&command_id),
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "hello sk-this-is-a-secret-123456789",
                &std::collections::BTreeMap::new(),
                &lease,
                2,
                None,
            )
            .unwrap();
        assert!(matches!(first, latte_core::CreateOutcome::Created(_)));

        // Same command_id, different raw secret that redacts to the same
        // value.  Must fail with 422 idempotency_mismatch, not replay.
        let result = store.create_started_session_v2(
            Some(&command_id),
            session_id,
            turn_id,
            &session_binding(),
            "/workspace",
            "hello sk-this-is-a-secret-987654321",
            &std::collections::BTreeMap::new(),
            &lease,
            3,
            None,
        );
        assert!(matches!(
            result,
            Err(StorageError::SessionCommandReplayMismatch)
        ));

        // The pre-acquire lookup must agree: same command_id with a
        // different raw payload is a 422 idempotency_mismatch.
        let replay = store.lookup_create_replay(
            &command_id,
            session_id,
            "/workspace",
            "hello sk-this-is-a-secret-987654321",
            &session_binding(),
            None,
        );
        assert!(
            matches!(replay, Err(StorageError::SessionCommandReplayMismatch)),
            "mismatched raw payload must return 422, got {replay:?}"
        );
    }

    // -- Pure helper coverage ------------------------------------------------

    #[test]
    fn to_i64_and_from_i64_round_trip() {
        assert_eq!(to_i64(0).unwrap(), 0);
        assert_eq!(to_i64(42).unwrap(), 42);
        assert!(to_i64(u64::MAX).is_err());
        assert_eq!(from_i64(0).unwrap(), 0);
        assert_eq!(from_i64(42).unwrap(), 42);
        assert!(from_i64(-1).is_err());
    }

    #[test]
    fn status_name_covers_all_variants() {
        assert_eq!(status_name(TurnStatus::Queued), "queued");
        assert_eq!(status_name(TurnStatus::Running), "running");
        assert_eq!(
            status_name(TurnStatus::WaitingPermission),
            "waiting_permission"
        );
        assert_eq!(status_name(TurnStatus::WaitingInput), "waiting_input");
        assert_eq!(status_name(TurnStatus::Cancelling), "cancelling");
        assert_eq!(status_name(TurnStatus::Interrupted), "interrupted");
        assert_eq!(status_name(TurnStatus::Failed), "failed");
        assert_eq!(status_name(TurnStatus::Completed), "completed");
    }

    #[test]
    fn parse_session_id_and_turn_id_reject_invalid() {
        let id = SystemIdSource::default().next_uuid_v7();
        let s = id.to_string();
        assert_eq!(
            parse_session_id(&s).unwrap(),
            latte_core::SessionId::from_uuid(id)
        );
        assert!(parse_session_id("not-a-uuid").is_err());
        let turn_id = TurnId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let rs = turn_id.to_string();
        assert_eq!(parse_turn_id(&rs).unwrap(), turn_id);
        assert!(parse_turn_id("not-a-uuid").is_err());
    }

    #[test]
    fn session_lease_scope_formats_correctly() {
        let id = latte_core::SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
        assert_eq!(session_lease_scope(id), format!("session:{id}"));
    }

    #[test]
    fn require_lease_scope_checks_exact_match() {
        let lease = Lease {
            scope: "session:abc".into(),
            owner: "o".into(),
            fencing_token: 1,
            expires_at_ms: 1,
        };
        assert!(require_lease_scope(&lease, "session:abc").is_ok());
        assert!(matches!(
            require_lease_scope(&lease, "session:xyz"),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    fn validate_workspace_root_rejects_invalid() {
        assert!(validate_workspace_root("/valid/path").is_ok());
        assert!(validate_workspace_root("").is_err());
        assert!(validate_workspace_root(&"a".repeat(4097)).is_err());
        assert!(validate_workspace_root("bad\npath").is_err());
    }

    #[test]
    fn validate_catalog_key_rejects_invalid() {
        assert!(validate_catalog_key("valid-key_123", "test").is_ok());
        assert!(validate_catalog_key("", "test").is_err());
        assert!(validate_catalog_key(&"a".repeat(257), "test").is_err());
        assert!(validate_catalog_key("bad key", "test").is_err());
        assert!(validate_catalog_key("bad/key", "test").is_err());
    }

    #[test]
    fn session_title_truncates_and_falls_back() {
        assert_eq!(session_title("hello world"), "hello world");
        assert_eq!(session_title("  trimmed  "), "trimmed");
        assert_eq!(session_title("line1\nline2"), "line1");
        assert_eq!(session_title(""), "Untitled session");
        assert_eq!(session_title("\n\t"), "Untitled session");
        let long = "a".repeat(200);
        let title = session_title(&long);
        assert!(title.ends_with('…'));
        assert!(title.len() <= 120 + 3);
        // Titles are user-visible durable records and rename/fork titles are
        // user-supplied raw, so redaction happens inside session_title —
        // before the length cap, so truncation can never split a secret
        // across the boundary and leak its visible half.
        let secret = "sk-live-0123456789abcdef";
        let titled = session_title(&format!("work with api_key={secret} please"));
        assert!(!titled.contains(secret), "{titled}");
        assert!(titled.contains("[REDACTED]"), "{titled}");
        // Redaction runs before the cap, so a long value containing the
        // secret is collapsed to the redacted assignment and nothing — not
        // even a truncated prefix of the value — survives the boundary.
        let long_value = format!("api_key={}{}", "a".repeat(110), secret);
        assert_eq!(session_title(&long_value), "api_key=[REDACTED]");
    }

    #[test]
    fn parse_lifecycle_covers_all_variants() {
        assert_eq!(parse_lifecycle("ready").unwrap(), SessionLifecycle::Ready);
        assert_eq!(
            parse_lifecycle("running").unwrap(),
            SessionLifecycle::Running
        );
        assert_eq!(
            parse_lifecycle("waiting_permission").unwrap(),
            SessionLifecycle::WaitingPermission
        );
        assert_eq!(
            parse_lifecycle("waiting_input").unwrap(),
            SessionLifecycle::WaitingInput
        );
        assert_eq!(
            parse_lifecycle("interrupted").unwrap(),
            SessionLifecycle::Interrupted
        );
        assert_eq!(parse_lifecycle("failed").unwrap(), SessionLifecycle::Failed);
        assert_eq!(
            parse_lifecycle("reconciliation_required").unwrap(),
            SessionLifecycle::ReconciliationRequired
        );
        assert!(parse_lifecycle("unknown").is_err());
    }

    #[test]
    fn session_run_status_maps_all_variants() {
        assert_eq!(
            session_turn_status(TurnStatus::Queued),
            SessionTurnStatus::Queued
        );
        assert_eq!(
            session_turn_status(TurnStatus::Running),
            SessionTurnStatus::Running
        );
        assert_eq!(
            session_turn_status(TurnStatus::Cancelling),
            SessionTurnStatus::Cancelling
        );
        assert_eq!(
            session_turn_status(TurnStatus::WaitingPermission),
            SessionTurnStatus::WaitingPermission
        );
        assert_eq!(
            session_turn_status(TurnStatus::WaitingInput),
            SessionTurnStatus::WaitingInput
        );
        assert_eq!(
            session_turn_status(TurnStatus::Interrupted),
            SessionTurnStatus::Interrupted
        );
        assert_eq!(
            session_turn_status(TurnStatus::Failed),
            SessionTurnStatus::Failed
        );
        assert_eq!(
            session_turn_status(TurnStatus::Completed),
            SessionTurnStatus::Completed
        );
    }

    #[test]
    fn transcript_kind_name_covers_all_variants() {
        assert_eq!(transcript_kind_name(TranscriptKind::User), "user");
        assert_eq!(transcript_kind_name(TranscriptKind::Assistant), "assistant");
        assert_eq!(transcript_kind_name(TranscriptKind::ToolCall), "tool_call");
        assert_eq!(
            transcript_kind_name(TranscriptKind::ToolResult),
            "tool_result"
        );
        assert_eq!(
            transcript_kind_name(TranscriptKind::Permission),
            "permission"
        );
        assert_eq!(transcript_kind_name(TranscriptKind::Input), "input");
        assert_eq!(transcript_kind_name(TranscriptKind::Failure), "failure");
        assert_eq!(
            transcript_kind_name(TranscriptKind::Completion),
            "completion"
        );
        assert_eq!(transcript_kind_name(TranscriptKind::System), "system");
    }

    #[test]
    fn validate_session_source_rejects_invalid() {
        assert!(validate_session_source("valid-source").is_ok());
        assert!(validate_session_source("").is_err());
        assert!(validate_session_source(&"a".repeat(257)).is_err());
        assert!(validate_session_source("bad\nsource").is_err());
    }

    #[test]
    fn validate_session_effect_id_rejects_invalid() {
        assert!(validate_session_effect_id("effect-123").is_ok());
        assert!(validate_session_effect_id("").is_err());
        assert!(validate_session_effect_id(&"a".repeat(513)).is_err());
        assert!(validate_session_effect_id("bad\neffect").is_err());
    }

    #[test]
    fn validate_session_digest_rejects_invalid() {
        let valid = "a".repeat(64);
        assert!(validate_session_digest(&valid).is_ok());
        assert!(validate_session_digest("short").is_err());
        assert!(validate_session_digest(&"g".repeat(64)).is_err());
        assert!(validate_session_digest(&"a".repeat(63)).is_err());
    }

    #[test]
    fn redact_functions_sanitize_terminal_controls() {
        let permission = latte_core::PendingPermission {
            request_id: "req\x1b[31m1".into(),
            operation_digest: "digest".into(),
            description: "desc\x1b[0m".into(),
        };
        let redacted = redact_permission(&permission);
        assert!(!redacted.request_id.contains('\x1b'));
        assert!(!redacted.description.contains('\x1b'));

        let input = latte_core::PendingInput {
            request_id: "req\x1b[31m2".into(),
            prompt: "prompt\x1b[0m".into(),
        };
        let redacted = redact_input(&input);
        assert!(!redacted.request_id.contains('\x1b'));
        assert!(!redacted.prompt.contains('\x1b'));

        let failure = TurnFailure {
            code: FailureCode::RuntimeFailed,
            message: "fail\x1b[0m".into(),
            retryability: Retryability::Retryable,
        };
        let redacted = redact_failure(&failure);
        assert!(!redacted.message.contains('\x1b'));
        assert_eq!(redacted.code, failure.code);

        let handoff = Handoff {
            summary: "summary\x1b[0m".into(),
            files_changed: vec!["file\x1b[31m1".into()],
            evidence: vec![Evidence {
                name: "evidence\x1b[0m".into(),
                status: VerificationStatus::Passed,
                summary: "sum\x1b[0m".into(),
            }],
        };
        let redacted = redact_handoff(&handoff);
        assert!(!redacted.summary.contains('\x1b'));
        assert!(!redacted.files_changed[0].contains('\x1b'));
        assert!(!redacted.evidence[0].name.contains('\x1b'));
        assert!(!redacted.evidence[0].summary.contains('\x1b'));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn commit_session_run_update_rejects_invalid_effect_fields() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (session_id, turn_id, queued) = create_linked_fixture(&store, &ids, "validation", 11);
        let lease = store.acquire_session_lease(session_id, 10, 10_000).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &queued,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "validate:start".into(),
            },
            12,
        )
        .snapshot;
        let valid_digest = "a".repeat(64);
        let canonical = crate::SessionEffectDescriptor {
            effect_id: "effect-validate".into(),
            tool_call_id: "call-validate".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
            attempt: 1,
        };
        let canonical_json = serde_json::to_string(&canonical).unwrap();
        let session_rev = running.revision;
        let turn_rev = running.turns[0].turn_revision;

        // PrepareEffect: invalid source key (control character).
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: session_rev,
                    expected_turn_revision: turn_rev,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: Some("effect-validate".into()),
                    update: CommitSessionTurnUpdate::PrepareEffect {
                        source_key: "bad\nsource".into(),
                        effect_id: "effect-validate".into(),
                        operation_digest: valid_digest.clone(),
                        descriptor_json: "{}".into(),
                        canonical_descriptor_json: canonical_json.clone(),
                        policy: SessionEffectPolicy::Allow,
                        description: "read".into(),
                        checkpoint_json: "{}".into(),
                    },
                },
                &lease,
                13,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // PrepareEffect: invalid effect id (empty).
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: session_rev,
                    expected_turn_revision: turn_rev,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: Some(String::new()),
                    update: CommitSessionTurnUpdate::PrepareEffect {
                        source_key: "validate:empty-id".into(),
                        effect_id: String::new(),
                        operation_digest: valid_digest.clone(),
                        descriptor_json: "{}".into(),
                        canonical_descriptor_json: canonical_json.clone(),
                        policy: SessionEffectPolicy::Allow,
                        description: "read".into(),
                        checkpoint_json: "{}".into(),
                    },
                },
                &lease,
                14,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // PrepareEffect: invalid digest (not 64 hex chars).
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: session_rev,
                    expected_turn_revision: turn_rev,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: Some("effect-validate".into()),
                    update: CommitSessionTurnUpdate::PrepareEffect {
                        source_key: "validate:bad-digest".into(),
                        effect_id: "effect-validate".into(),
                        operation_digest: "short".into(),
                        descriptor_json: "{}".into(),
                        canonical_descriptor_json: canonical_json.clone(),
                        policy: SessionEffectPolicy::Allow,
                        description: "read".into(),
                        checkpoint_json: "{}".into(),
                    },
                },
                &lease,
                15,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // PrepareEffect: invalid descriptor JSON.
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: session_rev,
                    expected_turn_revision: turn_rev,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: Some("effect-validate".into()),
                    update: CommitSessionTurnUpdate::PrepareEffect {
                        source_key: "validate:bad-descriptor".into(),
                        effect_id: "effect-validate".into(),
                        operation_digest: valid_digest.clone(),
                        descriptor_json: "not json".into(),
                        canonical_descriptor_json: canonical_json.clone(),
                        policy: SessionEffectPolicy::Allow,
                        description: "read".into(),
                        checkpoint_json: "{}".into(),
                    },
                },
                &lease,
                16,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // PrepareEffect: invalid checkpoint JSON.
        assert!(matches!(
            store.commit_session_turn_update(
                &SessionCommitRequest {
                    session_id,
                    turn_id,
                    expected_session_revision: session_rev,
                    expected_turn_revision: turn_rev,
                    command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: Some("effect-validate".into()),
                    update: CommitSessionTurnUpdate::PrepareEffect {
                        source_key: "validate:bad-checkpoint".into(),
                        effect_id: "effect-validate".into(),
                        operation_digest: valid_digest.clone(),
                        descriptor_json: "{}".into(),
                        canonical_descriptor_json: canonical_json.clone(),
                        policy: SessionEffectPolicy::Allow,
                        description: "read".into(),
                        checkpoint_json: "not json".into(),
                    },
                },
                &lease,
                17,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // StartEffect: invalid effect id, digest, checkpoint.
        for (eid, digest, cp) in [
            ("", valid_digest.as_str(), "{}"),
            ("effect-validate", "short", "{}"),
            ("effect-validate", valid_digest.as_str(), "not json"),
        ] {
            assert!(matches!(
                store.commit_session_turn_update(
                    &SessionCommitRequest {
                        session_id,
                        turn_id,
                        expected_session_revision: session_rev,
                        expected_turn_revision: turn_rev,
                        command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: Some(eid.into()),
                        effect_id: Some(eid.into()),
                        update: CommitSessionTurnUpdate::StartEffect {
                            source_key: "validate:start-effect".into(),
                            effect_id: eid.into(),
                            operation_digest: digest.into(),
                            checkpoint_json: cp.into(),
                        },
                    },
                    &lease,
                    18,
                ),
                Err(StorageError::InvalidData(_))
            ));
        }
        // ObserveEffect: invalid effect id, digest, checkpoint.
        for (eid, digest, cp) in [
            ("", valid_digest.as_str(), "{}"),
            ("effect-validate", "short", "{}"),
            ("effect-validate", valid_digest.as_str(), "not json"),
        ] {
            assert!(matches!(
                store.commit_session_turn_update(
                    &SessionCommitRequest {
                        session_id,
                        turn_id,
                        expected_session_revision: session_rev,
                        expected_turn_revision: turn_rev,
                        command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: Some(eid.into()),
                        update: CommitSessionTurnUpdate::ObserveEffect {
                            source_key: "validate:observe".into(),
                            effect_id: eid.into(),
                            operation_digest: digest.into(),
                            success: true,
                            result: "ok".into(),
                            payload: None,
                            checkpoint_json: cp.into(),
                        },
                    },
                    &lease,
                    19,
                ),
                Err(StorageError::InvalidData(_))
            ));
        }
        // UnknownEffect: invalid effect id, digest, checkpoint.
        for (eid, digest, cp) in [
            ("", valid_digest.as_str(), "{}"),
            ("effect-validate", "short", "{}"),
            ("effect-validate", valid_digest.as_str(), "not json"),
        ] {
            assert!(matches!(
                store.commit_session_turn_update(
                    &SessionCommitRequest {
                        session_id,
                        turn_id,
                        expected_session_revision: session_rev,
                        expected_turn_revision: turn_rev,
                        command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: Some(eid.into()),
                        update: CommitSessionTurnUpdate::UnknownEffect {
                            source_key: "validate:unknown".into(),
                            effect_id: eid.into(),
                            operation_digest: digest.into(),
                            checkpoint_json: cp.into(),
                        },
                    },
                    &lease,
                    20,
                ),
                Err(StorageError::InvalidData(_))
            ));
        }
        // ReconcileUnknownEffect: invalid effect id, checkpoint.
        for (eid, cp) in [("", "{}"), ("effect-validate", "not json")] {
            assert!(matches!(
                store.commit_session_turn_update(
                    &SessionCommitRequest {
                        session_id,
                        turn_id,
                        expected_session_revision: session_rev,
                        expected_turn_revision: turn_rev,
                        command_id: latte_core::SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: Some(eid.into()),
                        update: CommitSessionTurnUpdate::ReconcileUnknownEffect {
                            source_key: "validate:reconcile".into(),
                            effect_id: eid.into(),
                            checkpoint_json: cp.into(),
                        },
                    },
                    &lease,
                    21,
                ),
                Err(StorageError::InvalidData(_))
            ));
        }
    }

    #[test]
    fn legacy_runtime_functions_reject_session_lease_scope() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(id_source.next_uuid_v7());
        let (turn_id, _event_id) = ids();
        let session_lease = store.acquire_session_lease(session_id, 1, 10_000).unwrap();
        let digest = "a".repeat(64);
        // cancel_waiting with a session lease → LeaseLost.
        assert!(matches!(
            store.cancel_waiting(turn_id, 0, &session_lease, 2, false),
            Err(StorageError::LeaseLost)
        ));
        // append_event with a session lease → LeaseLost.
        let state = TurnState::queued(turn_id);
        let event = RuntimeEvent::StateChanged {
            status: TurnStatus::Queued,
        };
        assert!(matches!(
            store.append_event(
                &state,
                0,
                EventId::from_uuid(id_source.next_uuid_v7()),
                &event,
                2,
                &session_lease,
            ),
            Err(StorageError::LeaseLost)
        ));
        // replace_pending_effect with a session lease → LeaseLost.
        assert!(matches!(
            store.replace_pending_effect(
                "old-effect",
                "new-effect",
                turn_id,
                0,
                1,
                "{}",
                &digest,
                &session_lease,
                2,
            ),
            Err(StorageError::LeaseLost)
        ));
        // create_prepared_permission with a session lease → LeaseLost.
        assert!(matches!(
            store.create_prepared_permission(
                "effect",
                turn_id,
                0,
                0,
                1,
                "{}",
                &digest,
                &session_lease,
                2,
            ),
            Err(StorageError::LeaseLost)
        ));
        // consume_permission_and_start with a session lease → LeaseLost.
        assert!(matches!(
            store.consume_permission_and_start("effect", turn_id, 0, &session_lease, &digest, 2),
            Err(StorageError::LeaseLost)
        ));
        // reconcile_unknown_and_abort with a session lease → LeaseLost.
        assert!(matches!(
            store.reconcile_unknown_and_abort(turn_id, "effect", 0, &session_lease, 2),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    fn cancel_waiting_rejects_stale_revision_and_non_waiting_states() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // Wrong expected revision → StaleRevision.
        assert!(matches!(
            store.cancel_waiting(turn_id, 99, &lease, 3, false),
            Err(StorageError::StaleRevision { .. })
        ));
        // Non-waiting run (queued) → InvalidData.
        assert!(matches!(
            store.cancel_waiting(turn_id, 0, &lease, 3, false),
            Err(StorageError::InvalidData(_))
        ));
        // Denied on a non-WaitingPermission run → InvalidData.
        assert!(matches!(
            store.cancel_waiting(turn_id, 0, &lease, 3, true),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn append_event_rejects_unknown_run_and_stale_revision() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        let state = TurnState::queued(turn_id);
        let event = RuntimeEvent::StateChanged {
            status: TurnStatus::Queued,
        };
        // Non-existent run → TurnNotFound.
        assert!(matches!(
            store.append_event(
                &state,
                0,
                EventId::from_uuid(id_source.next_uuid_v7()),
                &event,
                2,
                &lease,
            ),
            Err(StorageError::TurnNotFound(_))
        ));
        // Create the run, then use a stale revision.
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        let mut next = state.clone();
        next.revision = 5; // does not match expected_revision + 1
        assert!(matches!(
            store.append_event(
                &next,
                0,
                EventId::from_uuid(id_source.next_uuid_v7()),
                &event,
                3,
                &lease,
            ),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn switch_session_binding_v2_rejects_invalid_binding_and_unknown_session() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let (session_id, _turn_id, queued) =
            create_linked_fixture(&store, &id_source, "binding switch", 11);
        let lease = store.acquire_session_lease(session_id, 10, 10_000).unwrap();
        // Invalid binding (empty provider_name) → InvalidData.
        let mut bad_binding = session_binding();
        bad_binding.provider_name = String::new();
        assert!(matches!(
            store.switch_session_binding_v2(session_id, queued.revision, &bad_binding, &lease, 12),
            Err(StorageError::InvalidData(_))
        ));
        // Unknown session → SessionNotFound.
        let unknown = latte_core::SessionId::from_uuid(id_source.next_uuid_v7());
        let unknown_lease = store.acquire_session_lease(unknown, 10, 10_000).unwrap();
        assert!(matches!(
            store.switch_session_binding_v2(
                unknown,
                queued.revision,
                &session_binding(),
                &unknown_lease,
                12,
            ),
            Err(StorageError::SessionNotFound(_))
        ));
        // Stale revision → StaleSessionRevision.
        assert!(matches!(
            store.switch_session_binding_v2(
                session_id,
                queued.revision + 1,
                &session_binding(),
                &lease,
                12,
            ),
            Err(StorageError::StaleSessionRevision { .. })
        ));
    }

    #[test]
    fn create_prepared_permission_rejects_invalid_descriptor_json() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // Invalid descriptor JSON → InvalidData.
        assert!(matches!(
            store.create_prepared_permission(
                "effect",
                turn_id,
                0,
                0,
                1,
                "not json",
                &"a".repeat(64),
                &lease,
                3,
            ),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn replace_pending_effect_rejects_invalid_json_and_missing_effect() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // Invalid descriptor JSON → InvalidData.
        assert!(matches!(
            store.replace_pending_effect(
                "old-effect",
                "new-effect",
                turn_id,
                0,
                1,
                "not json",
                &"a".repeat(64),
                &lease,
                3,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // No existing pending permission → LeaseLost (validity check fails).
        assert!(matches!(
            store.replace_pending_effect(
                "missing-effect",
                "new-effect",
                turn_id,
                0,
                1,
                "{}",
                &"a".repeat(64),
                &lease,
                3,
            ),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    fn consume_permission_and_start_rejects_missing_permission() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // Non-existent permission → InvalidData.
        assert!(matches!(
            store.consume_permission_and_start(
                "missing-effect",
                turn_id,
                0,
                &lease,
                &"a".repeat(64),
                3,
            ),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn release_lease_rejects_stale_token() {
        let store = Storage::memory().unwrap();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        let stale = Lease {
            scope: lease.scope.clone(),
            owner: lease.owner.clone(),
            fencing_token: lease.fencing_token + 1,
            expires_at_ms: lease.expires_at_ms,
        };
        assert!(matches!(
            store.release_lease(&stale),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    fn complete_verified_rejects_session_lease_scope() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(id_source.next_uuid_v7());
        let (turn_id, _) = ids();
        let session_lease = store.acquire_session_lease(session_id, 1, 10_000).unwrap();
        let manifest = std::collections::BTreeMap::new();
        assert!(matches!(
            store.complete_verified(
                turn_id,
                0,
                &session_lease,
                "summary".into(),
                &manifest,
                "digest",
                2,
            ),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    fn import_legacy_database_rejects_invalid_workspace_root_and_fingerprint() {
        let store = Storage::memory().unwrap();
        let (dir, path) = db();
        // Create a minimal legacy database file.
        std::fs::write(&path, "not a database").unwrap();
        // Invalid workspace root (control character) → InvalidData.
        assert!(matches!(
            store.import_legacy_database(&path, "/source/path", "fingerprint", "bad\nroot", 1,),
            Err(StorageError::InvalidData(_))
        ));
        // Invalid fingerprint (control character) → InvalidData.
        assert!(matches!(
            store.import_legacy_database(
                &path,
                "/source/path",
                "bad\nfingerprint",
                "/workspace",
                1,
            ),
            Err(StorageError::InvalidData(_))
        ));
        drop(dir);
    }

    #[test]
    fn lookup_create_replay_rejects_invalid_workspace_root_and_returns_none_for_missing() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let session_id = latte_core::SessionId::from_uuid(id_source.next_uuid_v7());
        let command_id = latte_core::SessionCommandId::from_uuid(id_source.next_uuid_v7());
        let binding = session_binding();
        // Invalid workspace root → InvalidData.
        assert!(matches!(
            store.lookup_create_replay(
                &command_id,
                session_id,
                "bad\nroot",
                "prompt",
                &binding,
                None,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // No existing command → Ok(None).
        assert!(
            store
                .lookup_create_replay(
                    &command_id,
                    session_id,
                    "/workspace",
                    "prompt",
                    &binding,
                    None,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconcile_unknown_and_abort_rejects_stale_revision() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // Wrong expected revision → StaleRevision.
        assert!(matches!(
            store.reconcile_unknown_and_abort(turn_id, "effect", 99, &lease, 3),
            Err(StorageError::StaleRevision { .. })
        ));
    }

    #[test]
    fn interrupt_after_lease_loss_rejects_still_authoritative_lease() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // Lease is still valid → InvalidData.
        assert!(matches!(
            store.interrupt_after_lease_loss(turn_id, &lease, 0, 3),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn apply_transition_rejects_stale_lease_token() {
        let store = Storage::memory().unwrap();
        let (turn_id, _) = ids();
        let lease = store.acquire_lease("owner", 1, 10_000).unwrap();
        store.create_turn(&TurnState::queued(turn_id), 2).unwrap();
        // A lease with a higher fencing token → LeaseLost.
        let stale = Lease {
            scope: lease.scope.clone(),
            owner: lease.owner.clone(),
            fencing_token: lease.fencing_token + 1,
            expires_at_ms: lease.expires_at_ms,
        };
        assert!(matches!(
            store.apply_transition(turn_id, 0, Transition::Start, 3, &stale,),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    fn create_session_v2_rejects_invalid_binding_workspace_and_duplicate() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let baseline = std::collections::BTreeMap::new();
        let session_id = latte_core::SessionId::from_uuid(id_source.next_uuid_v7());
        let turn_id = TurnId::from_uuid(id_source.next_uuid_v7());
        // Invalid binding (empty provider_name) → InvalidData.
        let mut bad_binding = session_binding();
        bad_binding.provider_name = String::new();
        assert!(matches!(
            store.create_session_v2(
                session_id,
                turn_id,
                &bad_binding,
                "/workspace",
                "prompt",
                &baseline,
                1,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // Invalid workspace root (control character) → InvalidData.
        assert!(matches!(
            store.create_session_v2(
                session_id,
                turn_id,
                &session_binding(),
                "bad\nroot",
                "prompt",
                &baseline,
                1,
            ),
            Err(StorageError::InvalidData(_))
        ));
        // Create the session, then duplicate → replays the existing snapshot.
        store
            .create_session_v2(
                session_id,
                turn_id,
                &session_binding(),
                "/workspace",
                "prompt",
                &baseline,
                1,
            )
            .unwrap();
        let turn_id2 = TurnId::from_uuid(id_source.next_uuid_v7());
        let duplicate = store
            .create_session_v2(
                session_id,
                turn_id2,
                &session_binding(),
                "/workspace",
                "prompt",
                &baseline,
                2,
            )
            .unwrap();
        assert_eq!(duplicate.session_id, session_id);
    }

    #[test]
    fn create_session_follow_up_v2_rejects_unknown_session_and_wrong_lease_scope() {
        let store = Storage::memory().unwrap();
        let id_source = SystemIdSource::default();
        let baseline = std::collections::BTreeMap::new();
        let (session_id, _turn_id, queued) =
            create_linked_fixture(&store, &id_source, "follow-up errors", 11);
        // Unknown session → SessionNotFound.
        let unknown = latte_core::SessionId::from_uuid(id_source.next_uuid_v7());
        let follow_up = TurnId::from_uuid(id_source.next_uuid_v7());
        assert!(matches!(
            store.create_session_follow_up_v2(
                unknown,
                follow_up,
                queued.revision,
                "next",
                &baseline,
                12,
            ),
            Err(StorageError::SessionNotFound(_))
        ));
        // Wrong lease scope (runtime lease instead of session lease) → LeaseLost.
        let runtime_lease = store.acquire_lease("owner", 10, 10_000).unwrap();
        assert!(matches!(
            store.create_started_session_follow_up_v2(
                None,
                session_id,
                follow_up,
                queued.revision,
                "next",
                &baseline,
                &runtime_lease,
                12,
            ),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn tool_round_counter_counts_only_tool_cards_replays_once_and_survives_drain() {
        use latte_core::SessionCommandId;
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (session_id, turn_id, mut snapshot) = create_linked_fixture(&store, &ids, "loop", 100);
        let lease = store.acquire_session_lease(session_id, 10, 10_000).unwrap();
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            CommitSessionTurnUpdate::Start {
                source_key: "start".into(),
            },
            101,
        )
        .snapshot;
        assert_eq!(store.tool_round_count_for_turn(turn_id).unwrap(), 0);

        let tool_card = |source_key: &str, calls: serde_json::Value| {
            CommitSessionTurnUpdate::AppendTranscript {
                source_key: source_key.into(),
                kind: TranscriptKind::Assistant,
                text: String::new(),
                payload: Some(serde_json::json!({ "tool_calls": calls })),
            }
        };
        let call = serde_json::json!([{"id":"c","name":"list_directory","input":{}}]);

        // First tool round (sent explicitly so the exact request can be
        // replayed below).
        let first_round = SessionCommitRequest {
            session_id,
            turn_id,
            expected_session_revision: snapshot.revision,
            expected_turn_revision: snapshot.turns[0].turn_revision,
            command_id: SessionCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: tool_card("round-1", call),
        };
        snapshot = store
            .commit_session_turn_update(&first_round, &lease, 102)
            .unwrap()
            .snapshot;
        assert_eq!(store.tool_round_count_for_turn(turn_id).unwrap(), 1);

        // Plain assistant final, tool result, and an empty tool_calls array
        // must not count.
        for (now, source_key, kind, payload) in [
            (103, "final", TranscriptKind::Assistant, None),
            (
                104,
                "tool-result",
                TranscriptKind::ToolResult,
                Some(serde_json::json!({"ok": true})),
            ),
            (
                105,
                "empty",
                TranscriptKind::Assistant,
                Some(serde_json::json!({ "tool_calls": [] })),
            ),
        ] {
            snapshot = commit_linked(
                &store,
                &ids,
                &lease,
                &snapshot,
                turn_id,
                CommitSessionTurnUpdate::AppendTranscript {
                    source_key: source_key.into(),
                    kind,
                    text: String::new(),
                    payload,
                },
                now,
            )
            .snapshot;
        }
        assert_eq!(store.tool_round_count_for_turn(turn_id).unwrap(), 1);

        // Second real tool round counts; multiple calls in one card are one
        // round.
        let six_calls = serde_json::json!(
            (0..6)
                .map(|i| serde_json::json!({
                    "id": format!("c-{i}"),
                    "name": "list_directory",
                    "input": {}
                }))
                .collect::<Vec<_>>()
        );
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            turn_id,
            tool_card("round-2", six_calls),
            106,
        )
        .snapshot;
        assert_eq!(store.tool_round_count_for_turn(turn_id).unwrap(), 2);

        // Idempotent replay of the first round's command must not increment
        // again even though later commits moved the revision.
        let replay = store
            .commit_session_turn_update(&first_round, &lease, 107)
            .unwrap()
            .snapshot;
        assert_ne!(
            replay.revision, snapshot.revision,
            "replay returns the stored result"
        );
        assert_eq!(store.tool_round_count_for_turn(turn_id).unwrap(), 2);

        // The counter is authoritative: draining the outbox into the JSONL
        // conversation log leaves the count intact.
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM conversation_outbox WHERE session_id=?1",
                [session_id.to_string()],
            )
            .unwrap();
        assert_eq!(store.tool_round_count_for_turn(turn_id).unwrap(), 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn schema_14_migration_backfills_tool_round_counts_from_the_outbox() {
        use latte_core::SessionId;
        let (dir, path) = db();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let other_session = SessionId::from_uuid(ids.next_uuid_v7());
        let other_turn = TurnId::from_uuid(ids.next_uuid_v7());
        {
            let store = Storage::open(&path).unwrap();
            store
                .create_session_v2(
                    session_id,
                    turn_id,
                    &session_binding(),
                    "/workspace",
                    "long loop",
                    &std::collections::BTreeMap::new(),
                    1,
                )
                .unwrap();
            store
                .create_session_v2(
                    other_session,
                    other_turn,
                    &session_binding(),
                    "/workspace",
                    "other",
                    &std::collections::BTreeMap::new(),
                    2,
                )
                .unwrap();
            let conn = store.connection.lock().unwrap();
            // seq 1 is the initial user card created above; lay out a run of
            // 3 tool rounds interleaved with non-round cards.
            let cards: Vec<(u64, TurnId, TranscriptKind, Option<serde_json::Value>)> = vec![
                (
                    2,
                    turn_id,
                    TranscriptKind::Assistant,
                    Some(serde_json::json!({"tool_calls": [
                        {"id":"a0","name":"list_directory","input":{}},
                        {"id":"a1","name":"list_directory","input":{}},
                        {"id":"a2","name":"list_directory","input":{}},
                        {"id":"a3","name":"list_directory","input":{}},
                        {"id":"a4","name":"list_directory","input":{}},
                        {"id":"a5","name":"list_directory","input":{}},
                    ]})),
                ),
                (3, turn_id, TranscriptKind::ToolResult, None),
                (4, turn_id, TranscriptKind::Assistant, None),
                (
                    5,
                    turn_id,
                    TranscriptKind::Assistant,
                    Some(serde_json::json!({"tool_calls": [
                        {"id":"b","name":"read_file","input":{"path":"x"}},
                    ]})),
                ),
                (6, turn_id, TranscriptKind::ToolResult, None),
                (7, turn_id, TranscriptKind::User, None),
                (
                    8,
                    turn_id,
                    TranscriptKind::Assistant,
                    Some(serde_json::json!({"tool_calls": []})),
                ),
                (
                    9,
                    turn_id,
                    TranscriptKind::Assistant,
                    Some(serde_json::json!({"tool_calls": [
                        {"id":"c","name":"read_file","input":{"path":"y"}},
                    ]})),
                ),
                (
                    10,
                    other_turn,
                    TranscriptKind::Assistant,
                    Some(serde_json::json!({"tool_calls": [
                        {"id":"o","name":"list_directory","input":{}},
                    ]})),
                ),
            ];
            for (sequence, card_run, kind, payload) in cards {
                let entry = TranscriptEntry {
                    entry_id: TranscriptEntryId::from_uuid(ids.next_uuid_v7()),
                    sequence,
                    turn_id: Some(card_run),
                    kind,
                    text: String::new(),
                    payload,
                    source_key: format!("fixture:{sequence}"),
                    created_at_ms: sequence,
                };
                conn.execute(
                    "INSERT INTO conversation_outbox(session_id,seq,entry_id,turn_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        if card_run == turn_id { session_id.to_string() } else { other_session.to_string() },
                        to_i64(sequence).unwrap(),
                        entry.entry_id.to_string(),
                        card_run.to_string(),
                        transcript_kind_name(kind),
                        entry.source_key,
                        serde_json::to_string(&entry).unwrap(),
                        to_i64(sequence).unwrap(),
                    ],
                )
                .unwrap();
            }
        }
        // Undo schemas 14 and 15: reach a genuine v13 database (run columns,
        // no counter) so reopening runs both migrations for real.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{REVERSE_SCHEMA_15}\
                 ALTER TABLE session_runs DROP COLUMN tool_round_count;\
                 DELETE FROM schema_migrations WHERE version=14;\
                 PRAGMA user_version=13;"
            ))
            .unwrap();
        }
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(reopened.tool_round_count_for_turn(turn_id).unwrap(), 3);
        assert_eq!(reopened.tool_round_count_for_turn(other_turn).unwrap(), 1);
        let version: i64 = reopened
            .connection
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 15);
        drop(dir);
    }

    /// Schema 15 renames the physical run graph to turns. Rows written by a
    /// pre-15 binary serialize their durable JSON with the old `run_id` key
    /// ([`TurnState`] in `runs.state_json`, [`EventEnvelope`] in
    /// `events.event_json`); after the upgrade those rows must deserialize through the read-side
    /// serde aliases without a rewrite, keeping UUIDs intact.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn schema_15_renames_the_graph_and_keeps_old_run_json_keys_readable() {
        use latte_core::{SessionCommandId, SessionId};
        let (dir, path) = db();
        let ids = SystemIdSource::default();
        let session_id = SessionId::from_uuid(ids.next_uuid_v7());
        let turn_id = TurnId::from_uuid(ids.next_uuid_v7());
        let initial = {
            let store = Storage::open(&path).unwrap();
            store
                .create_session_v2(
                    session_id,
                    turn_id,
                    &session_binding(),
                    "/workspace",
                    "upgrade me",
                    &std::collections::BTreeMap::new(),
                    1,
                )
                .unwrap();
            let lease = store.acquire_session_lease(session_id, 1, 100).unwrap();
            store
                .commit_session_turn_update(
                    &SessionCommitRequest {
                        session_id,
                        turn_id,
                        expected_session_revision: 0,
                        expected_turn_revision: 0,
                        command_id: SessionCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: None,
                        update: CommitSessionTurnUpdate::Start {
                            source_key: "start".into(),
                        },
                    },
                    &lease,
                    2,
                )
                .unwrap();
            turn_id
        };
        // Rewind to a v14 database, then rewrite the durable JSON to carry
        // the old `run_id` key the way a pre-15 binary wrote it. json_set's
        // path is missing in the current document, so insert the value
        // explicitly rather than copying a non-existent `$.turn_id`.
        {
            let run_id_json = serde_json::to_string(&turn_id).unwrap();
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("{REVERSE_SCHEMA_15} PRAGMA user_version=14;"))
                .unwrap();
            conn.execute(
                "UPDATE runs SET state_json = json_insert( \
                    json_remove(state_json,'$.turn_id'), '$.run_id', json(?1))",
                [run_id_json.clone()],
            )
            .unwrap();
            conn.execute(
                "UPDATE events SET event_json = json_insert( \
                    json_remove(event_json,'$.turn_id'), '$.run_id', json(?1))",
                [run_id_json],
            )
            .unwrap();
            conn.execute(
                "UPDATE run_read_model SET state_json = json_insert( \
                    json_remove(state_json,'$.turn_id'), '$.run_id', json(?1))",
                [serde_json::to_string(&turn_id).unwrap()],
            )
            .unwrap();
            let state_json: String = conn
                .query_row(
                    "SELECT state_json FROM runs WHERE run_id=?1",
                    [turn_id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                state_json.contains("\"run_id\"") && !state_json.contains("\"turn_id\""),
                "pre-15 state_json must carry run_id: {state_json}"
            );
        }
        let reopened = Storage::open(&path).unwrap();
        // The physical graph now uses turn names…
        let version: i64 = reopened
            .connection
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 15);
        // …and the pre-15 JSON rows are still readable with the same UUID.
        // (Opening the catalog without a live engine recovery-sweeps an
        // orphaned Running child to Interrupted; the point under test is that
        // the old `run_id` key still deserializes, not the recovered status.)
        let state = reopened.load_turn(initial).unwrap();
        assert_eq!(state.turn_id, turn_id);
        let snapshot = reopened.session_snapshot_tail_v2(session_id, 10).unwrap();
        assert!(snapshot.turns.iter().any(|turn| turn.turn_id == turn_id));
        drop(dir);
    }
}
