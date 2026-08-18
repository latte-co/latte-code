//! Private `SQLite` authority for durable engine state.
use crate::VerificationEvidence;
use latte_core::{
    CompletionPolicy, EventEnvelope, EventId, Evidence, FailureCode, Handoff, PROTOCOL_VERSION,
    Retryability, RunFailure, RunId, RunState, RunStatus, RuntimeEvent, ThreadEvent,
    ThreadEventEnvelope, ThreadEventId, ThreadLifecycle, ThreadPendingRequest,
    ThreadProviderBindingV2, ThreadRunStatus, ThreadRunSummary, ThreadSessionSummary,
    ThreadSnapshot, TranscriptEntry, TranscriptEntryId, TranscriptKind, TranscriptPage, Transition,
    VerificationStatus, redact_thread_text, redact_thread_value,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, sync::Mutex};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 12;
const LEGACY_RUNTIME_LEASE_SCOPE: &str = "runtime";
/// The interactive session list carries a recent, bounded transcript per
/// thread.  The bound prevents a single long-running conversation from
/// allocating an unbounded amount of terminal projection memory while still
/// being large enough to cover normal active conversations.
const THREAD_PROJECTION_TRANSCRIPT_LIMIT: usize = 500;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite storage failure: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
    #[error("database schema version {found} is newer than supported version {supported}")]
    NewerSchema { found: i64, supported: i64 },
    #[error("run {0} was not found")]
    RunNotFound(RunId),
    #[error("stale run revision: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("runtime lease is held by another owner")]
    EngineUnavailable,
    #[error("runtime lease was lost")]
    LeaseLost,
    #[error("effect terminal write was fenced")]
    EffectFenced,
    #[error("thread {0} was not found")]
    ThreadNotFound(latte_core::ThreadId),
    #[error("thread revision is stale: expected {expected}, actual {actual}")]
    StaleThreadRevision { expected: u64, actual: u64 },
    #[error("linked thread runs must use CommitThreadRunUpdate")]
    LinkedRunRequiresThreadCommit,
    #[error("thread command id was reused with different content")]
    ThreadCommandReplayMismatch,
    #[error("thread {0} already exists")]
    ThreadAlreadyExists(latte_core::ThreadId),
    #[error("thread does not have the requested active run")]
    ThreadActiveRunMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub sequence: u64,
    pub envelope: EventEnvelope,
}

/// Durable thread event returned only after its containing `SQLite` transaction
/// has committed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredThreadEvent {
    pub sequence: u64,
    pub envelope: ThreadEventEnvelope,
}

/// Engine-only mutation variants for a linked v2 child run.  The legacy
/// engine APIs reject these run IDs so every state/event/transcript write is
/// kept in one fenced transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum CommitThreadRunUpdate {
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
        /// preparation record, and is never returned by a thread snapshot.
        canonical_descriptor_json: String,
        policy: ThreadEffectPolicy,
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
        failure: RunFailure,
    },
    Interrupt {
        source_key: String,
        reconciliation_effect_id: Option<String>,
    },
}

impl CommitThreadRunUpdate {
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
pub enum ThreadEffectPolicy {
    Allow,
    Ask,
}

/// Exact mutation preconditions.  `command_id` is deduplicated using a
/// canonical redacted digest; replaying it with different content fails
/// closed before any write.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadCommitRequest {
    pub thread_id: latte_core::ThreadId,
    pub run_id: RunId,
    pub expected_thread_revision: u64,
    pub expected_run_revision: u64,
    pub command_id: latte_core::ThreadCommandId,
    pub request_id: Option<String>,
    pub effect_id: Option<String>,
    pub update: CommitThreadRunUpdate,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadCommitResponse {
    pub snapshot: ThreadSnapshot,
    pub thread_event: StoredThreadEvent,
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
    Interrupted(RunState),
    FencedNoop,
    AlreadyTerminal(RunState),
}

/// Result of recovering a stale v2 linked child.  Unlike the legacy result,
/// a thread recovery includes the durable v2 event that was committed with
/// the v1 interruption and effect ledger changes.
#[derive(Clone, Debug, PartialEq)]
pub enum ThreadLeaseLossRecovery {
    Recovered(ThreadCommitResponse),
    FencedNoop,
    AlreadyTerminal(ThreadSnapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectStatus {
    Declared,
    Prepared,
    Started,
    ObservedSuccess,
    ObservedFailed,
    Unknown,
}
#[derive(Clone, Debug)]
pub(crate) struct EffectAuthority {
    run_id: RunId,
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
        conn.execute(
            "ATTACH DATABASE ?1 AS legacy_import",
            [path.to_string_lossy().as_ref()],
        )?;
        let import_result = (|| {
            let version: i64 =
                conn.query_row("PRAGMA legacy_import.user_version", [], |row| row.get(0))?;
            if !(9..=SCHEMA_VERSION).contains(&version) {
                return Err(StorageError::InvalidData(format!(
                    "legacy database schema {version} cannot be imported; expected 9 through {SCHEMA_VERSION}"
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
                    "SELECT EXISTS(SELECT 1 FROM legacy_import.runs l JOIN main.runs m USING(run_id)) \
                     OR EXISTS(SELECT 1 FROM legacy_import.threads_v2 l JOIN main.threads_v2 m USING(thread_id))",
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
            for table in [
                "sessions",
                "runs",
                "events",
                "command_dedup",
                "effects",
                "effect_attempts",
                "evidence",
                "run_read_model",
                "pending_permissions",
                "runtime_checkpoints",
                "run_baselines",
            ] {
                tx.execute_batch(&format!(
                    "INSERT OR IGNORE INTO main.{table} SELECT * FROM legacy_import.{table};"
                ))?;
            }
            // threads_v2 must be imported before thread_runs_v2 and other
            // tables that reference it via foreign keys. Legacy v9-v12 may
            // lack parent_thread_id (added in v10) and focus (added in v12).
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
            let parent_expr = if has_parent {
                "parent_thread_id"
            } else {
                "NULL"
            };
            let focus_expr = if has_focus { "focus" } else { "NULL" };
            tx.execute_batch(&format!(
                "INSERT OR IGNORE INTO main.threads_v2(thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root,parent_thread_id,focus) \
                 SELECT thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root,{parent_expr},{focus_expr} FROM legacy_import.threads_v2;"
            ))?;
            for table in [
                "thread_runs_v2",
                "thread_active_runs_v2",
                "thread_events_v2",
                "thread_command_dedup_v2",
                "thread_commit_sources_v2",
                "thread_effect_canonical_v2",
            ] {
                tx.execute_batch(&format!(
                    "INSERT OR IGNORE INTO main.{table} SELECT * FROM legacy_import.{table};"
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
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn create_run(&self, state: &RunState, now_ms: u64) -> Result<(), StorageError> {
        self.create_run_with_baseline(state, now_ms, None)
    }
    pub(crate) fn create_run_with_baseline(
        &self,
        state: &RunState,
        now_ms: u64,
        baseline: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Result<(), StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let json = serde_json::to_string(state).map_err(invalid_json)?;
        tx.execute("INSERT INTO runs(run_id,state_json,status,revision,last_seq,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,0,?5,?5)",
            params![state.run_id.to_string(), json, status_name(state.status), to_i64(state.revision)?, to_i64(now_ms)?])?;
        tx.execute(
            "INSERT INTO run_read_model(run_id,revision,last_seq,state_json) VALUES(?1,?2,0,?3)",
            params![
                state.run_id.to_string(),
                to_i64(state.revision)?,
                serde_json::to_string(state).map_err(invalid_json)?
            ],
        )?;
        if let Some(baseline) = baseline {
            tx.execute(
                "INSERT INTO run_baselines(run_id,manifest_json) VALUES(?1,?2)",
                params![
                    state.run_id.to_string(),
                    serde_json::to_string(baseline).map_err(invalid_json)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn load_run(&self, run_id: RunId) -> Result<RunState, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let json: Option<String> = conn
            .query_row(
                "SELECT state_json FROM run_read_model WHERE run_id=?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        json.ok_or(StorageError::RunNotFound(run_id))
            .and_then(|v| serde_json::from_str(&v).map_err(invalid_json))
    }

    pub(crate) fn list_runs(&self) -> Result<Vec<RunState>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare("SELECT state_json FROM run_read_model ORDER BY rowid")?;
        let values = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|v| serde_json::from_str(&v).map_err(invalid_json))
            .collect()
    }

    pub(crate) fn is_thread_linked_run(&self, run_id: RunId) -> Result<bool, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_runs_v2 WHERE run_id=?1)",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_thread_v2(
        &self,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        binding: &ThreadProviderBindingV2,
        workspace_root: &str,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        self.create_thread_v2_inner(
            None,
            thread_id,
            run_id,
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
    /// `ThreadCommandReplayMismatch` when the id was reused with a different
    /// command identity. This runs *before* lease acquisition so a retry after
    /// a crash never blocks on the dead owner's still-unexpired lease.
    pub(crate) fn lookup_create_replay(
        &self,
        command_id: &latte_core::ThreadCommandId,
        thread_id: latte_core::ThreadId,
        workspace_root: &str,
        prompt: &str,
        binding: &ThreadProviderBindingV2,
        focus: Option<&str>,
    ) -> Result<Option<ThreadSnapshot>, StorageError> {
        let prompt = redact_thread_text(prompt);
        let workspace_root = validate_workspace_root(workspace_root)?;
        let digest = create_command_digest(thread_id, workspace_root, &prompt, binding, focus);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT digest,result_json FROM thread_command_dedup_v2 WHERE command_id=?1",
                [command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_digest, result_json)) = row else {
            return Ok(None);
        };
        if stored_digest != digest {
            return Err(StorageError::ThreadCommandReplayMismatch);
        }
        let snapshot: ThreadSnapshot = serde_json::from_str(&result_json).map_err(invalid_json)?;
        Ok(Some(snapshot))
    }

    /// Atomically creates the Session, durable user card, linked child, and
    /// started run under the exact thread lease. There is no committed token-0
    /// child for a caller to strand between acceptance and `Start`.
    ///
    /// When `command_id` is provided, the create is crash-safe idempotent: the
    /// command is deduplicated against `thread_command_dedup_v2` *inside* the
    /// write transaction (recheck after the pre-acquire lookup), a same-id
    /// different-digest retry fails with `ThreadCommandReplayMismatch`, and a
    /// non-replay create for an already-existing thread fails with
    /// `ThreadAlreadyExists`. A same-id same-digest retry returns `Replayed`
    /// without starting a runner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_started_thread_v2(
        &self,
        command_id: Option<&latte_core::ThreadCommandId>,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        binding: &ThreadProviderBindingV2,
        workspace_root: &str,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        lease: &Lease,
        now_ms: u64,
        focus: Option<&str>,
    ) -> Result<latte_core::CreateOutcome<ThreadSnapshot>, StorageError> {
        let (outcome, thread_event) = self.create_thread_v2_inner(
            command_id,
            thread_id,
            run_id,
            binding,
            workspace_root,
            prompt,
            baseline,
            Some(lease),
            now_ms,
            focus,
        )?;
        let _ = thread_event; // Event is handled by finish_thread_response in engine layer
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn create_thread_v2_inner(
        &self,
        command_id: Option<&latte_core::ThreadCommandId>,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        binding: &ThreadProviderBindingV2,
        workspace_root: &str,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        initial_lease: Option<&Lease>,
        now_ms: u64,
        focus: Option<&str>,
    ) -> Result<
        (
            latte_core::CreateOutcome<ThreadSnapshot>,
            Option<StoredThreadEvent>,
        ),
        StorageError,
    > {
        binding.validate().map_err(StorageError::InvalidData)?;
        let prompt = redact_thread_text(prompt);
        if prompt.trim().is_empty() {
            return Err(StorageError::InvalidData(
                "thread prompt must not be empty".into(),
            ));
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let title = session_title(&prompt);
        let expected_scope = thread_lease_scope(thread_id);
        if let Some(lease) = initial_lease {
            require_lease_scope(lease, &expected_scope)?;
        }
        // Compute the durable command digest once (before `prompt` is moved
        // into the transcript card) and reuse it for both the in-transaction
        // recheck and the dedup insert.
        let command_digest =
            create_command_digest(thread_id, workspace_root, &prompt, binding, focus);
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
        // different-digest retry is a conflict.
        if let Some(command_id) = command_id {
            let previous: Option<(String, String)> = tx
                .query_row(
                    "SELECT digest,result_json FROM thread_command_dedup_v2 WHERE command_id=?1",
                    [command_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((stored_digest, result_json)) = previous {
                if stored_digest != command_digest {
                    return Err(StorageError::ThreadCommandReplayMismatch);
                }
                let snapshot: ThreadSnapshot =
                    serde_json::from_str(&result_json).map_err(invalid_json)?;
                tx.commit()?;
                return Ok((latte_core::CreateOutcome::Replayed(snapshot), None));
            }
            // A non-replay create for an already-existing thread is a conflict.
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads_v2 WHERE thread_id=?1)",
                [thread_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                return Err(StorageError::ThreadAlreadyExists(thread_id));
            }
        } else {
            // Legacy in-process path: an existing thread replays its snapshot.
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM threads_v2 WHERE thread_id=?1)",
                [thread_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                let snapshot =
                    current_thread_snapshot(&tx, thread_id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
                tx.commit()?;
                return Ok((latte_core::CreateOutcome::Replayed(snapshot), None));
            }
        }
        let queued = RunState::queued(run_id);
        let state = if initial_lease.is_some() {
            queued
                .transition(0, Transition::Start)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?
        } else {
            queued
        };
        let run_sequence = u64::from(initial_lease.is_some());
        let thread_revision = u64::from(initial_lease.is_some());
        let thread_sequence = if initial_lease.is_some() { 2 } else { 1 };
        let lease_token = initial_lease.map_or(0, |lease| lease.fencing_token);
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute(
            "INSERT INTO runs(run_id,state_json,status,revision,last_seq,lease_token,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
            params![run_id.to_string(), state_json, status_name(state.status), to_i64(state.revision)?, to_i64(run_sequence)?, to_i64(lease_token)?, to_i64(now_ms)?],
        )?;
        tx.execute(
            "INSERT INTO run_read_model(run_id,revision,last_seq,state_json) VALUES(?1,?2,?3,?4)",
            params![
                run_id.to_string(),
                to_i64(state.revision)?,
                to_i64(run_sequence)?,
                serde_json::to_string(&state).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO run_baselines(run_id,manifest_json) VALUES(?1,?2)",
            params![
                run_id.to_string(),
                serde_json::to_string(baseline).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO threads_v2(thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root,focus) VALUES(?1,?2,?3,'running',?4,?5,?6,?6,?7,?8,?9)",
            params![thread_id.to_string(), to_i64(thread_revision)?, to_i64(thread_sequence)?, serde_json::to_string(binding).map_err(invalid_json)?, run_id.to_string(), to_i64(now_ms)?, title, workspace_root, focus],
        )?;
        tx.execute(
            "INSERT INTO thread_runs_v2(run_id,thread_id,parent_run_id,ordinal) VALUES(?1,?2,NULL,0)",
            params![run_id.to_string(), thread_id.to_string()],
        )?;
        tx.execute(
            "INSERT INTO thread_active_runs_v2(thread_id,run_id,lease_token) VALUES(?1,?2,?3)",
            params![
                thread_id.to_string(),
                run_id.to_string(),
                to_i64(lease_token)?
            ],
        )?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            // Transcript paging has an unsigned cursor, so reserve zero as
            // the initial cursor and put the first user card at one.
            sequence: 1,
            run_id: Some(run_id),
            kind: TranscriptKind::User,
            text: prompt,
            payload: None,
            source_key: "thread:create:user".into(),
            created_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,1,?2,?3,'user',?4,?5,?6)",
            params![thread_id.to_string(), entry.entry_id.to_string(), run_id.to_string(), entry.source_key, serde_json::to_string(&entry).map_err(invalid_json)?, to_i64(now_ms)?],
        )?;
        let thread_event = if initial_lease.is_some() {
            let run_event = EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                event_id: EventId::from_uuid(Uuid::now_v7()),
                run_id,
                revision: state.revision,
                event: RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
                },
            };
            tx.execute(
                "INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,1,?2,?3,?4,?5)",
                params![run_id.to_string(), run_event.event_id.to_string(), to_i64(state.revision)?, serde_json::to_string(&run_event).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            let envelope = ThreadEventEnvelope {
                protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
                event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
                thread_id,
                revision: thread_revision,
                sequence: thread_sequence,
                event: ThreadEvent::LifecycleChanged {
                    lifecycle: ThreadLifecycle::Running,
                    run_id: Some(run_id),
                },
            };
            tx.execute(
                "INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![thread_id.to_string(), to_i64(thread_sequence)?, envelope.event_id.to_string(), to_i64(thread_revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            Some(StoredThreadEvent {
                sequence: thread_sequence,
                envelope,
            })
        } else {
            None
        };
        let snapshot = current_thread_snapshot(&tx, thread_id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
        // Record the durable dedup entry so a retry after a crash (durable
        // accept before 202) replays this acceptance without re-creating the
        // session or restarting the provider runner.
        if let Some(command_id) = command_id {
            let result_json = serde_json::to_string(&snapshot).map_err(invalid_json)?;
            tx.execute(
                "INSERT INTO thread_command_dedup_v2(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",
                params![command_id.to_string(), command_digest, result_json, to_i64(now_ms)?],
            )?;
        }
        tx.commit()?;
        Ok((latte_core::CreateOutcome::Created(snapshot), thread_event))
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn create_thread_follow_up_v2(
        &self,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        expected_thread_revision: u64,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<ThreadSnapshot, StorageError> {
        self.create_thread_follow_up_v2_inner(
            thread_id,
            run_id,
            expected_thread_revision,
            prompt,
            baseline,
            None,
            now_ms,
        )
        .map(|(snapshot, _)| snapshot)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_started_thread_follow_up_v2(
        &self,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        expected_thread_revision: u64,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadCommitResponse, StorageError> {
        let (snapshot, thread_event) = self.create_thread_follow_up_v2_inner(
            thread_id,
            run_id,
            expected_thread_revision,
            prompt,
            baseline,
            Some(lease),
            now_ms,
        )?;
        Ok(ThreadCommitResponse {
            snapshot,
            thread_event: thread_event.ok_or_else(|| {
                StorageError::InvalidData("atomic follow-up start omitted its durable event".into())
            })?,
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn create_thread_follow_up_v2_inner(
        &self,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        expected_thread_revision: u64,
        prompt: &str,
        baseline: &std::collections::BTreeMap<String, String>,
        initial_lease: Option<&Lease>,
        now_ms: u64,
    ) -> Result<(ThreadSnapshot, Option<StoredThreadEvent>), StorageError> {
        let prompt = redact_thread_text(prompt);
        if prompt.trim().is_empty() {
            return Err(StorageError::InvalidData(
                "thread follow-up must not be empty".into(),
            ));
        }
        let expected_scope = thread_lease_scope(thread_id);
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
        let (revision, lifecycle, latest, fork_parent): (
            i64,
            String,
            Option<String>,
            Option<String>,
        ) = tx
            .query_row(
                "SELECT revision,lifecycle,latest_run_id,parent_thread_id FROM threads_v2 WHERE thread_id=?1",
                [thread_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::ThreadNotFound(thread_id))?;
        let revision = from_i64(revision)?;
        if revision != expected_thread_revision {
            return Err(StorageError::StaleThreadRevision {
                expected: expected_thread_revision,
                actual: revision,
            });
        }
        if lifecycle != "ready"
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM thread_active_runs_v2 WHERE thread_id=?1)",
                [thread_id.to_string()],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(StorageError::InvalidData(
                "follow-up requires a ready thread with no active child".into(),
            ));
        }
        if latest.is_none() && fork_parent.is_none() {
            return Err(StorageError::InvalidData(
                "follow-up thread has no completed child".into(),
            ));
        }
        let parent_state = latest
            .as_ref()
            .map(|parent| {
                tx.query_row(
                    "SELECT state_json FROM runs WHERE run_id=?1",
                    [parent],
                    |row| row.get::<_, String>(0),
                )
                .map_err(StorageError::from)
                .and_then(|state| serde_json::from_str::<RunState>(&state).map_err(invalid_json))
            })
            .transpose()?;
        if parent_state.as_ref().is_some_and(|parent| {
            parent.status != RunStatus::Completed
                && !(parent.status == RunStatus::Failed
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
            "SELECT COALESCE(MAX(ordinal),-1)+1 FROM thread_runs_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| row.get::<_, i64>(0),
        )?)?;
        let queued = RunState::queued(run_id);
        let state = if initial_lease.is_some() {
            queued
                .transition(0, Transition::Start)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?
        } else {
            queued
        };
        let run_sequence = u64::from(initial_lease.is_some());
        let lease_token = initial_lease.map_or(0, |lease| lease.fencing_token);
        tx.execute("INSERT INTO runs(run_id,state_json,status,revision,last_seq,lease_token,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",params![run_id.to_string(),serde_json::to_string(&state).map_err(invalid_json)?,status_name(state.status),to_i64(state.revision)?,to_i64(run_sequence)?,to_i64(lease_token)?,to_i64(now_ms)?])?;
        tx.execute(
            "INSERT INTO run_read_model(run_id,revision,last_seq,state_json) VALUES(?1,?2,?3,?4)",
            params![
                run_id.to_string(),
                to_i64(state.revision)?,
                to_i64(run_sequence)?,
                serde_json::to_string(&state).map_err(invalid_json)?
            ],
        )?;
        tx.execute(
            "INSERT INTO run_baselines(run_id,manifest_json) VALUES(?1,?2)",
            params![
                run_id.to_string(),
                serde_json::to_string(baseline).map_err(invalid_json)?
            ],
        )?;
        tx.execute("INSERT INTO thread_runs_v2(run_id,thread_id,parent_run_id,ordinal) VALUES(?1,?2,?3,?4)",params![run_id.to_string(),thread_id.to_string(),latest,to_i64(ordinal)?])?;
        tx.execute(
            "INSERT INTO thread_active_runs_v2(thread_id,run_id,lease_token) VALUES(?1,?2,?3)",
            params![
                thread_id.to_string(),
                run_id.to_string(),
                to_i64(lease_token)?
            ],
        )?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("thread revision overflow".into()))?;
        let seq: u64 = from_i64(tx.query_row(
            "SELECT last_seq FROM threads_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| row.get::<_, i64>(0),
        )?)?
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidData("thread sequence overflow".into()))?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: seq,
            run_id: Some(run_id),
            kind: TranscriptKind::User,
            text: prompt,
            payload: None,
            source_key: format!("follow-up:{run_id}:user"),
            created_at_ms: now_ms,
        };
        tx.execute("INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,'user',?5,?6,?7)",params![thread_id.to_string(),to_i64(seq)?,entry.entry_id.to_string(),run_id.to_string(),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let summary = ThreadRunSummary {
            run_id,
            parent_run_id: parent_state.map(|parent| parent.run_id),
            ordinal,
            status: ThreadRunStatus::Queued,
            run_revision: 0,
            completed_at_ms: None,
            failure_code: None,
        };
        let event = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
            thread_id,
            revision: next_revision,
            sequence: seq,
            event: ThreadEvent::RunLinked { run: summary },
        };
        tx.execute("INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![thread_id.to_string(),to_i64(seq)?,event.event_id.to_string(),to_i64(next_revision)?,serde_json::to_string(&event).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE threads_v2 SET revision=?1,last_seq=?2,lifecycle='running',latest_run_id=?3,updated_at_ms=?4 WHERE thread_id=?5",params![to_i64(next_revision)?,to_i64(seq)?,run_id.to_string(),to_i64(now_ms)?,thread_id.to_string()])?;
        let thread_event = if initial_lease.is_some() {
            let run_event = EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                event_id: EventId::from_uuid(Uuid::now_v7()),
                run_id,
                revision: state.revision,
                event: RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
                },
            };
            tx.execute(
                "INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,1,?2,?3,?4,?5)",
                params![run_id.to_string(), run_event.event_id.to_string(), to_i64(state.revision)?, serde_json::to_string(&run_event).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            let started_revision = next_revision
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("thread revision overflow".into()))?;
            let started_sequence = seq
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("thread sequence overflow".into()))?;
            let envelope = ThreadEventEnvelope {
                protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
                event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
                thread_id,
                revision: started_revision,
                sequence: started_sequence,
                event: ThreadEvent::LifecycleChanged {
                    lifecycle: ThreadLifecycle::Running,
                    run_id: Some(run_id),
                },
            };
            tx.execute(
                "INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![thread_id.to_string(), to_i64(started_sequence)?, envelope.event_id.to_string(), to_i64(started_revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?, to_i64(now_ms)?],
            )?;
            tx.execute(
                "UPDATE threads_v2 SET revision=?1,last_seq=?2 WHERE thread_id=?3",
                params![
                    to_i64(started_revision)?,
                    to_i64(started_sequence)?,
                    thread_id.to_string()
                ],
            )?;
            Some(StoredThreadEvent {
                sequence: started_sequence,
                envelope,
            })
        } else {
            None
        };
        let snapshot = current_thread_snapshot(&tx, thread_id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
        tx.commit()?;
        Ok((snapshot, thread_event))
    }

    pub(crate) fn switch_thread_binding_v2(
        &self,
        thread_id: latte_core::ThreadId,
        expected_thread_revision: u64,
        binding: &ThreadProviderBindingV2,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadCommitResponse, StorageError> {
        binding.validate().map_err(StorageError::InvalidData)?;
        let expected_scope = thread_lease_scope(thread_id);
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
                "SELECT revision,last_seq,lifecycle,binding_json FROM threads_v2 WHERE thread_id=?1",
                [thread_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::ThreadNotFound(thread_id))?;
        let revision = from_i64(revision)?;
        if revision != expected_thread_revision {
            return Err(StorageError::StaleThreadRevision {
                expected: expected_thread_revision,
                actual: revision,
            });
        }
        if lifecycle != "ready"
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM thread_active_runs_v2 WHERE thread_id=?1)",
                [thread_id.to_string()],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Err(StorageError::InvalidData(
                "model switching requires a ready thread with no active child".into(),
            ));
        }
        let current_binding: ThreadProviderBindingV2 =
            serde_json::from_str(&current_binding).map_err(invalid_json)?;
        if current_binding == *binding {
            return Err(StorageError::InvalidData(
                "selected provider and model are already active".into(),
            ));
        }
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("thread revision overflow".into()))?;
        let next_sequence = from_i64(sequence)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("thread sequence overflow".into()))?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: next_sequence,
            run_id: None,
            kind: TranscriptKind::System,
            text: format!(
                "Model switched to {}/{}",
                binding.provider_name, binding.model
            ),
            payload: Some(serde_json::json!({
                "provider_name": binding.provider_name,
                "model": binding.model,
            })),
            source_key: format!("thread:model:{next_revision}"),
            created_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,NULL,'system',?4,?5,?6)",
            params![thread_id.to_string(), to_i64(next_sequence)?, entry.entry_id.to_string(), entry.source_key, serde_json::to_string(&entry).map_err(invalid_json)?, to_i64(now_ms)?],
        )?;
        let envelope = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
            thread_id,
            revision: next_revision,
            sequence: next_sequence,
            event: ThreadEvent::BindingChanged {
                provider_name: binding.provider_name.clone(),
                model: binding.model.clone(),
            },
        };
        tx.execute(
            "INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![thread_id.to_string(), to_i64(next_sequence)?, envelope.event_id.to_string(), to_i64(next_revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?, to_i64(now_ms)?],
        )?;
        tx.execute(
            "UPDATE threads_v2 SET revision=?1,last_seq=?2,binding_json=?3,updated_at_ms=?4 WHERE thread_id=?5",
            params![to_i64(next_revision)?, to_i64(next_sequence)?, serde_json::to_string(binding).map_err(invalid_json)?, to_i64(now_ms)?, thread_id.to_string()],
        )?;
        let snapshot = current_thread_snapshot(&tx, thread_id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
        tx.commit()?;
        Ok(ThreadCommitResponse {
            snapshot,
            thread_event: StoredThreadEvent {
                sequence: next_sequence,
                envelope,
            },
        })
    }

    pub(crate) fn thread_snapshot_v2(
        &self,
        thread_id: latte_core::ThreadId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<ThreadSnapshot, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        thread_snapshot(&conn, thread_id, after, limit)
    }

    pub(crate) fn thread_snapshot_tail_v2(
        &self,
        thread_id: latte_core::ThreadId,
        limit: usize,
    ) -> Result<ThreadSnapshot, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        current_thread_snapshot(&conn, thread_id, limit)
    }

    pub(crate) fn conversation_outbox_entries(
        &self,
        thread_id: latte_core::ThreadId,
    ) -> Result<Vec<TranscriptEntry>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT entry_json FROM conversation_outbox WHERE thread_id=?1 ORDER BY seq ASC",
        )?;
        statement
            .query_map([thread_id.to_string()], |row| row.get::<_, String>(0))?
            .map(|value| serde_json::from_str::<TranscriptEntry>(&value?).map_err(invalid_json))
            .collect()
    }

    pub(crate) fn acknowledge_conversation_outbox(
        &self,
        thread_id: latte_core::ThreadId,
        through_sequence: u64,
    ) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute(
            "DELETE FROM conversation_outbox WHERE thread_id=?1 AND seq<=?2",
            params![thread_id.to_string(), to_i64(through_sequence)?],
        )?;
        Ok(())
    }

    pub(crate) fn list_threads_v2(&self) -> Result<Vec<ThreadSnapshot>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn
            .prepare("SELECT thread_id FROM threads_v2 ORDER BY updated_at_ms DESC, rowid DESC")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|value| {
                let id = parse_thread_id(&value)?;
                // `thread_snapshot` pages forward for history reconstruction.
                // The TUI instead needs the current end of a conversation: a
                // first-20 ascending page made a completed 21+ card thread
                // look stale while silently hiding its newest work.
                let mut snapshot = thread_snapshot(&conn, id, None, 1)?;
                snapshot.transcript =
                    thread_transcript_tail(&conn, id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
                Ok(snapshot)
            })
            .collect()
    }

    pub(crate) fn list_threads_v2_for_workspace(
        &self,
        workspace_root: &str,
    ) -> Result<Vec<ThreadSnapshot>, StorageError> {
        let workspace_root = validate_workspace_root(workspace_root)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT thread_id FROM threads_v2 WHERE workspace_root=?1 \
             ORDER BY updated_at_ms DESC, rowid DESC",
        )?;
        let ids = statement
            .query_map([workspace_root], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|value| {
                let id = parse_thread_id(&value)?;
                let mut snapshot = thread_snapshot(&conn, id, None, 1)?;
                snapshot.transcript =
                    thread_transcript_tail(&conn, id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
                Ok(snapshot)
            })
            .collect()
    }

    pub(crate) fn list_thread_sessions_v2_for_workspace(
        &self,
        workspace_root: &str,
        limit: usize,
    ) -> Result<Vec<ThreadSessionSummary>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let limit = limit.min(500);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT thread_id,title,workspace_root,parent_thread_id,lifecycle,binding_json,created_at_ms,updated_at_ms \
             FROM threads_v2 WHERE workspace_root=?1 \
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
                thread_id,
                title,
                workspace_root,
                parent,
                lifecycle,
                binding_json,
                created,
                updated,
            ) = row?;
            let binding: ThreadProviderBindingV2 =
                serde_json::from_str(&binding_json).map_err(invalid_json)?;
            Ok(ThreadSessionSummary {
                thread_id: parse_thread_id(&thread_id)?,
                title,
                workspace_root,
                parent_thread_id: parent.as_deref().map(parse_thread_id).transpose()?,
                lifecycle: parse_lifecycle(&lifecycle)?,
                provider_name: binding.provider_name,
                model: binding.model,
                created_at_ms: from_i64(created)?,
                updated_at_ms: from_i64(updated)?,
            })
        })
        .collect()
    }

    pub(crate) fn find_thread_sessions_v2_by_exact_title_for_workspace(
        &self,
        workspace_root: &str,
        title: &str,
        limit: usize,
    ) -> Result<Vec<ThreadSessionSummary>, StorageError> {
        if title.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let limit = limit.min(500);
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT thread_id,title,workspace_root,parent_thread_id,lifecycle,binding_json,created_at_ms,updated_at_ms \
             FROM threads_v2 WHERE workspace_root=?1 AND title=?2 \
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
            |row| {
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
            },
        )?;
        rows.map(|row| {
            let (
                thread_id,
                title,
                workspace_root,
                parent,
                lifecycle,
                binding_json,
                created,
                updated,
            ) = row?;
            let binding: ThreadProviderBindingV2 =
                serde_json::from_str(&binding_json).map_err(invalid_json)?;
            Ok(ThreadSessionSummary {
                thread_id: parse_thread_id(&thread_id)?,
                title,
                workspace_root,
                parent_thread_id: parent.as_deref().map(parse_thread_id).transpose()?,
                lifecycle: parse_lifecycle(&lifecycle)?,
                provider_name: binding.provider_name,
                model: binding.model,
                created_at_ms: from_i64(created)?,
                updated_at_ms: from_i64(updated)?,
            })
        })
        .collect()
    }

    pub(crate) fn thread_session_v2(
        &self,
        thread_id: latte_core::ThreadId,
    ) -> Result<Option<ThreadSessionSummary>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let row = conn
            .query_row(
                "SELECT title,workspace_root,parent_thread_id,lifecycle,binding_json,created_at_ms,updated_at_ms \
                 FROM threads_v2 WHERE thread_id=?1",
                [thread_id.to_string()],
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
                let binding: ThreadProviderBindingV2 =
                    serde_json::from_str(&binding_json).map_err(invalid_json)?;
                Ok(ThreadSessionSummary {
                    thread_id,
                    title,
                    workspace_root,
                    parent_thread_id: parent.as_deref().map(parse_thread_id).transpose()?,
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

    pub(crate) fn search_thread_sessions(
        &self,
        workspace_root: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ThreadSessionSummary>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = validate_workspace_root(workspace_root)?;
        let query = query.trim().to_lowercase();
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut statement = conn.prepare(
            "SELECT thread_id,title,workspace_root,parent_thread_id,lifecycle,binding_json,created_at_ms,updated_at_ms \
             FROM threads_v2 WHERE workspace_root=?1 AND \
             (?2='' OR instr(lower(title),?2)>0 OR instr(lower(thread_id),?2)>0) \
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
            |row| {
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
            },
        )?;
        rows.map(|row| {
            let (
                thread_id,
                title,
                workspace_root,
                parent,
                lifecycle,
                binding_json,
                created,
                updated,
            ) = row?;
            let binding: ThreadProviderBindingV2 =
                serde_json::from_str(&binding_json).map_err(invalid_json)?;
            Ok(ThreadSessionSummary {
                thread_id: parse_thread_id(&thread_id)?,
                title,
                workspace_root,
                parent_thread_id: parent.as_deref().map(parse_thread_id).transpose()?,
                lifecycle: parse_lifecycle(&lifecycle)?,
                provider_name: binding.provider_name,
                model: binding.model,
                created_at_ms: from_i64(created)?,
                updated_at_ms: from_i64(updated)?,
            })
        })
        .collect()
    }

    pub(crate) fn rename_thread_session(
        &self,
        thread_id: latte_core::ThreadId,
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
            "UPDATE threads_v2 SET title=?1 WHERE thread_id=?2",
            params![title, thread_id.to_string()],
        )?;
        if changed == 0 {
            return Err(StorageError::ThreadNotFound(thread_id));
        }
        Ok(())
    }

    pub(crate) fn create_thread_session_fork(
        &self,
        source_thread_id: latte_core::ThreadId,
        fork_thread_id: latte_core::ThreadId,
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
                "SELECT title,workspace_root,binding_json,focus FROM threads_v2 WHERE thread_id=?1",
                [source_thread_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::ThreadNotFound(source_thread_id))?;
        let title = title.map_or_else(
            || session_title(&format!("{source_title} (fork)")),
            session_title,
        );
        let last_sequence = history.last().map_or(0, |entry| entry.sequence);
        tx.execute(
            "INSERT INTO threads_v2(thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms,title,workspace_root,parent_thread_id,focus) \
             VALUES(?1,0,?2,'ready',?3,NULL,?4,?4,?5,?6,?7,?8)",
            params![fork_thread_id.to_string(),to_i64(last_sequence)?,binding_json,to_i64(now_ms)?,title,workspace_root,source_thread_id.to_string(),focus],
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
            entry.run_id = None;
            entry.source_key = format!("fork:{source_thread_id}:{}", source.sequence);
            tx.execute(
                "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) \
                 VALUES(?1,?2,?3,NULL,?4,?5,?6,?7)",
                params![fork_thread_id.to_string(),to_i64(entry.sequence)?,entry.entry_id.to_string(),transcript_kind_name(entry.kind),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(entry.created_at_ms)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Computes the exact workspace paths changed since this linked child was
    /// created.  The engine supplies the fresh manifest; the baseline never
    /// leaves durable storage except as this checked display list.
    pub(crate) fn thread_changed_files(
        &self,
        run_id: RunId,
        current_manifest: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let baseline_json: String = conn
            .query_row(
                "SELECT manifest_json FROM run_baselines WHERE run_id=?1",
                [run_id.to_string()],
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
    pub(crate) fn commit_thread_run_update(
        &self,
        request: &ThreadCommitRequest,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<ThreadCommitResponse, StorageError> {
        let expected_scope = thread_lease_scope(request.thread_id);
        require_lease_scope(lease, &expected_scope)?;
        validate_thread_source(request.update.source_key())?;
        let digest = thread_command_digest(request)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous_command: Option<(String, String)> = tx
            .query_row(
                "SELECT digest,result_json FROM thread_command_dedup_v2 WHERE command_id=?1",
                [request.command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_digest, result)) = previous_command {
            if stored_digest != digest {
                return Err(StorageError::ThreadCommandReplayMismatch);
            }
            let replay = serde_json::from_str(&result).map_err(invalid_json)?;
            tx.commit()?;
            return Ok(replay);
        }
        let previous_source: Option<(String, String)> = tx.query_row("SELECT digest,result_json FROM thread_commit_sources_v2 WHERE thread_id=?1 AND source_key=?2",params![request.thread_id.to_string(),request.update.source_key()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((stored_digest, result)) = previous_source {
            if stored_digest != digest {
                return Err(StorageError::ThreadCommandReplayMismatch);
            }
            let replay = serde_json::from_str(&result).map_err(invalid_json)?;
            tx.execute("INSERT INTO thread_command_dedup_v2(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",params![request.command_id.to_string(),digest,result,to_i64(now_ms)?])?;
            tx.commit()?;
            return Ok(replay);
        }
        let lease_ok: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![expected_scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|row|row.get(0))?;
        if !lease_ok {
            return Err(StorageError::LeaseLost);
        }
        let (thread_revision, last_seq, lifecycle, latest_run): (i64, i64, String, Option<String>) = tx
            .query_row(
                "SELECT revision,last_seq,lifecycle,latest_run_id FROM threads_v2 WHERE thread_id=?1",
                [request.thread_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or(StorageError::ThreadNotFound(request.thread_id))?;
        let thread_revision = from_i64(thread_revision)?;
        if thread_revision != request.expected_thread_revision {
            return Err(StorageError::StaleThreadRevision {
                expected: request.expected_thread_revision,
                actual: thread_revision,
            });
        }
        let active: Option<(String, i64)> = tx
            .query_row(
                "SELECT run_id,lease_token FROM thread_active_runs_v2 WHERE thread_id=?1",
                [request.thread_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // Unknown-effect reconciliation happens only after the conservative
        // terminal path has removed the active child.  It is still fenced by
        // the exact thread/run/revision binding below, but must not require a
        // row that recovery deliberately cleared.
        let recovered_reconciliation = active.is_none()
            && matches!(
                &request.update,
                CommitThreadRunUpdate::ReconcileUnknownEffect { .. }
            )
            && lifecycle == "reconciliation_required"
            && latest_run.as_deref() == Some(request.run_id.to_string().as_str());
        if let Some((active_run, active_token)) = active {
            if active_run != request.run_id.to_string()
                || from_i64(active_token)? > lease.fencing_token
            {
                return Err(StorageError::ThreadActiveRunMismatch);
            }
        } else if !recovered_reconciliation {
            return Err(StorageError::ThreadActiveRunMismatch);
        }
        let (state_json, run_seq, run_token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM runs WHERE run_id=?1",
            [request.run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if from_i64(run_token)? > lease.fencing_token {
            return Err(StorageError::LeaseLost);
        }
        let current: RunState = serde_json::from_str(&state_json).map_err(invalid_json)?;
        if current.revision != request.expected_run_revision {
            return Err(StorageError::StaleRevision {
                expected: request.expected_run_revision,
                actual: current.revision,
            });
        }

        let mut next = current.clone();
        let mut run_changed = false;
        let mut next_lifecycle = lifecycle;
        let mut card: Option<(TranscriptKind, String, Option<serde_json::Value>, String)> = None;
        let mut terminal = false;
        let mut reconciliation_effect = None;
        let mut checkpoint: Option<String> = None;
        match &request.update {
            CommitThreadRunUpdate::Start { .. } => {
                next = current
                    .transition(current.revision, Transition::Start)
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                run_changed = true;
                next_lifecycle = "running".into();
            }
            CommitThreadRunUpdate::AppendTranscript {
                source_key,
                kind,
                text,
                payload,
            } => {
                card = Some((
                    *kind,
                    redact_thread_text(text),
                    payload.clone().map(redact_thread_value),
                    source_key.clone(),
                ));
            }
            CommitThreadRunUpdate::PrepareEffect {
                source_key,
                effect_id,
                operation_digest,
                descriptor_json,
                canonical_descriptor_json,
                policy,
                description,
                checkpoint_json,
            } => {
                validate_thread_effect_id(effect_id)?;
                validate_thread_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(descriptor_json).map_err(invalid_json)?;
                serde_json::from_str::<crate::ThreadEffectDescriptor>(canonical_descriptor_json)
                    .map_err(invalid_json)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                if current.status != RunStatus::Running {
                    return Err(StorageError::InvalidData(
                        "only a running linked child can prepare an effect".into(),
                    ));
                }
                let pre_evidence = match policy {
                    ThreadEffectPolicy::Allow => r#"{"thread_policy":"allow"}"#,
                    ThreadEffectPolicy::Ask => r#"{"thread_policy":"ask"}"#,
                };
                tx.execute("INSERT INTO effects(effect_id,run_id,status,started_at_ms,attempt,descriptor_json,approval_digest,pre_evidence_json,prepared_at_ms) VALUES(?1,?2,'prepared',?3,1,?4,?5,?6,?3)",params![effect_id,request.run_id.to_string(),to_i64(now_ms)?,descriptor_json,operation_digest,pre_evidence])?;
                tx.execute(
                    "INSERT INTO thread_effect_canonical_v2(effect_id,run_id,descriptor_json) VALUES(?1,?2,?3)",
                    params![effect_id, request.run_id.to_string(), canonical_descriptor_json],
                )?;
                if *policy == ThreadEffectPolicy::Ask {
                    let pending = latte_core::PendingPermission {
                        request_id: redact_thread_text(effect_id),
                        operation_digest: redact_thread_text(operation_digest),
                        description: redact_thread_text(description),
                    };
                    next = current
                        .transition(current.revision, Transition::RequestPermission(pending))
                        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                    run_changed = true;
                    next_lifecycle = "waiting_permission".into();
                    let post_approval_revision = current
                        .revision
                        .checked_add(2)
                        .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
                    tx.execute("INSERT INTO pending_permissions(effect_id,run_id,run_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![effect_id,request.run_id.to_string(),to_i64(post_approval_revision)?,lease.owner,to_i64(lease.fencing_token)?,operation_digest])?;
                }
                card = Some((
                    TranscriptKind::ToolCall,
                    redact_thread_text(description),
                    Some(redact_thread_value(serde_json::json!({
                        "descriptor": serde_json::from_str::<serde_json::Value>(descriptor_json)
                            .map_err(invalid_json)?,
                        "operation_digest": operation_digest,
                    }))),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitThreadRunUpdate::StartEffect {
                source_key,
                effect_id,
                operation_digest,
                checkpoint_json,
            } => {
                validate_thread_effect_id(effect_id)?;
                validate_thread_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                if current.status != RunStatus::Running
                    || current.pending_permission.is_some()
                    || current.pending_input.is_some()
                {
                    return Err(StorageError::InvalidData(
                        "effect start requires a running child without a pending request".into(),
                    ));
                }
                let prepared: Option<(String, String)> = tx
                    .query_row(
                        "SELECT approval_digest,pre_evidence_json FROM effects WHERE effect_id=?1 AND run_id=?2 AND status='prepared'",
                        params![effect_id, request.run_id.to_string()],
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
                        "SELECT run_revision,lease_owner,lease_token,consumed_at_ms FROM pending_permissions WHERE effect_id=?1 AND run_id=?2",
                        params![effect_id, request.run_id.to_string()],
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
                } else if policy_marker != r#"{"thread_policy":"allow"}"# {
                    return Err(StorageError::InvalidData(
                        "prepared effect has no durable allow authorization".into(),
                    ));
                }
                let started = tx.execute(
                    "UPDATE effects SET status='started',started_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='prepared' AND approval_digest=?4",
                    params![to_i64(now_ms)?,effect_id,request.run_id.to_string(),operation_digest],
                )?;
                if started != 1 {
                    return Err(StorageError::EffectFenced);
                }
                let bumped = tx.execute(
                    "UPDATE runs SET effect_epoch=effect_epoch+1,lease_token=?1,updated_at_ms=?2 WHERE run_id=?3 AND revision=?4 AND lease_token<=?1",
                    params![to_i64(lease.fencing_token)?,to_i64(now_ms)?,request.run_id.to_string(),to_i64(current.revision)?],
                )?;
                if bumped != 1 {
                    return Err(StorageError::LeaseLost);
                }
                card = Some((
                    TranscriptKind::System,
                    "tool started".into(),
                    Some(
                        serde_json::json!({"effect_id":redact_thread_text(effect_id),"status":"started"}),
                    ),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitThreadRunUpdate::ObserveEffect {
                source_key,
                effect_id,
                operation_digest,
                success,
                result,
                payload,
                checkpoint_json,
            } => {
                validate_thread_effect_id(effect_id)?;
                validate_thread_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                if current.status != RunStatus::Running {
                    return Err(StorageError::InvalidData(
                        "effect observation requires a running linked child".into(),
                    ));
                }
                let changed = tx.execute(
                    "UPDATE effects SET status=?1,post_evidence_json=?2,observed_at_ms=?3 WHERE effect_id=?4 AND run_id=?5 AND status='started' AND approval_digest=?6",
                    params![if *success { "observed_success" } else { "observed_failed" },serde_json::to_string(&redact_thread_value(serde_json::json!({"result":result,"payload":payload.clone()}))).map_err(invalid_json)?,to_i64(now_ms)?,effect_id,request.run_id.to_string(),operation_digest],
                )?;
                if changed != 1 {
                    return Err(StorageError::EffectFenced);
                }
                card = Some((
                    TranscriptKind::ToolResult,
                    redact_thread_text(result),
                    payload.clone().map(redact_thread_value),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitThreadRunUpdate::UnknownEffect {
                source_key,
                effect_id,
                operation_digest,
                checkpoint_json,
            } => {
                validate_thread_effect_id(effect_id)?;
                validate_thread_digest(operation_digest)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                let changed = tx.execute(
                    r#"UPDATE effects SET status='unknown',post_evidence_json='{"outcome":"uncertain"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='started' AND approval_digest=?4"#,
                    params![to_i64(now_ms)?,effect_id,request.run_id.to_string(),operation_digest],
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
                run_changed = true;
                terminal = true;
                reconciliation_effect = Some(redact_thread_text(effect_id));
                next_lifecycle = "reconciliation_required".into();
                card = Some((
                    TranscriptKind::Failure,
                    "effect outcome unknown; reconciliation required".into(),
                    Some(
                        serde_json::json!({"effect_id":redact_thread_text(effect_id),"status":"unknown"}),
                    ),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitThreadRunUpdate::ReconcileUnknownEffect {
                source_key,
                effect_id,
                checkpoint_json,
            } => {
                validate_thread_effect_id(effect_id)?;
                serde_json::from_str::<serde_json::Value>(checkpoint_json).map_err(invalid_json)?;
                let changed = tx.execute(
                    r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"reconciliation":"acknowledged_failed"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='unknown'"#,
                    params![to_i64(now_ms)?,effect_id,request.run_id.to_string()],
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
                    if current.status != RunStatus::Interrupted {
                        return Err(StorageError::InvalidData(
                            "recovered reconciliation requires an interrupted child".into(),
                        ));
                    }
                    next.status = RunStatus::Failed;
                    next.revision = current
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
                    next.failure = Some(RunFailure {
                        code: FailureCode::RuntimeFailed,
                        message: format!(
                            "unknown effect {} acknowledged failed; run aborted",
                            redact_thread_text(effect_id)
                        ),
                        retryability: Retryability::Terminal,
                    });
                    next.pending_input = None;
                    next.pending_permission = None;
                } else {
                    next = current
                        .transition(
                            current.revision,
                            Transition::Fail(RunFailure {
                                code: FailureCode::RuntimeFailed,
                                message: format!(
                                    "unknown effect {} acknowledged failed; run aborted",
                                    redact_thread_text(effect_id)
                                ),
                                retryability: Retryability::Terminal,
                            }),
                        )
                        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
                }
                run_changed = true;
                terminal = true;
                next_lifecycle = "failed".into();
                card = Some((
                    TranscriptKind::Failure,
                    "unknown effect acknowledged failed; run aborted".into(),
                    Some(
                        serde_json::json!({"effect_id":redact_thread_text(effect_id),"status":"reconciled"}),
                    ),
                    format!("{source_key}:card"),
                ));
                checkpoint = Some(checkpoint_json.clone());
            }
            CommitThreadRunUpdate::RequestPermission {
                source_key,
                request: pending,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::RequestPermission(redact_permission(pending)),
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                run_changed = true;
                next_lifecycle = "waiting_permission".into();
                card = Some((
                    TranscriptKind::Permission,
                    next.pending_permission.as_ref().map_or_else(
                        || "permission requested".into(),
                        |value| redact_thread_text(&value.description),
                    ),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitThreadRunUpdate::ResolvePermission {
                source_key,
                request_id,
                allow,
                rebound_operation_digest,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::ResolvePermission {
                            request_id: redact_thread_text(request_id),
                            allowed: *allow,
                        },
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                run_changed = true;
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
                            "SELECT p.run_revision,p.lease_owner,p.lease_token,\
                                    p.approval_digest,e.approval_digest \
                             FROM pending_permissions p \
                             JOIN effects e ON e.effect_id=p.effect_id AND e.run_id=p.run_id \
                             WHERE p.effect_id=?1 AND p.run_id=?2 \
                               AND p.consumed_at_ms IS NULL AND e.status='prepared'",
                            params![request_id, request.run_id.to_string()],
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
                            validate_thread_digest(digest)?;
                            let effect_changed = tx.execute(
                                "UPDATE effects SET approval_digest=?1 \
                                 WHERE effect_id=?2 AND run_id=?3 AND status='prepared' \
                                   AND approval_digest=?4",
                                params![
                                    digest,
                                    request_id,
                                    request.run_id.to_string(),
                                    effect_digest
                                ],
                            )?;
                            let permission_changed = tx.execute(
                                "UPDATE pending_permissions \
                                 SET lease_owner=?1,lease_token=?2,approval_digest=?3 \
                                 WHERE effect_id=?4 AND run_id=?5 AND run_revision=?6 \
                                   AND approval_digest=?7 AND consumed_at_ms IS NULL",
                                params![
                                    lease.owner,
                                    to_i64(lease.fencing_token)?,
                                    digest,
                                    request_id,
                                    request.run_id.to_string(),
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
                        "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND consumed_at_ms IS NULL",
                        params![to_i64(now_ms)?, request_id, request.run_id.to_string()],
                    )?;
                    tx.execute(
                        r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"permission":"denied_before_start"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='prepared'"#,
                        params![to_i64(now_ms)?, request_id, request.run_id.to_string()],
                    )?;
                }
            }
            CommitThreadRunUpdate::RequestInput {
                source_key,
                request: pending,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::RequestInput(redact_input(pending)),
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                run_changed = true;
                next_lifecycle = "waiting_input".into();
                card = Some((
                    TranscriptKind::Input,
                    next.pending_input.as_ref().map_or_else(
                        || "input requested".into(),
                        |value| redact_thread_text(&value.prompt),
                    ),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitThreadRunUpdate::ProvideInput {
                source_key,
                request_id,
                value,
            } => {
                next = current
                    .transition(
                        current.revision,
                        Transition::ProvideInput {
                            request_id: redact_thread_text(request_id),
                        },
                    )
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                run_changed = true;
                next_lifecycle = "running".into();
                card = Some((
                    TranscriptKind::User,
                    redact_thread_text(value),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitThreadRunUpdate::Complete {
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
                run_changed = true;
                terminal = true;
                next_lifecycle = "ready".into();
                card = Some((
                    TranscriptKind::Completion,
                    next.handoff.as_ref().map_or_else(
                        || "completed".into(),
                        |value| redact_thread_text(&value.summary),
                    ),
                    next.handoff
                        .as_ref()
                        .map(|handoff| serde_json::json!({"handoff": redact_handoff(handoff)})),
                    format!("{source_key}:card"),
                ));
            }
            CommitThreadRunUpdate::CompleteVerified {
                source_key,
                summary,
                verification_effect_id,
                verified_manifest_digest,
                files_changed,
            } => {
                let effect_epoch = from_i64(tx.query_row(
                    "SELECT effect_epoch FROM runs WHERE run_id=?1",
                    [request.run_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )?)?;
                let metadata: Option<String> = tx
                    .query_row(
                        "SELECT metadata_json FROM evidence WHERE run_id=?1 ORDER BY rowid DESC LIMIT 1",
                        [request.run_id.to_string()],
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
                    summary: redact_thread_text(summary),
                    files_changed: files_changed
                        .iter()
                        .map(|path| redact_thread_text(path))
                        .collect(),
                    evidence: vec![Evidence {
                        name: format!(
                            "verification: {}",
                            redact_thread_text(verification_effect_id)
                        ),
                        status: VerificationStatus::Passed,
                        summary: format!(
                            "{}; verified_manifest_sha256={}; verified_at_ms={now_ms}; change_source=manifest_v1",
                            redact_thread_text(&record.summary),
                            redact_thread_text(verified_manifest_digest),
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
                run_changed = true;
                terminal = true;
                next_lifecycle = "ready".into();
                card = Some((
                    TranscriptKind::Completion,
                    next.handoff.as_ref().map_or_else(
                        || "completed".into(),
                        |value| redact_thread_text(&value.summary),
                    ),
                    next.handoff
                        .as_ref()
                        .map(|handoff| serde_json::json!({"handoff": redact_handoff(handoff)})),
                    format!("{source_key}:card"),
                ));
            }
            CommitThreadRunUpdate::Fail {
                source_key,
                failure,
            } => {
                let retryable = failure.retryability == Retryability::Retryable;
                next = current
                    .transition(current.revision, Transition::Fail(redact_failure(failure)))
                    .map_err(|e| StorageError::InvalidData(e.to_string()))?;
                run_changed = true;
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
                        |value| redact_thread_text(&value.message),
                    ),
                    None,
                    format!("{source_key}:card"),
                ));
            }
            CommitThreadRunUpdate::Interrupt {
                source_key,
                reconciliation_effect_id,
            } => {
                reconciliation_effect = reconciliation_effect_id
                    .as_ref()
                    .map(|value| redact_thread_text(value));
                if matches!(
                    current.status,
                    RunStatus::WaitingPermission | RunStatus::WaitingInput
                ) {
                    // Waiting requests have not started an external effect. The
                    // mandatory cancellation mapping is terminal Cancelled,
                    // not an ambiguous interruption.
                    next.revision = current
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
                    next.status = RunStatus::Failed;
                    next.pending_input = None;
                    next.pending_permission = None;
                    next.failure = Some(RunFailure {
                        code: FailureCode::Cancelled,
                        message: "run cancelled while waiting".into(),
                        retryability: Retryability::Terminal,
                    });
                    reconciliation_effect = None;
                    next_lifecycle = "failed".into();
                    if let Some(permission) = current.pending_permission.as_ref() {
                        tx.execute(
                            "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND consumed_at_ms IS NULL",
                            params![to_i64(now_ms)?, permission.request_id, request.run_id.to_string()],
                        )?;
                        tx.execute(
                            r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"cancelled":"before_start"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='prepared'"#,
                            params![to_i64(now_ms)?, permission.request_id, request.run_id.to_string()],
                        )?;
                    }
                } else {
                    // A durable Started record is proof that an external
                    // call may already have happened.  A generic cancellation
                    // must therefore discover it and turn the thread into a
                    // reconciliation case rather than claiming interruption.
                    if reconciliation_effect.is_none() {
                        reconciliation_effect = tx
                            .query_row(
                                "SELECT effect_id FROM effects WHERE run_id=?1 AND status='started' ORDER BY rowid DESC LIMIT 1",
                                [request.run_id.to_string()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?;
                    }
                    if let Some(effect_id) = reconciliation_effect.as_ref() {
                        tx.execute(
                            r#"UPDATE effects SET status='unknown',post_evidence_json='{"outcome":"cancelled_after_start"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='started'"#,
                            params![to_i64(now_ms)?, effect_id, request.run_id.to_string()],
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
                run_changed = true;
                terminal = true;
                card = Some((
                    TranscriptKind::Failure,
                    if reconciliation_effect.is_some() {
                        "effect outcome unknown; reconciliation required".into()
                    } else {
                        "run interrupted".into()
                    },
                    None,
                    format!("{source_key}:card"),
                ));
            }
        }
        let next_thread_revision = thread_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("thread revision overflow".into()))?;
        let next_sequence = from_i64(last_seq)?
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("thread sequence overflow".into()))?;
        if run_changed {
            append_linked_run_transition(&tx, &current, &next, from_i64(run_seq)?, lease, now_ms)?;
        } else {
            let changed = tx.execute(
                "UPDATE runs SET lease_token=?1,updated_at_ms=?2 \
                 WHERE run_id=?3 AND lease_token<=?1",
                params![
                    to_i64(lease.fencing_token)?,
                    to_i64(now_ms)?,
                    request.run_id.to_string()
                ],
            )?;
            if changed != 1 {
                return Err(StorageError::LeaseLost);
            }
        }
        if let Some(payload) = checkpoint {
            tx.execute(
                "INSERT INTO runtime_checkpoints(run_id,payload_json,updated_at_ms) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO UPDATE SET payload_json=excluded.payload_json,updated_at_ms=excluded.updated_at_ms",
                params![request.run_id.to_string(), payload, to_i64(now_ms)?],
            )?;
        }
        let transcript = if let Some((kind, text, payload, source_key)) = card {
            let entry = TranscriptEntry {
                entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
                sequence: next_sequence,
                run_id: Some(request.run_id),
                kind,
                text,
                payload,
                source_key,
                created_at_ms: now_ms,
            };
            tx.execute("INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![request.thread_id.to_string(),to_i64(next_sequence)?,entry.entry_id.to_string(),request.run_id.to_string(),transcript_kind_name(entry.kind),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(now_ms)?])?;
            Some(entry)
        } else {
            None
        };
        if terminal {
            tx.execute(
                "UPDATE thread_runs_v2 SET completed_at_ms=?1 WHERE run_id=?2",
                params![to_i64(now_ms)?, request.run_id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM thread_active_runs_v2 WHERE thread_id=?1 AND run_id=?2",
                params![request.thread_id.to_string(), request.run_id.to_string()],
            )?;
        } else {
            tx.execute(
                "UPDATE thread_active_runs_v2 SET lease_token=?1 WHERE thread_id=?2 AND run_id=?3",
                params![
                    to_i64(lease.fencing_token)?,
                    request.thread_id.to_string(),
                    request.run_id.to_string()
                ],
            )?;
        }
        tx.execute("UPDATE threads_v2 SET revision=?1,last_seq=?2,lifecycle=?3,latest_run_id=?4,updated_at_ms=?5 WHERE thread_id=?6",params![to_i64(next_thread_revision)?,to_i64(next_sequence)?,next_lifecycle,request.run_id.to_string(),to_i64(now_ms)?,request.thread_id.to_string()])?;
        let event = if let Some(entry) = transcript {
            ThreadEvent::TranscriptAppended { entry }
        } else if let Some(effect_id) = reconciliation_effect {
            ThreadEvent::ReconciliationRequired {
                run_id: request.run_id,
                effect_id,
            }
        } else {
            ThreadEvent::LifecycleChanged {
                lifecycle: parse_lifecycle(&next_lifecycle)?,
                run_id: Some(request.run_id),
            }
        };
        let envelope = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
            thread_id: request.thread_id,
            revision: next_thread_revision,
            sequence: next_sequence,
            event,
        };
        tx.execute("INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![request.thread_id.to_string(),to_i64(next_sequence)?,envelope.event_id.to_string(),to_i64(next_thread_revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let response = ThreadCommitResponse {
            snapshot: current_thread_snapshot(
                &tx,
                request.thread_id,
                THREAD_PROJECTION_TRANSCRIPT_LIMIT,
            )?,
            thread_event: StoredThreadEvent {
                sequence: next_sequence,
                envelope,
            },
        };
        let response_json = serde_json::to_string(&response).map_err(invalid_json)?;
        tx.execute("INSERT INTO thread_command_dedup_v2(command_id,digest,result_json,created_at_ms) VALUES(?1,?2,?3,?4)",params![request.command_id.to_string(),digest,response_json,to_i64(now_ms)?])?;
        tx.execute("INSERT INTO thread_commit_sources_v2(thread_id,source_key,digest,result_json) VALUES(?1,?2,?3,?4)",params![request.thread_id.to_string(),request.update.source_key(),thread_command_digest(request)?,serde_json::to_string(&response).map_err(invalid_json)?])?;
        tx.commit()?;
        Ok(response)
    }

    #[cfg(test)]
    pub(crate) fn append_event(
        &self,
        next: &RunState,
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
                "SELECT revision,last_seq,lease_token FROM runs WHERE run_id=?1",
                [next.run_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(StorageError::RunNotFound(next.run_id))?;
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
            run_id: next.run_id,
            revision: next.revision,
            event: event.clone(),
        };
        let state_json = serde_json::to_string(next).map_err(invalid_json)?;
        let event_json = serde_json::to_string(&envelope).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![next.run_id.to_string(), to_i64(sequence)?, event_id.to_string(), to_i64(next.revision)?, event_json, to_i64(now_ms)?])?;
        let changed = tx.execute("UPDATE runs SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE run_id=?7 AND revision=?8",
            params![state_json, status_name(next.status), to_i64(next.revision)?, to_i64(sequence)?, to_i64(lease.fencing_token)?, to_i64(now_ms)?, next.run_id.to_string(), to_i64(expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual,
            });
        }
        tx.execute(
            "UPDATE run_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE run_id=?4",
            params![
                to_i64(next.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(next).map_err(invalid_json)?,
                next.run_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(StoredEvent { sequence, envelope })
    }
    pub(crate) fn apply_transition(
        &self,
        run_id: RunId,
        expected_revision: u64,
        transition: Transition,
        now_ms: u64,
        lease: &Lease,
    ) -> Result<(RunState, StoredEvent), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM runs WHERE run_id=?1",
            [run_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let current: RunState = serde_json::from_str(&json).map_err(invalid_json)?;
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
            run_id,
            revision: next.revision,
            event,
        };
        let state_json = serde_json::to_string(&next).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![run_id.to_string(),to_i64(sequence)?,envelope.event_id.to_string(),to_i64(next.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE runs SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE run_id=?7 AND revision=?8",params![state_json,status_name(next.status),to_i64(next.revision)?,to_i64(sequence)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,run_id.to_string(),to_i64(expected_revision)?])?;
        tx.execute(
            "UPDATE run_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE run_id=?4",
            params![
                to_i64(next.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(&next).map_err(invalid_json)?,
                run_id.to_string()
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

    pub(crate) fn acquire_run_lease(
        &self,
        run_id: RunId,
        owner: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        self.acquire_scoped_lease("runtime", owner, Some(run_id), now_ms, ttl_ms)
    }

    pub(crate) fn acquire_thread_lease(
        &self,
        thread_id: latte_core::ThreadId,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Lease, StorageError> {
        let scope = thread_lease_scope(thread_id);
        let owner = format!("thread-{thread_id}-{}", Uuid::now_v7());
        self.acquire_scoped_lease(&scope, &owner, None, now_ms, ttl_ms)
    }

    fn acquire_scoped_lease(
        &self,
        scope: &str,
        owner: &str,
        run_id: Option<RunId>,
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
            if let Some(run_id) = run_id {
                let changed = tx.execute(
                    "UPDATE runs SET lease_token=?1 WHERE run_id=?2 \
                     AND NOT EXISTS(SELECT 1 FROM thread_runs_v2 WHERE thread_runs_v2.run_id=runs.run_id)",
                    params![to_i64(token)?, run_id.to_string()],
                )?;
                if changed != 1 {
                    let exists: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM runs WHERE run_id=?1)",
                        [run_id.to_string()],
                        |row| row.get(0),
                    )?;
                    return Err(if exists {
                        StorageError::LinkedRunRequiresThreadCommit
                    } else {
                        StorageError::RunNotFound(run_id)
                    });
                }
            } else {
                tx.execute(
                    "UPDATE runs SET lease_token=?1 WHERE status='queued' \
                     AND NOT EXISTS(SELECT 1 FROM thread_runs_v2 WHERE thread_runs_v2.run_id=runs.run_id)",
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
    ) -> Result<Option<ThreadCommitResponse>, StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut recovery = None;
        if let Some(thread) = lease.scope.strip_prefix("thread:") {
            let thread_id = parse_thread_id(thread)?;
            let active_run: Option<(String, i64)> = tx
                .query_row(
                    "SELECT run_id,lease_token FROM thread_active_runs_v2 WHERE thread_id=?1",
                    [thread_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            // Returning from a durable input/permission wait is a clean
            // quiescence boundary, not a coordinator crash. Token zero means
            // that no writer is authoritative and lets a later coordinator
            // acquire a fresh global epoch immediately. Nonzero stale tokens
            // remain distinguishable for conservative startup recovery.
            let quiesced = tx.execute(
                "UPDATE runs SET lease_token=0 \
                 WHERE run_id=(SELECT run_id FROM thread_active_runs_v2 \
                               WHERE thread_id=?1 AND lease_token=?2) \
                   AND lease_token=?2 \
                   AND status IN ('waiting_permission','waiting_input')",
                params![thread_id.to_string(), to_i64(lease.fencing_token)?],
            )?;
            if quiesced == 1 {
                let active_quiesced = tx.execute(
                    "UPDATE thread_active_runs_v2 SET lease_token=0 \
                     WHERE thread_id=?1 AND lease_token=?2",
                    params![thread_id.to_string(), to_i64(lease.fencing_token)?],
                )?;
                if active_quiesced != 1 {
                    return Err(StorageError::LeaseLost);
                }
            } else if let Some((run_id, active_token)) = active_run
                && from_i64(active_token)? == lease.fencing_token
            {
                recovery = recover_linked_thread_run(
                    &tx,
                    thread_id,
                    parse_run_id(&run_id)?,
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
        run_id: RunId,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute(
            "INSERT INTO effects(effect_id,run_id,status,started_at_ms) VALUES(?1,?2,'started',?3)",
            params![id, run_id.to_string(), to_i64(now_ms)?],
        )?;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn declare_effect(
        &self,
        id: &str,
        run_id: RunId,
        attempt: u64,
        descriptor_json: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        serde_json::from_str::<serde_json::Value>(descriptor_json).map_err(invalid_json)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute("INSERT INTO effects(effect_id,run_id,status,started_at_ms,attempt,descriptor_json) VALUES(?1,?2,'declared',?3,?4,?5)",params![id,run_id.to_string(),to_i64(now_ms)?,to_i64(attempt)?,descriptor_json])?;
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
        let changed=tx.execute("UPDATE effects SET status=?1,post_evidence_json=?2,observed_at_ms=?3 WHERE effect_id=?4 AND run_id=?5 AND status='started' AND attempt=?6 AND approval_digest=?7 AND EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?8 AND owner=?9 AND fencing_token=?10 AND expires_at_ms>?3) AND EXISTS(SELECT 1 FROM runs WHERE run_id=?5 AND revision=?11 AND lease_token=?10)",params![status,post_evidence_json,to_i64(now_ms)?,authority.effect_id,authority.run_id.to_string(),to_i64(authority.attempt)?,authority.digest,authority.lease.scope,authority.lease.owner,to_i64(authority.lease.fencing_token)?,to_i64(authority.expected_revision)?])?;
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
        let changed=tx.execute("UPDATE effects SET status='unknown',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='started' AND attempt=?4 AND approval_digest=?5 AND EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?6 AND owner=?7 AND fencing_token=?8 AND expires_at_ms>?1) AND EXISTS(SELECT 1 FROM runs WHERE run_id=?3 AND revision=?9 AND lease_token=?8)",params![to_i64(now_ms)?,authority.effect_id,authority.run_id.to_string(),to_i64(authority.attempt)?,authority.digest,authority.lease.scope,authority.lease.owner,to_i64(authority.lease.fencing_token)?,to_i64(authority.expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::EffectFenced);
        }
        tx.commit()?;
        Ok(())
    }
    #[cfg(test)]
    fn seed_unknown_for_recovery_test(&self, id: &str) -> Result<(), StorageError> {
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
        run_id: RunId,
        run_revision: u64,
        lease: &Lease,
        digest: &str,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.execute("INSERT INTO pending_permissions(effect_id,run_id,run_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![effect_id,run_id.to_string(),to_i64(run_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn create_prepared_permission(
        &self,
        effect_id: &str,
        run_id: RunId,
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
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease l JOIN runs r ON r.run_id=?1 WHERE l.scope=?2 AND l.owner=?3 AND l.fencing_token=?4 AND l.expires_at_ms>?5 AND r.revision=?6 AND r.lease_token=?4)",params![run_id.to_string(),lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?,to_i64(expected_revision)?],|r|r.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        tx.execute("INSERT INTO effects(effect_id,run_id,status,started_at_ms,attempt,descriptor_json,approval_digest,pre_evidence_json,prepared_at_ms) VALUES(?1,?2,'prepared',?3,?4,?5,?6,'{}',?3)",params![effect_id,run_id.to_string(),to_i64(now_ms)?,to_i64(attempt)?,descriptor,digest])?;
        tx.execute("INSERT INTO pending_permissions(effect_id,run_id,run_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![effect_id,run_id.to_string(),to_i64(bound_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn consume_permission_and_start(
        &self,
        effect_id: &str,
        run_id: RunId,
        run_revision: u64,
        lease: &Lease,
        digest: &str,
        now_ms: u64,
    ) -> Result<EffectAuthority, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND run_revision=?4 AND lease_owner=?5 AND lease_token=?6 AND approval_digest=?7 AND consumed_at_ms IS NULL AND EXISTS(SELECT 1 FROM runs WHERE run_id=?3 AND revision=?4) AND EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?8 AND owner=?5 AND fencing_token=?6 AND expires_at_ms>?1)",params![to_i64(now_ms)?,effect_id,run_id.to_string(),to_i64(run_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest,lease.scope])?;
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
            "UPDATE runs SET effect_epoch=effect_epoch+1 WHERE run_id=?1 AND revision=?2 AND lease_token=?3",
            params![run_id.to_string(), to_i64(run_revision)?, to_i64(lease.fencing_token)?],
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
            run_id,
            expected_revision: run_revision,
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
        run_id: RunId,
        run_revision: u64,
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
        let expected_revision = run_revision.saturating_sub(2);
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease l JOIN runs r ON r.run_id=?1 JOIN pending_permissions p ON p.run_id=r.run_id JOIN effects e ON e.effect_id=p.effect_id AND e.run_id=p.run_id WHERE l.scope=?2 AND l.owner=?3 AND l.fencing_token=?4 AND l.expires_at_ms>?5 AND r.revision=?6 AND r.lease_token=?4 AND p.effect_id=?7 AND p.consumed_at_ms IS NULL AND e.status='prepared' AND e.approval_digest=p.approval_digest AND (p.lease_owner<>?3 OR p.lease_token<>?4))",params![run_id.to_string(),lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?,to_i64(expected_revision)?,old_effect_id],|r|r.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let deleted = tx.execute("DELETE FROM pending_permissions WHERE effect_id=?1 AND run_id=?2 AND consumed_at_ms IS NULL AND EXISTS(SELECT 1 FROM effects e WHERE e.effect_id=?1 AND e.run_id=?2 AND e.status='prepared' AND e.approval_digest=pending_permissions.approval_digest)", params![old_effect_id,run_id.to_string()])?;
        let abandoned = tx.execute("UPDATE effects SET status='observed_failed',post_evidence_json='{\"abandoned\":\"lease_changed\"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='prepared'",params![to_i64(now_ms)?,old_effect_id,run_id.to_string()])?;
        if deleted != 1 || abandoned != 1 {
            return Err(StorageError::InvalidData(
                "old pending effect cannot be replaced".into(),
            ));
        }
        tx.execute("INSERT INTO effects(effect_id,run_id,status,started_at_ms,attempt,descriptor_json,approval_digest,pre_evidence_json,prepared_at_ms) VALUES(?1,?2,'prepared',?3,?4,?5,?6,'{}',?3)",params![new_effect_id,run_id.to_string(),to_i64(now_ms)?,to_i64(attempt)?,descriptor_json,digest])?;
        tx.execute("INSERT INTO pending_permissions(effect_id,run_id,run_revision,lease_owner,lease_token,approval_digest) VALUES(?1,?2,?3,?4,?5,?6)",params![new_effect_id,run_id.to_string(),to_i64(run_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn permission_matches(
        &self,
        effect_id: &str,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        digest: &str,
        now_ms: u64,
    ) -> Result<bool, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let conn = self.connection.lock().expect("storage mutex poisoned");
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM pending_permissions p JOIN runs r ON r.run_id=p.run_id JOIN runtime_lease l ON l.scope=?4 AND l.owner=?5 WHERE p.effect_id=?1 AND p.run_id=?2 AND p.run_revision=?3 AND p.lease_owner=?5 AND p.lease_token=?6 AND p.approval_digest=?7 AND p.consumed_at_ms IS NULL AND r.revision+1=?3 AND l.fencing_token=?6 AND l.expires_at_ms>?8)",params![effect_id,run_id.to_string(),to_i64(expected_revision)?,lease.scope,lease.owner,to_i64(lease.fencing_token)?,digest,to_i64(now_ms)?],|r|r.get(0))?)
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
    pub(crate) fn thread_effect_digest(&self, effect_id: &str) -> Result<String, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        conn.query_row(
            "SELECT approval_digest FROM effects WHERE effect_id=?1",
            [effect_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }
    /// Returns an exact descriptor only to the engine module. The public
    /// thread snapshot and all event/transcript readers use the independently
    /// redacted projection in `effects.descriptor_json` instead.
    pub(crate) fn thread_effect_canonical_descriptor(
        &self,
        effect_id: &str,
        run_id: RunId,
    ) -> Result<crate::ThreadEffectDescriptor, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let json: String = conn.query_row(
            "SELECT descriptor_json FROM thread_effect_canonical_v2 WHERE effect_id=?1 AND run_id=?2",
            params![effect_id, run_id.to_string()],
            |row| row.get(0),
        )?;
        serde_json::from_str(&json).map_err(invalid_json)
    }
    pub(crate) fn unknown_effects_for_run(
        &self,
        run_id: RunId,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT effect_id FROM effects WHERE run_id=?1 AND status='unknown' ORDER BY effect_id",
        )?;
        Ok(stmt
            .query_map([run_id.to_string()], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Fences a stale v2 coordinator and atomically leaves its linked child
    /// in the same conservative state used by startup recovery.  The caller
    /// must have just failed a renewal; no write here relies on that stale
    /// lease being authoritative.
    pub(crate) fn recover_thread_after_lease_loss(
        &self,
        thread_id: latte_core::ThreadId,
        run_id: RunId,
        lost_lease: &Lease,
        expected_run_revision: u64,
        now_ms: u64,
    ) -> Result<ThreadLeaseLossRecovery, StorageError> {
        let expected_scope = thread_lease_scope(thread_id);
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
                "SELECT state_json FROM runs WHERE run_id=?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state_json) = state_json else {
            return Err(StorageError::RunNotFound(run_id));
        };
        let state: RunState = serde_json::from_str(&state_json).map_err(invalid_json)?;
        if matches!(
            state.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Interrupted
        ) {
            let snapshot =
                current_thread_snapshot(&tx, thread_id, THREAD_PROJECTION_TRANSCRIPT_LIMIT)?;
            tx.commit()?;
            return Ok(ThreadLeaseLossRecovery::AlreadyTerminal(snapshot));
        }
        let response = recover_linked_thread_run(
            &tx,
            thread_id,
            run_id,
            lost_lease.fencing_token,
            Some(expected_run_revision),
            now_ms,
        )?;
        let Some(response) = response else {
            tx.commit()?;
            return Ok(ThreadLeaseLossRecovery::FencedNoop);
        };
        tx.commit()?;
        Ok(ThreadLeaseLossRecovery::Recovered(response))
    }
    pub(crate) fn interrupt_after_lease_loss(
        &self,
        run_id: RunId,
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
        let (json, last_seq, run_token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM runs WHERE run_id=?1",
            [run_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut state: RunState = serde_json::from_str(&json).map_err(invalid_json)?;
        if from_i64(run_token)? != lost_lease.fencing_token || state.revision != expected_revision {
            return Ok(LeaseLossRecovery::FencedNoop);
        }
        if !matches!(state.status, RunStatus::Running | RunStatus::Cancelling) {
            return Ok(LeaseLossRecovery::AlreadyTerminal(state));
        }
        state.status = RunStatus::Interrupted;
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
            run_id,
            revision: state.revision,
            event: RuntimeEvent::StateChanged {
                status: RunStatus::Interrupted,
            },
        };
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![run_id.to_string(),to_i64(seq)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE runs SET state_json=?1,status='interrupted',revision=?2,last_seq=?3,updated_at_ms=?4 WHERE run_id=?5",params![state_json,to_i64(state.revision)?,to_i64(seq)?,to_i64(now_ms)?,run_id.to_string()])?;
        tx.execute(
            "UPDATE run_read_model SET state_json=?1,revision=?2,last_seq=?3 WHERE run_id=?4",
            params![
                serde_json::to_string(&state).map_err(invalid_json)?,
                to_i64(state.revision)?,
                to_i64(seq)?,
                run_id.to_string()
            ],
        )?;
        tx.execute(
            "UPDATE effects SET status='unknown' WHERE run_id=?1 AND status='started'",
            [run_id.to_string()],
        )?;
        tx.commit()?;
        Ok(LeaseLossRecovery::Interrupted(state))
    }
    pub(crate) fn reconcile_unknown_and_abort(
        &self,
        run_id: RunId,
        effect_id: &str,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
    ) -> Result<RunState, StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
        if !authoritative {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM runs WHERE run_id=?1",
            [run_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut state: RunState = serde_json::from_str(&json).map_err(invalid_json)?;
        if state.revision != expected_revision || from_i64(token)? != lease.fencing_token {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: state.revision,
            });
        }
        let changed=tx.execute("UPDATE effects SET status='observed_failed',post_evidence_json='{\"reconciliation\":\"acknowledged_failed\"}',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='unknown'",params![to_i64(now_ms)?,effect_id,run_id.to_string()])?;
        if changed != 1 {
            return Err(StorageError::InvalidData(
                "unknown effect does not belong to run".into(),
            ));
        }
        state.status = RunStatus::Failed;
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidData("revision overflow".into()))?;
        state.failure = Some(RunFailure {
            code: FailureCode::RuntimeFailed,
            message: format!("unknown effect {effect_id} acknowledged failed; run aborted"),
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
            run_id,
            revision: state.revision,
            event: RuntimeEvent::StateChanged {
                status: RunStatus::Failed,
            },
        };
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![run_id.to_string(),to_i64(seq)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE runs SET state_json=?1,status='failed',revision=?2,last_seq=?3,updated_at_ms=?4 WHERE run_id=?5",params![state_json,to_i64(state.revision)?,to_i64(seq)?,to_i64(now_ms)?,run_id.to_string()])?;
        tx.execute(
            "UPDATE run_read_model SET state_json=?1,revision=?2,last_seq=?3 WHERE run_id=?4",
            params![
                serde_json::to_string(&state).map_err(invalid_json)?,
                to_i64(state.revision)?,
                to_i64(seq)?,
                run_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(state)
    }

    pub(crate) fn record_verification_evidence(
        &self,
        run_id: RunId,
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
        let linked_thread: Option<String> = tx
            .query_row(
                "SELECT thread_id FROM thread_runs_v2 WHERE run_id=?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let expected_scope = linked_thread
            .as_deref()
            .map(parse_thread_id)
            .transpose()?
            .map_or_else(|| LEGACY_RUNTIME_LEASE_SCOPE.to_owned(), thread_lease_scope);
        require_lease_scope(lease, &expected_scope)?;
        let changed = tx.execute(
            "INSERT INTO evidence(id,run_id,metadata_json,blob_ref) \
             SELECT ?1,?2,?3,?4 \
             WHERE EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?5 AND owner=?6 AND fencing_token=?7 AND expires_at_ms>?8) \
             AND EXISTS(SELECT 1 FROM runs WHERE run_id=?2 AND revision=?9 AND effect_epoch=?10 AND lease_token=?7) \
             AND ((?11 IS NULL AND NOT EXISTS(SELECT 1 FROM thread_runs_v2 WHERE run_id=?2)) \
                  OR EXISTS(SELECT 1 FROM thread_runs_v2 tr \
                            JOIN thread_active_runs_v2 ar ON ar.thread_id=tr.thread_id AND ar.run_id=tr.run_id \
                            WHERE tr.run_id=?2 AND tr.thread_id=?11 AND ar.lease_token=?7))",
            params![
                evidence.id,
                run_id.to_string(),
                evidence.metadata_json,
                evidence.blob_ref,
                expected_scope,
                lease.owner,
                to_i64(lease.fencing_token)?,
                to_i64(now_ms)?,
                to_i64(expected_revision)?,
                to_i64(record.effect_epoch)?,
                linked_thread,
            ],
        )?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn effect_epoch(&self, run_id: RunId) -> Result<u64, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let value: i64 = conn.query_row(
            "SELECT effect_epoch FROM runs WHERE run_id=?1",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        from_i64(value)
    }
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn complete_verified(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        summary: String,
        current_manifest: &std::collections::BTreeMap<String, String>,
        manifest_digest: &str,
        now_ms: u64,
    ) -> Result<(RunState, StoredEvent), StorageError> {
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
            "SELECT state_json,last_seq,lease_token,effect_epoch FROM runs WHERE run_id=?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let current: RunState = serde_json::from_str(&json).map_err(invalid_json)?;
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
                "SELECT metadata_json FROM evidence WHERE run_id=?1 ORDER BY rowid DESC LIMIT 1",
                [run_id.to_string()],
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
                "SELECT manifest_json FROM run_baselines WHERE run_id=?1",
                [run_id.to_string()],
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
            run_id,
            revision: next.revision,
            event: RuntimeEvent::HandoffProduced { handoff },
        };
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![run_id.to_string(),to_i64(sequence)?,envelope.event_id.to_string(),to_i64(next.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let state_json = serde_json::to_string(&next).map_err(invalid_json)?;
        let changed = tx.execute("UPDATE runs SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE run_id=?7 AND revision=?8 AND effect_epoch=?9", params![state_json,status_name(next.status),to_i64(next.revision)?,to_i64(sequence)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,run_id.to_string(),to_i64(expected_revision)?,to_i64(epoch)?])?;
        if changed != 1 {
            return Err(StorageError::StaleRevision {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        tx.execute(
            "UPDATE run_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE run_id=?4",
            params![
                to_i64(next.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(&next).map_err(invalid_json)?,
                run_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok((next, StoredEvent { sequence, envelope }))
    }
    pub(crate) fn put_checkpoint(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        payload: &str,
        now_ms: u64,
    ) -> Result<(), StorageError> {
        require_legacy_runtime_lease(lease)?;
        serde_json::from_str::<serde_json::Value>(payload).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("INSERT INTO runtime_checkpoints(run_id,payload_json,updated_at_ms) SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?4 AND owner=?5 AND fencing_token=?6 AND expires_at_ms>?3) AND EXISTS(SELECT 1 FROM runs WHERE run_id=?1 AND revision=?7 AND lease_token=?6) ON CONFLICT(run_id) DO UPDATE SET payload_json=excluded.payload_json,updated_at_ms=excluded.updated_at_ms",params![run_id.to_string(),payload,to_i64(now_ms)?,lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(expected_revision)?])?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn checkpoint(&self, run_id: RunId) -> Result<Option<String>, StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT payload_json FROM runtime_checkpoints WHERE run_id=?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }
    #[allow(clippy::too_many_lines)]
    pub(crate) fn cancel_waiting(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
        denied: bool,
    ) -> Result<(RunState, Option<StoredEvent>), StorageError> {
        require_legacy_runtime_lease(lease)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE scope=?1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4)",params![lease.scope,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|row|row.get(0))?;
        if !valid {
            return Err(StorageError::LeaseLost);
        }
        let (json, last_seq, token): (String, i64, i64) = tx.query_row(
            "SELECT state_json,last_seq,lease_token FROM runs WHERE run_id=?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut state: RunState = serde_json::from_str(&json).map_err(invalid_json)?;
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
            RunStatus::Completed | RunStatus::Failed | RunStatus::Interrupted
        ) {
            tx.commit()?;
            return Ok((state, None));
        }
        if denied && state.status != RunStatus::WaitingPermission {
            return Err(StorageError::InvalidData(
                "run is not waiting for permission".into(),
            ));
        }
        if !matches!(
            state.status,
            RunStatus::WaitingPermission | RunStatus::WaitingInput
        ) {
            return Err(StorageError::InvalidData("run is not waiting".into()));
        }
        if let Some(permission) = state.pending_permission.as_ref() {
            let removed=tx.execute("DELETE FROM pending_permissions WHERE effect_id=?1 AND run_id=?2 AND approval_digest=?3 AND consumed_at_ms IS NULL",params![permission.request_id,run_id.to_string(),permission.operation_digest])?;
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
            let marked=tx.execute("UPDATE effects SET status='observed_failed',post_evidence_json=?1,observed_at_ms=?2 WHERE effect_id=?3 AND run_id=?4 AND status='prepared'",params![evidence,to_i64(now_ms)?,permission.request_id,run_id.to_string()])?;
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
        state.status = RunStatus::Failed;
        state.pending_permission = None;
        state.pending_input = None;
        state.failure = Some(RunFailure {
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
            run_id,
            revision: state.revision,
            event: RuntimeEvent::StateChanged {
                status: RunStatus::Failed,
            },
        };
        let state_json = serde_json::to_string(&state).map_err(invalid_json)?;
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![run_id.to_string(),to_i64(sequence)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE runs SET state_json=?1,status='failed',revision=?2,last_seq=?3,lease_token=?4,updated_at_ms=?5 WHERE run_id=?6 AND revision=?7",params![state_json,to_i64(state.revision)?,to_i64(sequence)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,run_id.to_string(),to_i64(expected_revision)?])?;
        tx.execute(
            "UPDATE run_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE run_id=?4",
            params![
                to_i64(state.revision)?,
                to_i64(sequence)?,
                serde_json::to_string(&state).map_err(invalid_json)?,
                run_id.to_string()
            ],
        )?;
        tx.execute(
            "DELETE FROM runtime_checkpoints WHERE run_id=?1",
            [run_id.to_string()],
        )?;
        tx.commit()?;
        Ok((state, Some(StoredEvent { sequence, envelope })))
    }

    /// Recovers all expired leases and returns the committed thread responses
    /// for every recovered linked child, so the engine can broadcast their
    /// durable events and wake connected SSE clients.
    pub(crate) fn recover_at(
        &self,
        now_ms: u64,
    ) -> Result<Vec<ThreadCommitResponse>, StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Linked children must recover their v1 run, effect ledger, thread
        // projection, transcript, and thread event together.  In particular
        // never let the legacy loop interrupt a v2 child by itself: doing so
        // leaves an active-row/lifecycle pair that no v2 command can safely
        // reconcile.
        let mut recovered = Vec::new();
        let linked_rows = {
            let mut stmt = tx.prepare(
                "SELECT ar.thread_id,ar.run_id,r.lease_token FROM thread_active_runs_v2 ar \
                 JOIN runs r ON r.run_id=ar.run_id \
                 WHERE r.status IN ('queued','running','cancelling','waiting_permission','waiting_input') \
                 AND r.lease_token<>0 \
                 AND NOT EXISTS(SELECT 1 FROM runtime_lease l WHERE l.scope='thread:'||ar.thread_id AND l.fencing_token=r.lease_token AND l.expires_at_ms>?1)",
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
        for (thread, run, token) in linked_rows {
            let thread_id = parse_thread_id(&thread)?;
            let run_id = uuid::Uuid::parse_str(&run)
                .map(RunId::from_uuid)
                .map_err(|error| {
                    StorageError::InvalidData(format!("invalid stored run id: {error}"))
                })?;
            if let Some(response) =
                recover_linked_thread_run(&tx, thread_id, run_id, from_i64(token)?, None, now_ms)?
            {
                recovered.push(response);
            }
        }
        let mut stmt = tx.prepare(
            "SELECT r.run_id,r.state_json,r.last_seq FROM runs r WHERE r.status IN ('running','cancelling') AND NOT EXISTS(SELECT 1 FROM thread_runs_v2 tr WHERE tr.run_id=r.run_id) AND NOT EXISTS(SELECT 1 FROM runtime_lease l WHERE l.scope='runtime' AND l.fencing_token=r.lease_token AND l.expires_at_ms>?1)",
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
            let mut state: RunState = serde_json::from_str(&json).map_err(invalid_json)?;
            state.status = RunStatus::Interrupted;
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
                run_id: state.run_id,
                revision: state.revision,
                event: RuntimeEvent::StateChanged {
                    status: RunStatus::Interrupted,
                },
            };
            tx.execute(
                "INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, to_i64(sequence)?, envelope.event_id.to_string(), to_i64(state.revision)?, serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?],
            )?;
            tx.execute(
                "UPDATE runs SET state_json=?1,status='interrupted',revision=?2,last_seq=?3 WHERE run_id=?4",
                params![json, to_i64(state.revision)?, to_i64(sequence)?, id],
            )?;
            tx.execute(
                "UPDATE run_read_model SET state_json=?1,revision=?2,last_seq=?3 WHERE run_id=?4",
                params![
                    serde_json::to_string(&state).map_err(invalid_json)?,
                    to_i64(state.revision)?,
                    to_i64(sequence)?,
                    id
                ],
            )?;
            tx.execute(
                "UPDATE effects SET status='unknown' WHERE run_id=?1 AND status='started'",
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
fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::WaitingPermission => "waiting_permission",
        RunStatus::WaitingInput => "waiting_input",
        RunStatus::Cancelling => "cancelling",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Failed => "failed",
        RunStatus::Completed => "completed",
    }
}

fn parse_thread_id(value: &str) -> Result<latte_core::ThreadId, StorageError> {
    uuid::Uuid::parse_str(value)
        .map(latte_core::ThreadId::from_uuid)
        .map_err(|error| StorageError::InvalidData(format!("invalid stored thread id: {error}")))
}

fn parse_run_id(value: &str) -> Result<RunId, StorageError> {
    uuid::Uuid::parse_str(value)
        .map(RunId::from_uuid)
        .map_err(|error| StorageError::InvalidData(format!("invalid stored run id: {error}")))
}

fn thread_lease_scope(thread_id: latte_core::ThreadId) -> String {
    format!("thread:{thread_id}")
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

fn parse_lifecycle(value: &str) -> Result<ThreadLifecycle, StorageError> {
    match value {
        "ready" => Ok(ThreadLifecycle::Ready),
        "running" => Ok(ThreadLifecycle::Running),
        "waiting_permission" => Ok(ThreadLifecycle::WaitingPermission),
        "waiting_input" => Ok(ThreadLifecycle::WaitingInput),
        "interrupted" => Ok(ThreadLifecycle::Interrupted),
        "failed" => Ok(ThreadLifecycle::Failed),
        "reconciliation_required" => Ok(ThreadLifecycle::ReconciliationRequired),
        _ => Err(StorageError::InvalidData(format!(
            "invalid thread lifecycle {value}"
        ))),
    }
}

fn thread_run_status(status: RunStatus) -> ThreadRunStatus {
    match status {
        RunStatus::Queued => ThreadRunStatus::Queued,
        RunStatus::Running => ThreadRunStatus::Running,
        RunStatus::Cancelling => ThreadRunStatus::Cancelling,
        RunStatus::WaitingPermission => ThreadRunStatus::WaitingPermission,
        RunStatus::WaitingInput => ThreadRunStatus::WaitingInput,
        RunStatus::Interrupted => ThreadRunStatus::Interrupted,
        RunStatus::Failed => ThreadRunStatus::Failed,
        RunStatus::Completed => ThreadRunStatus::Completed,
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
    }
}

#[allow(clippy::too_many_lines)]
fn thread_snapshot(
    connection: &Connection,
    thread_id: latte_core::ThreadId,
    after: Option<u64>,
    limit: usize,
) -> Result<ThreadSnapshot, StorageError> {
    let (revision, sequence, lifecycle, binding_json, latest, focus): (i64, i64, String, String, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT revision,last_seq,lifecycle,binding_json,latest_run_id,focus FROM threads_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()?
        .ok_or(StorageError::ThreadNotFound(thread_id))?;
    let binding = serde_json::from_str(&binding_json).map_err(invalid_json)?;
    let latest_run_id = latest
        .as_deref()
        .map(|value| uuid::Uuid::parse_str(value).map(RunId::from_uuid))
        .transpose()
        .map_err(|error| StorageError::InvalidData(format!("invalid stored run id: {error}")))?;
    let active_run_id: Option<String> = connection
        .query_row(
            "SELECT run_id FROM thread_active_runs_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let active_run_id = active_run_id
        .as_deref()
        .map(|value| uuid::Uuid::parse_str(value).map(RunId::from_uuid))
        .transpose()
        .map_err(|error| StorageError::InvalidData(format!("invalid active run id: {error}")))?;
    let mut statement = connection.prepare(
        "SELECT tr.run_id,tr.parent_run_id,tr.ordinal,tr.completed_at_ms,r.state_json
         FROM thread_runs_v2 tr JOIN runs r ON r.run_id=tr.run_id
         WHERE tr.thread_id=?1 ORDER BY tr.ordinal ASC",
    )?;
    let runs = statement
        .query_map([thread_id.to_string()], |row| {
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
            let state: RunState = serde_json::from_str(&state_json).map_err(invalid_json)?;
            let run_id = uuid::Uuid::parse_str(&run)
                .map(RunId::from_uuid)
                .map_err(|error| {
                    StorageError::InvalidData(format!("invalid stored run id: {error}"))
                })?;
            let parent_run_id = parent
                .as_deref()
                .map(|value| uuid::Uuid::parse_str(value).map(RunId::from_uuid))
                .transpose()
                .map_err(|error| {
                    StorageError::InvalidData(format!("invalid parent run id: {error}"))
                })?;
            Ok(ThreadRunSummary {
                run_id,
                parent_run_id,
                ordinal: from_i64(ordinal)?,
                status: thread_run_status(state.status),
                run_revision: state.revision,
                completed_at_ms: completed.map(from_i64).transpose()?,
                failure_code: state.failure.map(|failure| failure.code),
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    let bounded_limit = limit.clamp(1, 500);
    let query_after = after.unwrap_or(0);
    let mut entries = connection
        .prepare(
            "SELECT entry_json FROM conversation_outbox WHERE thread_id=?1 AND seq>?2 ORDER BY seq ASC LIMIT ?3",
        )?
        .query_map(
            params![thread_id.to_string(), to_i64(query_after)?, i64::try_from(bounded_limit + 1).map_err(|_| StorageError::InvalidData("page limit overflow".into()))?],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| row.map_err(StorageError::from).and_then(|json| serde_json::from_str(&json).map_err(invalid_json)))
        .collect::<Result<Vec<TranscriptEntry>, StorageError>>()?;
    let has_more = entries.len() > bounded_limit;
    entries.truncate(bounded_limit);
    let next_after = entries.last().map(|entry| entry.sequence);
    let pending = active_run_id.and_then(|active| {
        runs.iter()
            .find(|run| run.run_id == active)
            .and_then(|summary| {
                let state_json: Option<String> = connection
                    .query_row(
                        "SELECT state_json FROM runs WHERE run_id=?1",
                        [active.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let state =
                    state_json.and_then(|json| serde_json::from_str::<RunState>(&json).ok())?;
                if let Some(permission) = state.pending_permission {
                    Some(ThreadPendingRequest::Permission {
                        run_id: active,
                        request_id: redact_thread_text(&permission.request_id),
                        description: redact_thread_text(&permission.description),
                        expected_run_revision: summary.run_revision,
                    })
                } else {
                    state
                        .pending_input
                        .map(|input| ThreadPendingRequest::Input {
                            run_id: active,
                            request_id: redact_thread_text(&input.request_id),
                            prompt: redact_thread_text(&input.prompt),
                            expected_run_revision: summary.run_revision,
                        })
                }
            })
    });
    Ok(ThreadSnapshot {
        thread_id,
        revision: from_i64(revision)?,
        sequence: from_i64(sequence)?,
        lifecycle: parse_lifecycle(&lifecycle)?,
        binding,
        latest_run_id,
        active_run_id,
        pending,
        runs,
        transcript: TranscriptPage {
            entries,
            next_after,
            has_more,
        },
        focus,
    })
}

fn current_thread_snapshot(
    connection: &Connection,
    thread_id: latte_core::ThreadId,
    transcript_limit: usize,
) -> Result<ThreadSnapshot, StorageError> {
    let mut snapshot = thread_snapshot(connection, thread_id, None, 1)?;
    snapshot.transcript = thread_transcript_tail(connection, thread_id, transcript_limit)?;
    Ok(snapshot)
}

/// Loads the newest bounded transcript page for a presentation projection.
///
/// `has_more` here means that older cards were deliberately omitted. The
/// normal forward `thread_snapshot` paging API remains unchanged for durable
/// history/restart reconstruction. Presentation consumers must render an
/// explicit truncation notice whenever this returns `has_more`.
fn thread_transcript_tail(
    connection: &Connection,
    thread_id: latte_core::ThreadId,
    limit: usize,
) -> Result<TranscriptPage, StorageError> {
    let bounded_limit = limit.clamp(1, THREAD_PROJECTION_TRANSCRIPT_LIMIT);
    let mut newest_first = connection
        .prepare(
            "SELECT entry_json FROM conversation_outbox WHERE thread_id=?1 ORDER BY seq DESC LIMIT ?2",
        )?
        .query_map(
            params![
                thread_id.to_string(),
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

fn append_linked_run_transition(
    tx: &rusqlite::Transaction<'_>,
    current: &RunState,
    next: &RunState,
    run_last_seq: u64,
    lease: &Lease,
    now_ms: u64,
) -> Result<(), StorageError> {
    if next.revision <= current.revision {
        return Err(StorageError::InvalidData(
            "linked run transition did not advance".into(),
        ));
    }
    // Cancellation is intentionally represented as the two v1 transitions so
    // a v1 reader never observes a revision jump without its cancelling event.
    let mut states = Vec::new();
    if next.revision == current.revision + 2 && next.status == RunStatus::Interrupted {
        states.push(
            current
                .transition(current.revision, Transition::Cancel)
                .map_err(|error| StorageError::InvalidData(error.to_string()))?,
        );
    }
    states.push(next.clone());
    let mut last_seq = run_last_seq;
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
            run_id: state.run_id,
            revision: state.revision,
            event,
        };
        tx.execute("INSERT INTO events(run_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![state.run_id.to_string(),to_i64(last_seq)?,envelope.event_id.to_string(),to_i64(state.revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        tx.execute("UPDATE runs SET state_json=?1,status=?2,revision=?3,last_seq=?4,lease_token=?5,updated_at_ms=?6 WHERE run_id=?7 AND revision<?3",params![serde_json::to_string(&state).map_err(invalid_json)?,status_name(state.status),to_i64(state.revision)?,to_i64(last_seq)?,to_i64(lease.fencing_token)?,to_i64(now_ms)?,state.run_id.to_string()])?;
        tx.execute(
            "UPDATE run_read_model SET revision=?1,last_seq=?2,state_json=?3 WHERE run_id=?4",
            params![
                to_i64(state.revision)?,
                to_i64(last_seq)?,
                serde_json::to_string(&state).map_err(invalid_json)?,
                state.run_id.to_string()
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
fn recover_linked_thread_run(
    tx: &rusqlite::Transaction<'_>,
    thread_id: latte_core::ThreadId,
    run_id: RunId,
    expected_lease_token: u64,
    expected_run_revision: Option<u64>,
    now_ms: u64,
) -> Result<Option<ThreadCommitResponse>, StorageError> {
    let active: Option<(String, i64)> = tx
        .query_row(
            "SELECT run_id,lease_token FROM thread_active_runs_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((active_run_id, active_token)) = active else {
        return Ok(None);
    };
    if active_run_id != run_id.to_string() || from_i64(active_token)? != expected_lease_token {
        return Ok(None);
    }
    let (state_json, run_last_seq, run_token): (String, i64, i64) = tx.query_row(
        "SELECT state_json,last_seq,lease_token FROM runs WHERE run_id=?1",
        [run_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if from_i64(run_token)? != expected_lease_token {
        return Ok(None);
    }
    let current: RunState = serde_json::from_str(&state_json).map_err(invalid_json)?;
    if expected_run_revision.is_some_and(|expected| expected != current.revision)
        || matches!(
            current.status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Interrupted
        )
    {
        return Ok(None);
    }
    let (thread_revision, thread_last_seq): (i64, i64) = tx
        .query_row(
            "SELECT revision,last_seq FROM threads_v2 WHERE thread_id=?1",
            [thread_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(StorageError::ThreadNotFound(thread_id))?;

    let started_effect_ids = {
        let mut statement = tx.prepare(
            "SELECT effect_id FROM effects WHERE run_id=?1 AND status='started' ORDER BY rowid ASC",
        )?;
        statement
            .query_map([run_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let prepared_effect_ids = {
        let mut statement = tx.prepare(
            "SELECT effect_id FROM effects WHERE run_id=?1 AND status='prepared' ORDER BY rowid ASC",
        )?;
        statement
            .query_map([run_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };

    // A Prepared record has not crossed the external-effect boundary.  It is
    // terminalized as a non-execution, never labelled Unknown.  Started is
    // the sole proof that an external outcome may exist, so every such effect
    // becomes Unknown before the active child is cleared.
    tx.execute(
        "UPDATE pending_permissions SET consumed_at_ms=?1 WHERE run_id=?2 AND consumed_at_ms IS NULL",
        params![to_i64(now_ms)?, run_id.to_string()],
    )?;
    tx.execute(
        r#"UPDATE effects SET status='observed_failed',post_evidence_json='{"recovery":"not_started"}',observed_at_ms=?1 WHERE run_id=?2 AND status='prepared'"#,
        params![to_i64(now_ms)?, run_id.to_string()],
    )?;
    tx.execute(
        r#"UPDATE effects SET status='unknown',post_evidence_json='{"outcome":"lease_lost_after_start"}',observed_at_ms=?1 WHERE run_id=?2 AND status='started'"#,
        params![to_i64(now_ms)?, run_id.to_string()],
    )?;

    // Keep the checkpoint intact: it is the only restart evidence we may
    // retain without asserting an effect outcome.  The v1 interruption is
    // deliberately one event/revision, matching the legacy recovery path.
    let mut interrupted = current.clone();
    interrupted.status = RunStatus::Interrupted;
    interrupted.revision = current
        .revision
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidData("revision overflow during recovery".into()))?;
    interrupted.pending_input = None;
    interrupted.pending_permission = None;
    let stale_fence = Lease {
        scope: thread_lease_scope(thread_id),
        owner: "recovery".into(),
        fencing_token: expected_lease_token,
        expires_at_ms: 0,
    };
    append_linked_run_transition(
        tx,
        &current,
        &interrupted,
        from_i64(run_last_seq)?,
        &stale_fence,
        now_ms,
    )?;
    tx.execute(
        "UPDATE thread_runs_v2 SET completed_at_ms=?1 WHERE thread_id=?2 AND run_id=?3",
        params![to_i64(now_ms)?, thread_id.to_string(), run_id.to_string()],
    )?;
    tx.execute(
        "DELETE FROM thread_active_runs_v2 WHERE thread_id=?1 AND run_id=?2 AND lease_token=?3",
        params![
            thread_id.to_string(),
            run_id.to_string(),
            to_i64(expected_lease_token)?
        ],
    )?;

    let next_thread_revision = from_i64(thread_revision)?.checked_add(1).ok_or_else(|| {
        StorageError::InvalidData("thread revision overflow during recovery".into())
    })?;
    let mut next_sequence = from_i64(thread_last_seq)?;
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
            StorageError::InvalidData("thread sequence overflow during recovery".into())
        })?;
        let entry = TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(Uuid::now_v7()),
            sequence: next_sequence,
            run_id: Some(run_id),
            kind,
            text: redact_thread_text(&text),
            payload: payload.map(redact_thread_value),
            source_key,
            created_at_ms: now_ms,
        };
        tx.execute("INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![thread_id.to_string(),to_i64(next_sequence)?,entry.entry_id.to_string(),run_id.to_string(),transcript_kind_name(entry.kind),entry.source_key,serde_json::to_string(&entry).map_err(invalid_json)?,to_i64(now_ms)?])?;
        let envelope = ThreadEventEnvelope {
            protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
            event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
            thread_id,
            revision: next_thread_revision,
            sequence: next_sequence,
            event: ThreadEvent::TranscriptAppended { entry },
        };
        tx.execute("INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![thread_id.to_string(),to_i64(next_sequence)?,envelope.event_id.to_string(),to_i64(next_thread_revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
        Ok(())
    };
    append_card(
        TranscriptKind::System,
        "lease authority lost; linked run interrupted".into(),
        Some(serde_json::json!({"recovery":"lease_lost"})),
        format!("recovery:{run_id}:{}:interrupted", current.revision),
    )?;
    for (ordinal, effect_id) in started_effect_ids.iter().enumerate() {
        append_card(
            TranscriptKind::Failure,
            "effect outcome unknown; reconciliation required".into(),
            Some(serde_json::json!({
                "effect_id": redact_thread_text(effect_id),
                "status":"unknown"
            })),
            format!("recovery:{run_id}:{}:unknown:{ordinal}", current.revision),
        )?;
    }
    for (ordinal, effect_id) in prepared_effect_ids.iter().enumerate() {
        append_card(
            TranscriptKind::Failure,
            "prepared effect terminalized before external execution".into(),
            Some(serde_json::json!({
                "effect_id": redact_thread_text(effect_id),
                "status":"not_started"
            })),
            format!("recovery:{run_id}:{}:prepared:{ordinal}", current.revision),
        )?;
    }
    next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
        StorageError::InvalidData("thread sequence overflow during recovery".into())
    })?;
    let final_event = if let Some(effect_id) = started_effect_ids.first() {
        ThreadEvent::ReconciliationRequired {
            run_id,
            effect_id: redact_thread_text(effect_id),
        }
    } else {
        ThreadEvent::LifecycleChanged {
            lifecycle: ThreadLifecycle::Interrupted,
            run_id: Some(run_id),
        }
    };
    let envelope = ThreadEventEnvelope {
        protocol_version: latte_core::THREAD_PROTOCOL_VERSION,
        event_id: ThreadEventId::from_uuid(Uuid::now_v7()),
        thread_id,
        revision: next_thread_revision,
        sequence: next_sequence,
        event: final_event,
    };
    tx.execute("INSERT INTO thread_events_v2(thread_id,seq,event_id,revision,event_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![thread_id.to_string(),to_i64(next_sequence)?,envelope.event_id.to_string(),to_i64(next_thread_revision)?,serde_json::to_string(&envelope).map_err(invalid_json)?,to_i64(now_ms)?])?;
    tx.execute(
        "UPDATE threads_v2 SET revision=?1,last_seq=?2,lifecycle=?3,latest_run_id=?4,updated_at_ms=?5 WHERE thread_id=?6",
        params![
            to_i64(next_thread_revision)?,
            to_i64(next_sequence)?,
            lifecycle,
            run_id.to_string(),
            to_i64(now_ms)?,
            thread_id.to_string()
        ],
    )?;
    let snapshot = thread_snapshot(tx, thread_id, None, 100)?;
    Ok(Some(ThreadCommitResponse {
        snapshot,
        thread_event: StoredThreadEvent {
            sequence: next_sequence,
            envelope,
        },
    }))
}

fn redact_permission(value: &latte_core::PendingPermission) -> latte_core::PendingPermission {
    latte_core::PendingPermission {
        request_id: redact_thread_text(&value.request_id),
        operation_digest: redact_thread_text(&value.operation_digest),
        description: redact_thread_text(&value.description),
    }
}
fn redact_input(value: &latte_core::PendingInput) -> latte_core::PendingInput {
    latte_core::PendingInput {
        request_id: redact_thread_text(&value.request_id),
        prompt: redact_thread_text(&value.prompt),
    }
}
fn redact_failure(value: &RunFailure) -> RunFailure {
    RunFailure {
        code: value.code,
        message: redact_thread_text(&value.message),
        retryability: value.retryability,
    }
}
fn redact_handoff(value: &Handoff) -> Handoff {
    Handoff {
        summary: redact_thread_text(&value.summary),
        files_changed: value
            .files_changed
            .iter()
            .map(|path| redact_thread_text(path))
            .collect(),
        evidence: value
            .evidence
            .iter()
            .map(|evidence| Evidence {
                name: redact_thread_text(&evidence.name),
                status: evidence.status,
                summary: redact_thread_text(&evidence.summary),
            })
            .collect(),
    }
}

fn validate_thread_source(source: &str) -> Result<(), StorageError> {
    if source.is_empty() || source.len() > 256 || source.chars().any(char::is_control) {
        return Err(StorageError::InvalidData(
            "invalid thread source key".into(),
        ));
    }
    Ok(())
}

/// Computes the durable digest that binds a session-create command to its
/// complete identity: operation kind, protocol version, workspace, thread,
/// prompt, binding, and normalized focus. Two creates with the same
/// `command_id` but different digests are a replay conflict (409).
fn create_command_digest(
    thread_id: latte_core::ThreadId,
    workspace_root: &str,
    prompt: &str,
    binding: &ThreadProviderBindingV2,
    focus: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::json!({
        "operation": "thread.start",
        "protocol_version": latte_core::THREAD_PROTOCOL_VERSION,
        "workspace": workspace_root,
        "thread_id": thread_id.to_string(),
        "prompt": prompt,
        "binding": binding,
        "focus": focus.map(str::trim).filter(|value| !value.is_empty()),
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).unwrap_or_default())
    )
}

#[allow(clippy::too_many_lines)]
fn thread_command_digest(request: &ThreadCommitRequest) -> Result<String, StorageError> {
    use sha2::{Digest, Sha256};
    let update = match &request.update {
        CommitThreadRunUpdate::Start { source_key } => {
            serde_json::json!({"kind":"start","source_key":redact_thread_text(source_key)})
        }
        CommitThreadRunUpdate::AppendTranscript {
            source_key,
            kind,
            text,
            payload,
        } => {
            serde_json::json!({"kind":"append_transcript","source_key":redact_thread_text(source_key),"card":format!("{kind:?}"),"text":redact_thread_text(text),"payload":payload.clone().map(redact_thread_value)})
        }
        CommitThreadRunUpdate::PrepareEffect {
            source_key,
            effect_id,
            operation_digest,
            descriptor_json,
            canonical_descriptor_json: _,
            policy,
            description,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"prepare_effect","source_key":redact_thread_text(source_key),"effect_id":redact_thread_text(effect_id),"operation_digest":redact_thread_text(operation_digest),"descriptor":serde_json::from_str::<serde_json::Value>(descriptor_json).ok().map(redact_thread_value),"policy":match policy { ThreadEffectPolicy::Allow=>"allow", ThreadEffectPolicy::Ask=>"ask"},"description":redact_thread_text(description),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_thread_value)})
        }
        CommitThreadRunUpdate::StartEffect {
            source_key,
            effect_id,
            operation_digest,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"start_effect","source_key":redact_thread_text(source_key),"effect_id":redact_thread_text(effect_id),"operation_digest":redact_thread_text(operation_digest),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_thread_value)})
        }
        CommitThreadRunUpdate::ObserveEffect {
            source_key,
            effect_id,
            operation_digest,
            success,
            result,
            payload,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"observe_effect","source_key":redact_thread_text(source_key),"effect_id":redact_thread_text(effect_id),"operation_digest":redact_thread_text(operation_digest),"success":success,"result":redact_thread_text(result),"payload":payload.clone().map(redact_thread_value),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_thread_value)})
        }
        CommitThreadRunUpdate::UnknownEffect {
            source_key,
            effect_id,
            operation_digest,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"unknown_effect","source_key":redact_thread_text(source_key),"effect_id":redact_thread_text(effect_id),"operation_digest":redact_thread_text(operation_digest),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_thread_value)})
        }
        CommitThreadRunUpdate::ReconcileUnknownEffect {
            source_key,
            effect_id,
            checkpoint_json,
        } => {
            serde_json::json!({"kind":"reconcile_unknown_effect","source_key":redact_thread_text(source_key),"effect_id":redact_thread_text(effect_id),"checkpoint":serde_json::from_str::<serde_json::Value>(checkpoint_json).ok().map(redact_thread_value)})
        }
        CommitThreadRunUpdate::RequestPermission {
            source_key,
            request,
        } => {
            serde_json::json!({"kind":"request_permission","source_key":redact_thread_text(source_key),"request":redact_permission(request)})
        }
        CommitThreadRunUpdate::ResolvePermission {
            source_key,
            request_id,
            allow,
            rebound_operation_digest,
        } => {
            serde_json::json!({"kind":"resolve_permission","source_key":redact_thread_text(source_key),"request_id":redact_thread_text(request_id),"allow":allow,"rebound_operation_digest":rebound_operation_digest})
        }
        CommitThreadRunUpdate::RequestInput {
            source_key,
            request,
        } => {
            serde_json::json!({"kind":"request_input","source_key":redact_thread_text(source_key),"request":redact_input(request)})
        }
        CommitThreadRunUpdate::ProvideInput {
            source_key,
            request_id,
            value,
        } => {
            serde_json::json!({"kind":"provide_input","source_key":redact_thread_text(source_key),"request_id":redact_thread_text(request_id),"value":redact_thread_text(value)})
        }
        CommitThreadRunUpdate::Complete {
            source_key,
            handoff,
        } => {
            serde_json::json!({"kind":"complete","source_key":redact_thread_text(source_key),"handoff":redact_handoff(handoff)})
        }
        CommitThreadRunUpdate::CompleteVerified {
            source_key,
            summary,
            verification_effect_id,
            verified_manifest_digest,
            files_changed,
        } => {
            serde_json::json!({"kind":"complete_verified","source_key":redact_thread_text(source_key),"summary":redact_thread_text(summary),"verification_effect_id":redact_thread_text(verification_effect_id),"verified_manifest_digest":redact_thread_text(verified_manifest_digest),"files_changed":files_changed.iter().map(|path|redact_thread_text(path)).collect::<Vec<_>>()})
        }
        CommitThreadRunUpdate::Fail {
            source_key,
            failure,
        } => {
            serde_json::json!({"kind":"fail","source_key":redact_thread_text(source_key),"failure":redact_failure(failure)})
        }
        CommitThreadRunUpdate::Interrupt {
            source_key,
            reconciliation_effect_id,
        } => {
            serde_json::json!({"kind":"interrupt","source_key":redact_thread_text(source_key),"effect_id":reconciliation_effect_id.as_deref().map(redact_thread_text)})
        }
    };
    let canonical = serde_json::json!({
        "thread_id":request.thread_id.to_string(), "command_id":request.command_id.to_string(), "run_id":request.run_id.to_string(),
        "expected_thread_revision":request.expected_thread_revision, "expected_run_revision":request.expected_run_revision,
        "request_id":request.request_id.as_deref().map(redact_thread_text), "effect_id":request.effect_id.as_deref().map(redact_thread_text), "update":update
    });
    let bytes = serde_json::to_vec(&canonical).map_err(invalid_json)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_thread_effect_id(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(StorageError::InvalidData("invalid thread effect id".into()));
    }
    Ok(())
}

fn validate_thread_digest(value: &str) -> Result<(), StorageError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StorageError::InvalidData(
            "invalid thread effect operation digest".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_core::{IdSource, SystemIdSource, Transition};
    use tempfile::TempDir;

    fn ids() -> (RunId, EventId) {
        let source = SystemIdSource::default();
        (
            RunId::from_uuid(source.next_uuid_v7()),
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
        use latte_core::{RunId, SystemIdSource, ThreadId};

        let (source_dir, source_path) = db();
        let source = Storage::open(&source_path).unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        source
            .create_thread_v2(
                thread_id,
                RunId::from_uuid(ids.next_uuid_v7()),
                &thread_binding(),
                source_dir.path().to_str().unwrap(),
                "imported conversation",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        drop(source);
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
            destination.list_threads_v2().unwrap()[0].thread_id,
            thread_id
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

    #[test]
    fn legacy_import_rejects_same_unsupported_and_colliding_databases() {
        use latte_core::ThreadId;

        let (source_dir, source_path) = db();
        let source = Storage::open(&source_path).unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        source
            .create_thread_v2(
                thread_id,
                RunId::from_uuid(ids.next_uuid_v7()),
                &thread_binding(),
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
            .create_thread_v2(
                thread_id,
                RunId::from_uuid(ids.next_uuid_v7()),
                &thread_binding(),
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
        let queued = RunState::queued(run);
        store.create_run(&queued, 1).unwrap();
        let a = store.acquire_lease("a", 2, 100).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
                    status: RunStatus::Interrupted,
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
        let q = RunState::queued(run);
        store.create_run(&q, 1).unwrap();
        let a = store.acquire_lease("a", 2, 5).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
        let q = RunState::queued(run);
        store.create_run(&q, 1).unwrap();
        let lease = store.acquire_lease("owner", 2, 100).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
        let q = RunState::queued(run);
        store.create_run(&q, 1).unwrap();
        let a = store.acquire_lease("a", 2, 10).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
        assert_eq!(store.load_run(run).unwrap().status, RunStatus::Interrupted);
    }
    #[test]
    fn second_open_is_read_only_while_live_then_recovers_orphan_once() {
        let (_dir, path) = db();
        let now = crate::wall_now_ms();
        let first = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let q = RunState::queued(run);
        first.create_run(&q, now).unwrap();
        let lease = first.acquire_lease("live", now, 10_000).unwrap();
        let running = q.transition(0, Transition::Start).unwrap();
        first
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
        assert_eq!(second.load_run(run).unwrap(), running);
        assert_eq!(
            second.effect_status("live-effect").unwrap(),
            EffectStatus::Started
        );
        second.recover_at(lease.expires_at_ms() + 1).unwrap();
        let recovered = second.load_run(run).unwrap();
        assert_eq!(recovered.status, RunStatus::Interrupted);
        assert_eq!(recovered.revision, 2);
        assert_eq!(
            second.effect_status("live-effect").unwrap(),
            EffectStatus::Unknown
        );
        second.recover_at(lease.expires_at_ms() + 2).unwrap();
        assert_eq!(second.load_run(run).unwrap(), recovered);
    }

    #[test]
    fn unknown_reconcile_rejects_cross_run_without_mutation() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let lease = store.acquire_lease("owner", 1, 100).unwrap();
        let (a, ea) = ids();
        let (b, eb) = ids();
        for (run, event) in [(a, ea), (b, eb)] {
            let q = RunState::queued(run);
            store.create_run(&q, 1).unwrap();
            let r = q.transition(0, Transition::Start).unwrap();
            store
                .append_event(
                    &r,
                    0,
                    event,
                    &RuntimeEvent::StateChanged {
                        status: RunStatus::Running,
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
        assert_eq!(store.load_run(a).unwrap().status, RunStatus::Running);
        assert_eq!(store.load_run(b).unwrap().status, RunStatus::Running);
    }

    #[test]
    fn exact_unknown_reconcile_atomically_aborts_own_run() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let lease = store.acquire_lease("owner", 1, 100).unwrap();
        let (run, event) = ids();
        let queued = RunState::queued(run);
        store.create_run(&queued, 1).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.revision, 2);
        assert_eq!(
            store.effect_status("unknown").unwrap(),
            EffectStatus::ObservedFailed
        );
        assert_eq!(store.load_run(run).unwrap(), failed);
    }

    #[test]
    fn lease_loss_recovery_interrupts_only_matching_stored_token() {
        let (_dir, path) = db();
        let store = Storage::open(&path).unwrap();
        let (run, event) = ids();
        let queued = RunState::queued(run);
        store.create_run(&queued, 1).unwrap();
        let lease = store.acquire_lease("lost", 1, 2).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
                },
                2,
                &lease,
            )
            .unwrap();
        store.start_effect("started", run, 2).unwrap();
        assert!(
            matches!(store.interrupt_after_lease_loss(run,&lease,1,4).unwrap(),LeaseLossRecovery::Interrupted(state) if state.status==RunStatus::Interrupted)
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
        let queued = RunState::queued(run);
        store.create_run(&queued, 1).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        assert_eq!(
            store
                .append_event(
                    &running,
                    0,
                    event,
                    &RuntimeEvent::StateChanged {
                        status: RunStatus::Running
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
                    status: RunStatus::Running
                },
                3,
                &lease
            ),
            Err(StorageError::StaleRevision { .. })
        ));
        assert_eq!(store.load_run(run).unwrap(), running);
        drop(store);
        let reopened = Storage::open(&path).unwrap();
        let recovered = reopened.load_run(run).unwrap();
        assert_eq!(recovered.status, RunStatus::Interrupted);
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
        let queued = RunState::queued(run);
        store.create_run(&queued, 1).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        assert!(matches!(
            store.append_event(
                &running,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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
                    status: RunStatus::Running,
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
        let first_thread = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let second_thread = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());

        let first = store.acquire_thread_lease(first_thread, 10, 100).unwrap();
        let second = store.acquire_thread_lease(second_thread, 11, 100).unwrap();
        assert_eq!(first.scope(), format!("thread:{first_thread}"));
        assert_ne!(first.scope, second.scope);
        assert!(second.fencing_token > first.fencing_token);
        assert!(matches!(
            store.acquire_thread_lease(first_thread, 12, 100),
            Err(StorageError::EngineUnavailable)
        ));
        assert_eq!(
            store.renew_lease(&first, 12, 100).unwrap().fencing_token,
            first.fencing_token
        );

        store.release_lease(&first).unwrap();
        let takeover = store.acquire_thread_lease(first_thread, 13, 100).unwrap();
        assert!(takeover.fencing_token > second.fencing_token);
        assert!(matches!(
            store.renew_lease(&first, 14, 100),
            Err(StorageError::LeaseLost)
        ));

        let (_, linked_run, _) = create_linked_fixture(&store, &ids, "linked authority", 15);
        assert!(matches!(
            store.acquire_run_lease(linked_run, "legacy-linked", 16, 100),
            Err(StorageError::LinkedRunRequiresThreadCommit)
        ));
        let missing_run = RunId::from_uuid(ids.next_uuid_v7());
        assert!(matches!(
            store.acquire_run_lease(missing_run, "legacy-missing", 17, 100),
            Err(StorageError::RunNotFound(id)) if id == missing_run
        ));

        store.release_lease(&takeover).unwrap();
        store.release_lease(&second).unwrap();
    }

    #[test]
    fn releasing_a_running_thread_lease_recovers_immediately() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_thread_lease(thread_id, 1, 100).unwrap();
        let started = match store
            .create_started_thread_v2(
                None,
                thread_id,
                run_id,
                &thread_binding(),
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
        assert_eq!(started.lifecycle, ThreadLifecycle::Running);

        let recovered = store
            .release_lease(&lease)
            .unwrap()
            .expect("running release must recover");
        assert_eq!(recovered.snapshot.lifecycle, ThreadLifecycle::Interrupted);
        assert!(recovered.snapshot.active_run_id.is_none());
        assert_eq!(
            store.thread_snapshot_v2(thread_id, None, 100).unwrap(),
            recovered.snapshot
        );
    }

    #[test]
    fn ready_session_switches_binding_durably_under_exact_thread_authority() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (thread_id, run_id, created) = create_linked_fixture(&store, &ids, "switch model", 10);
        let lease = store.acquire_thread_lease(thread_id, 11, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &created,
            run_id,
            CommitThreadRunUpdate::Start {
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
            run_id,
            CommitThreadRunUpdate::Fail {
                source_key: "switch:retryable".into(),
                failure: RunFailure {
                    code: FailureCode::RuntimeFailed,
                    message: "retry with another model".into(),
                    retryability: Retryability::Retryable,
                },
            },
            13,
        )
        .snapshot;
        assert_eq!(ready.lifecycle, ThreadLifecycle::Ready);

        let mut next = thread_binding();
        next.provider_name = "other-provider".into();
        next.model = "other-model".into();
        next.config_fingerprint = "other-config".into();
        let switched = store
            .switch_thread_binding_v2(thread_id, ready.revision, &next, &lease, 14)
            .unwrap();
        assert_eq!(switched.snapshot.binding, next);
        assert_eq!(switched.snapshot.lifecycle, ThreadLifecycle::Ready);
        assert!(matches!(
            switched.thread_event.envelope.event,
            ThreadEvent::BindingChanged {
                ref provider_name,
                ref model
            } if provider_name == "other-provider" && model == "other-model"
        ));
        let card = switched.snapshot.transcript.entries.last().unwrap();
        assert_eq!(card.kind, TranscriptKind::System);
        assert_eq!(card.text, "Model switched to other-provider/other-model");

        let foreign_thread = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let foreign = store.acquire_thread_lease(foreign_thread, 15, 100).unwrap();
        assert!(matches!(
            store.switch_thread_binding_v2(
                thread_id,
                switched.snapshot.revision,
                &thread_binding(),
                &foreign,
                16
            ),
            Err(StorageError::LeaseLost)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn thread_snapshot_projects_failure_code_from_durable_run_state() {
        // The CLI exit-code contract needs to distinguish a permission-denied
        // run from any other terminal failure. failure_code must be projected
        // from the durable RunState.failure.code, not hardcoded to None.
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();

        // A terminal permission-denied run projects PermissionDenied.
        let (denied_thread, denied_run, created) =
            create_linked_fixture(&store, &ids, "denied run", 10);
        let lease = store.acquire_thread_lease(denied_thread, 11, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &created,
            denied_run,
            CommitThreadRunUpdate::Start {
                source_key: "denied:start".into(),
            },
            12,
        )
        .snapshot;
        // A run still executing has no failure code yet.
        assert_eq!(
            running
                .runs
                .iter()
                .find(|run| run.run_id == denied_run)
                .unwrap()
                .failure_code,
            None,
            "an active run has no failure code"
        );
        let denied = commit_linked(
            &store,
            &ids,
            &lease,
            &running,
            denied_run,
            CommitThreadRunUpdate::Fail {
                source_key: "denied:fail".into(),
                failure: RunFailure {
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
                .runs
                .iter()
                .find(|run| run.run_id == denied_run)
                .unwrap()
                .failure_code,
            Some(FailureCode::PermissionDenied),
            "a permission-denied run projects PermissionDenied"
        );
        // The projection survives a fresh read (not just the in-memory commit).
        let reread = store.thread_snapshot_v2(denied_thread, None, 100).unwrap();
        assert_eq!(
            reread
                .runs
                .iter()
                .find(|run| run.run_id == denied_run)
                .unwrap()
                .failure_code,
            Some(FailureCode::PermissionDenied),
        );

        // An ordinary terminal failure projects RuntimeFailed, so exit-code
        // logic can tell it apart from the permission-denied case.
        let (failed_thread, failed_run, created) =
            create_linked_fixture(&store, &ids, "failed run", 20);
        let lease = store.acquire_thread_lease(failed_thread, 21, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &lease,
            &created,
            failed_run,
            CommitThreadRunUpdate::Start {
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
            failed_run,
            CommitThreadRunUpdate::Fail {
                source_key: "failed:fail".into(),
                failure: RunFailure {
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
                .runs
                .iter()
                .find(|run| run.run_id == failed_run)
                .unwrap()
                .failure_code,
            Some(FailureCode::RuntimeFailed),
            "an ordinary terminal failure projects RuntimeFailed, not PermissionDenied"
        );
    }

    #[test]
    fn runtime_and_thread_lease_scopes_are_bidirectional_authority_boundaries() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let legacy_run = RunId::from_uuid(ids.next_uuid_v7());
        store.create_run(&RunState::queued(legacy_run), 1).unwrap();
        let runtime_lease = store
            .acquire_run_lease(legacy_run, "legacy", 2, 1_000)
            .unwrap();

        let (thread_id, linked_run, queued_thread) =
            create_linked_fixture(&store, &ids, "scoped", 3);
        let thread_lease = store.acquire_thread_lease(thread_id, 4, 1_000).unwrap();
        assert!(thread_lease.fencing_token > runtime_lease.fencing_token);

        assert!(matches!(
            store.apply_transition(legacy_run, 0, Transition::Start, 5, &thread_lease),
            Err(StorageError::LeaseLost)
        ));
        assert_eq!(
            store.load_run(legacy_run).unwrap().status,
            RunStatus::Queued
        );

        assert!(matches!(
            store.commit_thread_run_update(
                &ThreadCommitRequest {
                    thread_id,
                    run_id: linked_run,
                    expected_thread_revision: queued_thread.revision,
                    expected_run_revision: 0,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitThreadRunUpdate::Start {
                        source_key: "wrong-runtime-scope".into(),
                    },
                },
                &runtime_lease,
                5,
            ),
            Err(StorageError::LeaseLost)
        ));
        assert_eq!(
            store.load_run(linked_run).unwrap().status,
            RunStatus::Queued
        );

        store
            .apply_transition(legacy_run, 0, Transition::Start, 6, &runtime_lease)
            .unwrap();
        commit_linked(
            &store,
            &ids,
            &thread_lease,
            &queued_thread,
            linked_run,
            CommitThreadRunUpdate::Start {
                source_key: "correct-thread-scope".into(),
            },
            6,
        );
    }

    #[test]
    fn atomic_thread_acceptance_rolls_back_every_record_when_start_write_fails() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_thread_lease(thread_id, 10, 1_000).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER inject_atomic_start_failure \
                 BEFORE INSERT ON thread_events_v2 \
                 BEGIN SELECT RAISE(ABORT, 'injected atomic start failure'); END;",
            )
            .unwrap();

        let error = store
            .create_started_thread_v2(
                None,
                thread_id,
                run_id,
                &thread_binding(),
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
            store.load_run(run_id),
            Err(StorageError::RunNotFound(id)) if id == run_id
        ));
        assert!(matches!(
            store.thread_snapshot_v2(thread_id, None, 10),
            Err(StorageError::ThreadNotFound(id)) if id == thread_id
        ));
        let orphan_rows: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM conversation_outbox WHERE thread_id=?1",
                [thread_id.to_string()],
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
        let queued = RunState::queued(run);
        store.create_run(&queued, 1).unwrap();
        let cancelling = queued.transition(0, Transition::Cancel).unwrap();
        store
            .append_event(
                &cancelling,
                0,
                event,
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Cancelling,
                },
                2,
                &lease,
            )
            .unwrap();
        store.start_effect("e", run, 2).unwrap();
        assert_eq!(store.effect_status("e").unwrap(), EffectStatus::Started);
        drop(store);
        let store = Storage::open(&path).unwrap();
        assert_eq!(store.load_run(run).unwrap().status, RunStatus::Interrupted);
        assert_eq!(store.effect_status("e").unwrap(), EffectStatus::Unknown);
    }

    #[test]
    fn effect_phases_bind_approval_and_preserve_unknown_without_retry() {
        let (_dir, path) = db();
        let (run, _) = ids();
        {
            let store = Storage::open(&path).unwrap();
            let queued = RunState::queued(run);
            store.create_run(&queued, 1).unwrap();
            let lease = store.acquire_lease("owner", 1, 2).unwrap();
            let running = queued.transition(0, Transition::Start).unwrap();
            store
                .append_event(
                    &running,
                    0,
                    ids().1,
                    &RuntimeEvent::StateChanged {
                        status: RunStatus::Running,
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
        store.create_run(&RunState::queued(run), 1).unwrap();
        let lease = store.acquire_lease("owner", 2, 100).unwrap();
        for (id, success, status) in [
            ("ok", true, EffectStatus::ObservedSuccess),
            ("fail", false, EffectStatus::ObservedFailed),
        ] {
            store.declare_effect(id, run, 1, "{}", 2).unwrap();
            store.prepare_effect(id, "d", "{}", 3).unwrap();
            store.start_prepared_effect(id, "d", 4).unwrap();
            let authority = EffectAuthority {
                run_id: run,
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
        store.create_run(&RunState::queued(run), 1).unwrap();
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
            .execute_batch(
                "DROP TABLE thread_effect_canonical_v2; \
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
            )
            .unwrap();
        drop(connection);

        drop(Storage::open(&path).unwrap());
        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let descriptor_table: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='thread_effect_canonical_v2')",
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
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('threads_v2') WHERE name='focus')",
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
        let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let store = Storage::open(&path).unwrap();
        store
            .create_thread_v2(
                thread_id,
                run_id,
                &thread_binding(),
                "/old/workspace",
                "legacy title",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "UPDATE threads_v2 SET title='',workspace_root=''; \
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
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            Storage::open(&path),
            Err(StorageError::InvalidData(message))
                if message.contains("legacy Sessions have no workspace identity")
        ));
        let migrated = Storage::open_in_workspace(&path, "/adopted/workspace").unwrap();
        let metadata = migrated.thread_session_v2(thread_id).unwrap().unwrap();
        assert_eq!(metadata.workspace_root, "/adopted/workspace");
        assert_eq!(metadata.title, "legacy title");
        assert_eq!(
            migrated
                .list_thread_sessions_v2_for_workspace("/adopted/workspace", 10)
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
    fn v2_thread_child_is_fenced_idempotent_and_parent_is_immutable() {
        use latte_core::{ThreadCommandId, ThreadId, ThreadProviderBindingV2};
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread = ThreadId::from_uuid(ids.next_uuid_v7());
        let first = RunId::from_uuid(ids.next_uuid_v7());
        let binding = ThreadProviderBindingV2 {
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
            .create_thread_v2(
                thread,
                first,
                &binding,
                "/workspace",
                "hello sk-this-is-a-secret-123456789",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        assert_eq!(initial.lifecycle, ThreadLifecycle::Running);
        assert_eq!(initial.sequence, 1);
        assert_eq!(initial.transcript.entries[0].sequence, 1);
        assert!(!initial.transcript.entries[0].text.contains("sk-this"));
        assert!(store.is_thread_linked_run(first).unwrap());
        let lease = store.acquire_thread_lease(thread, 2, 100).unwrap();
        let start = ThreadCommitRequest {
            thread_id: thread,
            run_id: first,
            expected_thread_revision: 0,
            expected_run_revision: 0,
            command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: CommitThreadRunUpdate::Start {
                source_key: "start".into(),
            },
        };
        let started = store.commit_thread_run_update(&start, &lease, 3).unwrap();
        assert_eq!(started.snapshot.runs[0].run_revision, 1);
        let replay = store.commit_thread_run_update(&start, &lease, 4).unwrap();
        assert_eq!(replay, started);
        let changed = ThreadCommitRequest {
            update: CommitThreadRunUpdate::AppendTranscript {
                source_key: "other".into(),
                kind: TranscriptKind::Assistant,
                text: "different".into(),
                payload: None,
            },
            ..start.clone()
        };
        assert!(matches!(
            store.commit_thread_run_update(&changed, &lease, 5),
            Err(StorageError::ThreadCommandReplayMismatch)
        ));
        let completed = ThreadCommitRequest {
            thread_id: thread,
            run_id: first,
            expected_thread_revision: 1,
            expected_run_revision: 1,
            command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: CommitThreadRunUpdate::Complete {
                source_key: "complete".into(),
                handoff: Handoff {
                    summary: "done".into(),
                    files_changed: vec![],
                    evidence: vec![],
                },
            },
        };
        let completed = store
            .commit_thread_run_update(&completed, &lease, 6)
            .unwrap();
        assert_eq!(completed.snapshot.lifecycle, ThreadLifecycle::Ready);
        let parent = store.load_run(first).unwrap();
        assert_eq!(parent.status, RunStatus::Completed);
        let child = RunId::from_uuid(ids.next_uuid_v7());
        let followup = store
            .create_thread_follow_up_v2(
                thread,
                child,
                completed.snapshot.revision,
                "next",
                &std::collections::BTreeMap::new(),
                7,
            )
            .unwrap();
        assert_eq!(followup.runs.len(), 2);
        assert_eq!(followup.runs[1].parent_run_id, Some(first));
        assert_eq!(store.load_run(first).unwrap(), parent);
    }

    #[test]
    fn initial_thread_cursor_allows_a_queued_run_to_fail_with_a_durable_card() {
        use latte_core::{ThreadCommandId, ThreadId, ThreadProviderBindingV2};
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let binding = ThreadProviderBindingV2 {
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
            .create_thread_v2(
                thread_id,
                run_id,
                &binding,
                "/workspace",
                "durable prompt",
                &std::collections::BTreeMap::new(),
                1,
            )
            .unwrap();
        assert_eq!(initial.sequence, 1);
        assert_eq!(initial.transcript.entries[0].sequence, 1);

        let lease = store.acquire_thread_lease(thread_id, 2, 100).unwrap();
        let failed = store
            .commit_thread_run_update(
                &ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: 0,
                    expected_run_revision: 0,
                    command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: CommitThreadRunUpdate::Fail {
                        source_key: "provider-configuration-failure".into(),
                        failure: RunFailure {
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

        assert_eq!(failed.snapshot.lifecycle, ThreadLifecycle::Failed);
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
        use latte_core::{ThreadId, ThreadProviderBindingV2};
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let binding = ThreadProviderBindingV2 {
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
            .create_thread_v2(
                thread_id,
                run_id,
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
                run_id: Some(run_id),
                kind: TranscriptKind::Assistant,
                text: format!("card-{sequence}"),
                payload: None,
                source_key: format!("fixture:{sequence}"),
                created_at_ms: sequence,
            };
            conn.execute(
                "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) VALUES(?1,?2,?3,?4,'assistant',?5,?6,?7)",
                params![
                    thread_id.to_string(),
                    to_i64(sequence).unwrap(),
                    entry.entry_id.to_string(),
                    run_id.to_string(),
                    entry.source_key,
                    serde_json::to_string(&entry).unwrap(),
                    to_i64(sequence).unwrap(),
                ],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE threads_v2 SET last_seq=502,updated_at_ms=502 WHERE thread_id=?1",
            [thread_id.to_string()],
        )
        .unwrap();
        drop(conn);

        let sessions = store.list_threads_v2().unwrap();
        assert_eq!(sessions.len(), 1);
        let transcript = &sessions[0].transcript;
        assert_eq!(transcript.entries.len(), THREAD_PROJECTION_TRANSCRIPT_LIMIT);
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
        use latte_core::{RunId, SystemIdSource, ThreadId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        store
            .create_thread_v2(
                thread_id,
                run_id,
                &thread_binding(),
                "/workspace/catalog",
                "First session title\nwith more context",
                &std::collections::BTreeMap::new(),
                42,
            )
            .unwrap();
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE conversation_outbox SET entry_json='{' WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
        }

        let catalog = store
            .list_thread_sessions_v2_for_workspace("/workspace/catalog", 1)
            .unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].thread_id, thread_id);
        assert_eq!(catalog[0].title, "First session title");
        assert_eq!(catalog[0].workspace_root, "/workspace/catalog");
        assert_eq!(catalog[0].model, "model");
        assert_eq!(catalog[0].created_at_ms, 42);
        assert_eq!(catalog[0].updated_at_ms, 42);
        assert_eq!(
            store.thread_session_v2(thread_id).unwrap().unwrap(),
            catalog[0]
        );
        assert_eq!(
            store
                .list_thread_sessions_v2_for_workspace("/workspace/catalog", 10)
                .unwrap(),
            catalog
        );
        assert!(
            store
                .search_thread_sessions("/workspace/catalog", "title", 0,)
                .unwrap()
                .is_empty()
        );
        assert!(store.rename_thread_session(thread_id, "  ").is_err());
        let missing = ThreadId::from_uuid(ids.next_uuid_v7());
        assert!(store.rename_thread_session(missing, "missing").is_err());
    }

    #[test]
    fn workspace_scoped_thread_projection_never_matches_an_identical_foreign_prompt() {
        use latte_core::{RunId, SystemIdSource, ThreadId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let local_thread = ThreadId::from_uuid(ids.next_uuid_v7());
        let foreign_thread = ThreadId::from_uuid(ids.next_uuid_v7());
        for (thread_id, workspace_root, now_ms) in [
            (local_thread, "/workspace/local", 1),
            (foreign_thread, "/workspace/foreign", 2),
        ] {
            store
                .create_thread_v2(
                    thread_id,
                    RunId::from_uuid(ids.next_uuid_v7()),
                    &thread_binding(),
                    workspace_root,
                    "identical prompt",
                    &std::collections::BTreeMap::new(),
                    now_ms,
                )
                .unwrap();
        }

        let local = store
            .list_threads_v2_for_workspace("/workspace/local")
            .unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].thread_id, local_thread);
        assert_eq!(local[0].transcript.entries[0].text, "identical prompt");
        assert_eq!(
            store
                .list_thread_sessions_v2_for_workspace("/workspace/local", 10)
                .unwrap()
                .into_iter()
                .map(|session| session.thread_id)
                .collect::<Vec<_>>(),
            vec![local_thread]
        );
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
            (RunStatus::Queued, "queued", ThreadRunStatus::Queued),
            (RunStatus::Running, "running", ThreadRunStatus::Running),
            (
                RunStatus::WaitingPermission,
                "waiting_permission",
                ThreadRunStatus::WaitingPermission,
            ),
            (
                RunStatus::WaitingInput,
                "waiting_input",
                ThreadRunStatus::WaitingInput,
            ),
            (
                RunStatus::Cancelling,
                "cancelling",
                ThreadRunStatus::Cancelling,
            ),
            (
                RunStatus::Interrupted,
                "interrupted",
                ThreadRunStatus::Interrupted,
            ),
            (RunStatus::Failed, "failed", ThreadRunStatus::Failed),
            (
                RunStatus::Completed,
                "completed",
                ThreadRunStatus::Completed,
            ),
        ] {
            assert_eq!(status_name(status), stored);
            assert_eq!(thread_run_status(status), projected);
        }

        for (stored, lifecycle) in [
            ("ready", ThreadLifecycle::Ready),
            ("running", ThreadLifecycle::Running),
            ("waiting_permission", ThreadLifecycle::WaitingPermission),
            ("waiting_input", ThreadLifecycle::WaitingInput),
            ("interrupted", ThreadLifecycle::Interrupted),
            ("failed", ThreadLifecycle::Failed),
            (
                "reconciliation_required",
                ThreadLifecycle::ReconciliationRequired,
            ),
        ] {
            assert_eq!(parse_lifecycle(stored).unwrap(), lifecycle);
        }
        assert!(matches!(
            parse_lifecycle("completed"),
            Err(StorageError::InvalidData(message)) if message.contains("invalid thread lifecycle")
        ));

        let id = SystemIdSource::default().next_uuid_v7();
        assert_eq!(
            parse_thread_id(&id.to_string()).unwrap().to_string(),
            id.to_string()
        );
        assert!(matches!(
            parse_thread_id("not-a-uuid"),
            Err(StorageError::InvalidData(message)) if message.contains("invalid stored thread id")
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
    fn thread_command_digest_covers_every_variant_and_excludes_private_descriptor_secrets() {
        use latte_core::{PendingInput, PendingPermission, ThreadCommandId, ThreadId};
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let command_id = ThreadCommandId::from_uuid(ids.next_uuid_v7());
        let source_key = "source".to_owned();
        let updates = vec![
            CommitThreadRunUpdate::Start {
                source_key: source_key.clone(),
            },
            CommitThreadRunUpdate::AppendTranscript {
                source_key: source_key.clone(),
                kind: TranscriptKind::Assistant,
                text: "assistant text".into(),
                payload: Some(serde_json::json!({"value":"payload"})),
            },
            CommitThreadRunUpdate::PrepareEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                descriptor_json: r#"{"name":"write_file"}"#.into(),
                canonical_descriptor_json: r#"{"api_key":"sk-private-secret-123456789"}"#.into(),
                policy: ThreadEffectPolicy::Ask,
                description: "prepare".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
            CommitThreadRunUpdate::StartEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                checkpoint_json: r#"{"phase":"started"}"#.into(),
            },
            CommitThreadRunUpdate::ObserveEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                success: true,
                result: "observed".into(),
                payload: Some(serde_json::json!({"result":"ok"})),
                checkpoint_json: r#"{"phase":"observed"}"#.into(),
            },
            CommitThreadRunUpdate::UnknownEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                operation_digest: "a".repeat(64),
                checkpoint_json: r#"{"phase":"unknown"}"#.into(),
            },
            CommitThreadRunUpdate::ReconcileUnknownEffect {
                source_key: source_key.clone(),
                effect_id: "effect".into(),
                checkpoint_json: r#"{"phase":"reconciled"}"#.into(),
            },
            CommitThreadRunUpdate::RequestPermission {
                source_key: source_key.clone(),
                request: PendingPermission {
                    request_id: "permission".into(),
                    operation_digest: "b".repeat(64),
                    description: "allow write".into(),
                },
            },
            CommitThreadRunUpdate::ResolvePermission {
                source_key: source_key.clone(),
                request_id: "permission".into(),
                allow: true,
                rebound_operation_digest: None,
            },
            CommitThreadRunUpdate::RequestInput {
                source_key: source_key.clone(),
                request: PendingInput {
                    request_id: "input".into(),
                    prompt: "value?".into(),
                },
            },
            CommitThreadRunUpdate::ProvideInput {
                source_key: source_key.clone(),
                request_id: "input".into(),
                value: "answer".into(),
            },
            CommitThreadRunUpdate::Complete {
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
            CommitThreadRunUpdate::CompleteVerified {
                source_key: source_key.clone(),
                summary: "verified".into(),
                verification_effect_id: "verification".into(),
                verified_manifest_digest: "c".repeat(64),
                files_changed: vec!["a.txt".into()],
            },
            CommitThreadRunUpdate::Fail {
                source_key: source_key.clone(),
                failure: RunFailure {
                    code: FailureCode::RuntimeFailed,
                    message: "failed".into(),
                    retryability: Retryability::Terminal,
                },
            },
            CommitThreadRunUpdate::Interrupt {
                source_key: source_key.clone(),
                reconciliation_effect_id: Some("effect".into()),
            },
        ];

        let mut digests = std::collections::BTreeSet::new();
        for update in &updates {
            assert_eq!(update.source_key(), source_key);
            let request = ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: 2,
                expected_run_revision: 3,
                command_id,
                request_id: Some("request".into()),
                effect_id: Some("effect".into()),
                update: update.clone(),
            };
            let digest = thread_command_digest(&request).unwrap();
            assert_eq!(digest.len(), 64);
            assert!(
                digests.insert(digest),
                "variant digest collision: {update:?}"
            );
        }

        let CommitThreadRunUpdate::PrepareEffect { .. } = &updates[2] else {
            unreachable!()
        };
        let mut changed_private = updates[2].clone();
        let CommitThreadRunUpdate::PrepareEffect {
            canonical_descriptor_json,
            ..
        } = &mut changed_private
        else {
            unreachable!()
        };
        *canonical_descriptor_json = r#"{"api_key":"sk-a-different-private-secret"}"#.into();
        let request = |update| ThreadCommitRequest {
            thread_id,
            run_id,
            expected_thread_revision: 2,
            expected_run_revision: 3,
            command_id,
            request_id: Some("request".into()),
            effect_id: Some("effect".into()),
            update,
        };
        assert_eq!(
            thread_command_digest(&request(updates[2].clone())).unwrap(),
            thread_command_digest(&request(changed_private)).unwrap(),
            "engine-private canonical content must not enter the replay digest"
        );
    }

    #[test]
    fn thread_identifiers_sources_and_redaction_helpers_reject_unsafe_durable_values() {
        use latte_core::{PendingInput, PendingPermission};
        for invalid in ["", "line\nbreak"] {
            assert!(validate_thread_source(invalid).is_err());
            assert!(validate_thread_effect_id(invalid).is_err());
        }
        assert!(validate_thread_source(&"s".repeat(257)).is_err());
        assert!(validate_thread_effect_id(&"e".repeat(513)).is_err());
        validate_thread_source(&"s".repeat(256)).unwrap();
        validate_thread_effect_id(&"e".repeat(512)).unwrap();

        for invalid in ["a".repeat(63), "g".repeat(64), "a".repeat(65)] {
            assert!(validate_thread_digest(&invalid).is_err());
        }
        validate_thread_digest(&"aB09".repeat(16)).unwrap();

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
        let failure = redact_failure(&RunFailure {
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

    fn thread_binding() -> ThreadProviderBindingV2 {
        ThreadProviderBindingV2 {
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
    ) -> (latte_core::ThreadId, RunId, ThreadSnapshot) {
        let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let snapshot = store
            .create_thread_v2(
                thread_id,
                run_id,
                &thread_binding(),
                "/workspace",
                prompt,
                &std::collections::BTreeMap::new(),
                now_ms,
            )
            .unwrap();
        (thread_id, run_id, snapshot)
    }

    fn commit_linked(
        store: &Storage,
        ids: &SystemIdSource,
        lease: &Lease,
        snapshot: &ThreadSnapshot,
        run_id: RunId,
        update: CommitThreadRunUpdate,
        now_ms: u64,
    ) -> ThreadCommitResponse {
        let run_revision = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .unwrap()
            .run_revision;
        store
            .commit_thread_run_update(
                &ThreadCommitRequest {
                    thread_id: snapshot.thread_id,
                    run_id,
                    expected_thread_revision: snapshot.revision,
                    expected_run_revision: run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
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
    fn clean_wait_reopen_requires_atomic_permission_rebind_to_the_new_epoch() {
        let (dir, path) = db();
        let ids = SystemIdSource::default();
        let store = Storage::open(&path).unwrap();
        let (thread_id, run_id, queued) =
            create_linked_fixture(&store, &ids, "rebind permission", 10);
        let first = store.acquire_thread_lease(thread_id, 11, 100).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &first,
            &queued,
            run_id,
            CommitThreadRunUpdate::Start {
                source_key: "rebind:start".into(),
            },
            12,
        )
        .snapshot;
        let descriptor = crate::ThreadEffectDescriptor {
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
            run_id,
            CommitThreadRunUpdate::PrepareEffect {
                source_key: "rebind:prepare".into(),
                effect_id: descriptor.effect_id.clone(),
                operation_digest: old_digest.clone(),
                descriptor_json: serde_json::to_string(&descriptor).unwrap(),
                canonical_descriptor_json: serde_json::to_string(&descriptor).unwrap(),
                policy: ThreadEffectPolicy::Ask,
                description: "write rebound.txt".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
            13,
        )
        .snapshot;
        assert_eq!(waiting.lifecycle, ThreadLifecycle::WaitingPermission);

        store.release_lease(&first).unwrap();
        drop(store);
        let reopened = Storage::open(&path).unwrap();
        let preserved = reopened.thread_snapshot_v2(thread_id, None, 100).unwrap();
        assert_eq!(preserved.lifecycle, ThreadLifecycle::WaitingPermission);
        assert_eq!(preserved.active_run_id, Some(run_id));
        let second = reopened.acquire_thread_lease(thread_id, 14, 100).unwrap();
        assert!(second.fencing_token > first.fencing_token);

        let mut mismatched_descriptor = descriptor.clone();
        mismatched_descriptor.effect_id = "different-effect".into();
        reopened
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE thread_effect_canonical_v2 SET descriptor_json=?1 WHERE effect_id=?2",
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
            .resolve_thread_effect_permission(
                thread_id,
                run_id,
                preserved.revision,
                preserved.runs[0].run_revision,
                descriptor.effect_id.clone(),
                "rebind:bad-identifier".into(),
                true,
                latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                &second,
                15,
            )
            .unwrap_err();
        assert!(
            identifier_error
                .to_string()
                .contains("canonical thread effect identifier mismatch")
        );
        reopened
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE thread_effect_canonical_v2 SET descriptor_json=?1 WHERE effect_id=?2",
                params![
                    serde_json::to_string(&descriptor).unwrap(),
                    descriptor.effect_id
                ],
            )
            .unwrap();
        let overflow = engine
            .resolve_thread_effect_permission(
                thread_id,
                run_id,
                preserved.revision,
                u64::MAX,
                descriptor.effect_id.clone(),
                "rebind:overflow".into(),
                true,
                latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                &second,
                15,
            )
            .unwrap_err();
        assert!(overflow.to_string().contains("run revision overflow"));
        drop(engine);

        let resolve = |digest: Option<String>| {
            reopened.commit_thread_run_update(
                &ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: preserved.revision,
                    expected_run_revision: preserved.runs[0].run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: Some(descriptor.effect_id.clone()),
                    effect_id: Some(descriptor.effect_id.clone()),
                    update: CommitThreadRunUpdate::ResolvePermission {
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
                .thread_snapshot_v2(thread_id, None, 100)
                .unwrap()
                .lifecycle,
            ThreadLifecycle::WaitingPermission
        );

        let rebound_digest = "b".repeat(64);
        let allowed = resolve(Some(rebound_digest.clone())).unwrap().snapshot;
        assert_eq!(allowed.lifecycle, ThreadLifecycle::Running);
        assert_eq!(
            reopened
                .thread_effect_digest(&descriptor.effect_id)
                .unwrap(),
            rebound_digest
        );
        let started = commit_linked(
            &reopened,
            &ids,
            &second,
            &allowed,
            run_id,
            CommitThreadRunUpdate::StartEffect {
                source_key: "rebind:started".into(),
                effect_id: descriptor.effect_id.clone(),
                operation_digest: rebound_digest,
                checkpoint_json: r#"{"phase":"started"}"#.into(),
            },
            16,
        );
        assert_eq!(started.snapshot.lifecycle, ThreadLifecycle::Running);
        assert_eq!(
            reopened.effect_status(&descriptor.effect_id).unwrap(),
            EffectStatus::Started
        );

        let (generic_thread, generic_run, generic_queued) =
            create_linked_fixture(&reopened, &ids, "generic permission", 20);
        let generic_lease = reopened
            .acquire_thread_lease(generic_thread, 20, 100)
            .unwrap();
        let generic_running = commit_linked(
            &reopened,
            &ids,
            &generic_lease,
            &generic_queued,
            generic_run,
            CommitThreadRunUpdate::Start {
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
            generic_run,
            CommitThreadRunUpdate::RequestPermission {
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
            .commit_thread_run_update(
                &ThreadCommitRequest {
                    thread_id: generic_thread,
                    run_id: generic_run,
                    expected_thread_revision: generic_waiting.revision,
                    expected_run_revision: generic_waiting.runs[0].run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: Some("generic-permission".into()),
                    effect_id: Some("generic-permission".into()),
                    update: CommitThreadRunUpdate::ResolvePermission {
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
    fn linked_thread_waits_replays_and_terminal_resolutions_are_atomic() {
        use latte_core::{PendingInput, PendingPermission, ThreadCommandId};

        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (thread_id, run_id, mut snapshot) = create_linked_fixture(&store, &ids, "initial", 11);
        let lease = store.acquire_thread_lease(thread_id, 10, 10_000).unwrap();

        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::Start {
                source_key: "start".into(),
            },
            12,
        )
        .snapshot;
        assert_eq!(snapshot.runs[0].status, ThreadRunStatus::Running);

        let append = ThreadCommitRequest {
            thread_id,
            run_id,
            expected_thread_revision: snapshot.revision,
            expected_run_revision: snapshot.runs[0].run_revision,
            command_id: ThreadCommandId::from_uuid(ids.next_uuid_v7()),
            request_id: None,
            effect_id: None,
            update: CommitThreadRunUpdate::AppendTranscript {
                source_key: "assistant-card".into(),
                kind: TranscriptKind::Assistant,
                text: "safe card".into(),
                payload: Some(serde_json::json!({"status":"ok"})),
            },
        };
        let appended = store.commit_thread_run_update(&append, &lease, 13).unwrap();
        // The source ledger is the second durable idempotency key. Simulate a
        // lost command-index row and prove that the source record still
        // returns the exact committed projection without applying twice.
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM thread_command_dedup_v2 WHERE command_id=?1",
                [append.command_id.to_string()],
            )
            .unwrap();
        let source_replay = append.clone();
        assert_eq!(
            store
                .commit_thread_run_update(&source_replay, &lease, 14)
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
                "DELETE FROM thread_command_dedup_v2 WHERE command_id=?1",
                [mismatched_source.command_id.to_string()],
            )
            .unwrap();
        let CommitThreadRunUpdate::AppendTranscript { text, .. } = &mut mismatched_source.update
        else {
            unreachable!()
        };
        *text = "different card".into();
        assert!(matches!(
            store.commit_thread_run_update(&mismatched_source, &lease, 15),
            Err(StorageError::ThreadCommandReplayMismatch)
        ));
        snapshot = appended.snapshot;

        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::RequestInput {
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
            Some(ThreadPendingRequest::Input { .. })
        ));
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::ProvideInput {
                source_key: "provide-input".into(),
                request_id: "input-1".into(),
                value: "answer".into(),
            },
            17,
        )
        .snapshot;
        assert_eq!(snapshot.lifecycle, ThreadLifecycle::Running);

        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::RequestPermission {
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
            Some(ThreadPendingRequest::Permission { .. })
        ));
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::ResolvePermission {
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
            run_id,
            CommitThreadRunUpdate::Complete {
                source_key: "complete".into(),
                handoff: Handoff {
                    summary: "done".into(),
                    files_changed: vec!["a.txt".into()],
                    evidence: vec![],
                },
            },
            20,
        );
        assert_eq!(completed.snapshot.lifecycle, ThreadLifecycle::Ready);
        assert_eq!(completed.snapshot.active_run_id, None);

        let (denied_thread, denied_run, mut denied) =
            create_linked_fixture(&store, &ids, "deny", 21);
        let denied_lease = store
            .acquire_thread_lease(denied_thread, 21, 10_000)
            .unwrap();
        denied = commit_linked(
            &store,
            &ids,
            &denied_lease,
            &denied,
            denied_run,
            CommitThreadRunUpdate::Start {
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
            denied_run,
            CommitThreadRunUpdate::RequestPermission {
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
            denied_run,
            CommitThreadRunUpdate::ResolvePermission {
                source_key: "deny:resolve".into(),
                request_id: "permission-denied".into(),
                allow: false,
                rebound_operation_digest: None,
            },
            24,
        );
        assert_eq!(denied.snapshot.lifecycle, ThreadLifecycle::Ready);
        assert!(denied.snapshot.active_run_id.is_none());
        assert!(denied.snapshot.pending.is_none());
        assert_eq!(
            store.load_run(denied_run).unwrap().status,
            RunStatus::Failed
        );
        let retry_run = RunId::from_uuid(ids.next_uuid_v7());
        let retry = store
            .create_thread_follow_up_v2(
                denied_thread,
                retry_run,
                denied.snapshot.revision,
                "continue after denial",
                &std::collections::BTreeMap::new(),
                25,
            )
            .unwrap();
        assert_eq!(retry.lifecycle, ThreadLifecycle::Running);
        assert_eq!(retry.active_run_id, Some(retry_run));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn linked_effect_started_interrupt_requires_exact_reconciliation() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let (thread_id, run_id, mut snapshot) = create_linked_fixture(&store, &ids, "effect", 101);
        let lease = store.acquire_thread_lease(thread_id, 100, 10_000).unwrap();
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::Start {
                source_key: "effect:start-run".into(),
            },
            102,
        )
        .snapshot;

        let effect_id = "effect-1";
        let digest = "c".repeat(64);
        let canonical_descriptor = crate::ThreadEffectDescriptor {
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
            run_id,
            CommitThreadRunUpdate::PrepareEffect {
                source_key: "effect:prepare".into(),
                effect_id: effect_id.into(),
                operation_digest: digest.clone(),
                descriptor_json: r#"{"name":"read_file","input":{"path":"a.txt"}}"#.into(),
                canonical_descriptor_json: canonical.clone(),
                policy: ThreadEffectPolicy::Allow,
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
                .thread_effect_canonical_descriptor(effect_id, run_id)
                .unwrap(),
            canonical_descriptor
        );
        snapshot = commit_linked(
            &store,
            &ids,
            &lease,
            &snapshot,
            run_id,
            CommitThreadRunUpdate::StartEffect {
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
            run_id,
            CommitThreadRunUpdate::Interrupt {
                source_key: "effect:interrupt".into(),
                reconciliation_effect_id: None,
            },
            105,
        );
        assert_eq!(
            interrupted.snapshot.lifecycle,
            ThreadLifecycle::ReconciliationRequired
        );
        assert_eq!(
            store.effect_status(effect_id).unwrap(),
            EffectStatus::Unknown
        );
        assert_eq!(
            store.unknown_effects_for_run(run_id).unwrap(),
            vec![effect_id.to_owned()]
        );
        let reconciled = commit_linked(
            &store,
            &ids,
            &lease,
            &interrupted.snapshot,
            run_id,
            CommitThreadRunUpdate::ReconcileUnknownEffect {
                source_key: "effect:reconcile".into(),
                effect_id: effect_id.into(),
                checkpoint_json: r#"{"phase":"reconciled"}"#.into(),
            },
            106,
        );
        assert_eq!(reconciled.snapshot.lifecycle, ThreadLifecycle::Failed);
        assert_eq!(
            store.effect_status(effect_id).unwrap(),
            EffectStatus::ObservedFailed
        );
        assert!(store.unknown_effects_for_run(run_id).unwrap().is_empty());

        for (suffix, success, expected) in [
            ("success", true, EffectStatus::ObservedSuccess),
            ("failure", false, EffectStatus::ObservedFailed),
        ] {
            let (observed_thread, observed_run, mut observed) =
                create_linked_fixture(&store, &ids, suffix, 110);
            let observed_lease = store
                .acquire_thread_lease(observed_thread, 110, 10_000)
                .unwrap();
            observed = commit_linked(
                &store,
                &ids,
                &observed_lease,
                &observed,
                observed_run,
                CommitThreadRunUpdate::Start {
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
            let observed_canonical = serde_json::to_string(&crate::ThreadEffectDescriptor {
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
                observed_run,
                CommitThreadRunUpdate::PrepareEffect {
                    source_key: format!("{suffix}:prepare"),
                    effect_id: observed_effect.clone(),
                    operation_digest: observed_digest.clone(),
                    descriptor_json: r#"{"name":"read_file"}"#.into(),
                    canonical_descriptor_json: observed_canonical,
                    policy: ThreadEffectPolicy::Allow,
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
                observed_run,
                CommitThreadRunUpdate::StartEffect {
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
                observed_run,
                CommitThreadRunUpdate::ObserveEffect {
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
            assert_eq!(observed.snapshot.lifecycle, ThreadLifecycle::Running);
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
        let empty_thread = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let empty_run = RunId::from_uuid(ids.next_uuid_v7());
        assert!(
            store
                .create_thread_v2(
                    empty_thread,
                    empty_run,
                    &thread_binding(),
                    "/workspace",
                    " \n ",
                    &baseline,
                    1,
                )
                .unwrap_err()
                .to_string()
                .contains("prompt must not be empty")
        );

        let (thread_id, run_id, queued) = create_linked_fixture(&store, &ids, "initial", 11);
        let lease = store.acquire_thread_lease(thread_id, 10, 10_000).unwrap();
        let follow_up = RunId::from_uuid(ids.next_uuid_v7());
        assert!(
            store
                .create_thread_follow_up_v2(
                    thread_id,
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
            store.create_thread_follow_up_v2(
                thread_id,
                follow_up,
                queued.revision + 1,
                "next",
                &baseline,
                12,
            ),
            Err(StorageError::StaleThreadRevision { .. })
        ));
        assert!(
            store
                .create_thread_follow_up_v2(
                    thread_id,
                    follow_up,
                    queued.revision,
                    "next",
                    &baseline,
                    12,
                )
                .unwrap_err()
                .to_string()
                .contains("ready thread")
        );

        let start = |command_id, thread_revision, run_revision, run_id| ThreadCommitRequest {
            thread_id,
            run_id,
            expected_thread_revision: thread_revision,
            expected_run_revision: run_revision,
            command_id,
            request_id: None,
            effect_id: None,
            update: CommitThreadRunUpdate::Start {
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
            store.commit_thread_run_update(
                &start(
                    latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision,
                    0,
                    run_id,
                ),
                &fenced,
                13,
            ),
            Err(StorageError::LeaseLost)
        ));
        assert!(matches!(
            store.commit_thread_run_update(
                &start(
                    latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision + 1,
                    0,
                    run_id,
                ),
                &lease,
                13,
            ),
            Err(StorageError::StaleThreadRevision { .. })
        ));
        let other_run = RunId::from_uuid(ids.next_uuid_v7());
        assert!(matches!(
            store.commit_thread_run_update(
                &start(
                    latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision,
                    0,
                    other_run,
                ),
                &lease,
                13,
            ),
            Err(StorageError::ThreadActiveRunMismatch)
        ));
        assert!(matches!(
            store.commit_thread_run_update(
                &start(
                    latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    queued.revision,
                    1,
                    run_id,
                ),
                &lease,
                13,
            ),
            Err(StorageError::StaleRevision { .. })
        ));

        let canonical = crate::ThreadEffectDescriptor {
            effect_id: "effect-matrix".into(),
            tool_call_id: "call_matrix".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path":"a.txt"}),
            attempt: 1,
        };
        let prepare = |snapshot: &ThreadSnapshot,
                       command_id: latte_core::ThreadCommandId,
                       source: &str| ThreadCommitRequest {
            thread_id,
            run_id,
            expected_thread_revision: snapshot.revision,
            expected_run_revision: snapshot.runs[0].run_revision,
            command_id,
            request_id: None,
            effect_id: Some("effect-matrix".into()),
            update: CommitThreadRunUpdate::PrepareEffect {
                source_key: source.into(),
                effect_id: "effect-matrix".into(),
                operation_digest: "a".repeat(64),
                descriptor_json: r#"{"name":"read_file","input":{"path":"a.txt"}}"#.into(),
                canonical_descriptor_json: serde_json::to_string(&canonical).unwrap(),
                policy: ThreadEffectPolicy::Allow,
                description: "read a.txt".into(),
                checkpoint_json: r#"{"phase":"prepared"}"#.into(),
            },
        };
        assert!(
            store
                .commit_thread_run_update(
                    &prepare(
                        &queued,
                        latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
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
            run_id,
            CommitThreadRunUpdate::Start {
                source_key: "matrix:start".into(),
            },
            15,
        )
        .snapshot;
        assert!(
            store
                .commit_thread_run_update(
                    &ThreadCommitRequest {
                        thread_id,
                        run_id,
                        expected_thread_revision: running.revision,
                        expected_run_revision: running.runs[0].run_revision,
                        command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                        request_id: None,
                        effect_id: Some("missing-effect".into()),
                        update: CommitThreadRunUpdate::StartEffect {
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
            .commit_thread_run_update(
                &prepare(
                    &running,
                    latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    "matrix:prepare",
                ),
                &lease,
                17,
            )
            .unwrap()
            .snapshot;
        let start_effect =
            |snapshot: &ThreadSnapshot, digest: String, source: &str| ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision: snapshot.runs[0].run_revision,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: Some("effect-matrix".into()),
                effect_id: Some("effect-matrix".into()),
                update: CommitThreadRunUpdate::StartEffect {
                    source_key: source.into(),
                    effect_id: "effect-matrix".into(),
                    operation_digest: digest,
                    checkpoint_json: r#"{"phase":"started"}"#.into(),
                },
            };
        assert!(
            store
                .commit_thread_run_update(
                    &start_effect(&running, "b".repeat(64), "matrix:wrong-digest"),
                    &lease,
                    18,
                )
                .unwrap_err()
                .to_string()
                .contains("digest mismatch")
        );
        running = store
            .commit_thread_run_update(
                &start_effect(&running, "a".repeat(64), "matrix:start-effect"),
                &lease,
                19,
            )
            .unwrap()
            .snapshot;
        let observe = |snapshot: &ThreadSnapshot, effect: &str, digest: String, source: &str| {
            ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: snapshot.revision,
                expected_run_revision: snapshot.runs[0].run_revision,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: Some(effect.into()),
                effect_id: Some(effect.into()),
                update: CommitThreadRunUpdate::ObserveEffect {
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
            store.commit_thread_run_update(
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
            .commit_thread_run_update(
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
            store.commit_thread_run_update(
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
            store.thread_snapshot_v2(
                latte_core::ThreadId::from_uuid(ids.next_uuid_v7()),
                None,
                10,
            ),
            Err(StorageError::ThreadNotFound(_))
        ));
        assert!(
            store
                .list_threads_v2()
                .unwrap()
                .iter()
                .any(|s| s.thread_id == thread_id)
        );
        let boundary = |snapshot: &ThreadSnapshot, update: CommitThreadRunUpdate| store.commit_thread_run_update(&ThreadCommitRequest { thread_id, run_id, expected_thread_revision: snapshot.revision, expected_run_revision: snapshot.runs[0].run_revision, command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()), request_id: None, effect_id: None, update }, &lease, 23); let running_state = store.load_run(run_id).unwrap(); let mut failed_state = running_state.clone(); failed_state.status = RunStatus::Failed; store.connection.lock().unwrap().execute("UPDATE runs SET state_json=?1 WHERE run_id=?2", params![serde_json::to_string(&failed_state).unwrap(), run_id.to_string()]).unwrap(); assert!(boundary(&observed.snapshot, CommitThreadRunUpdate::ObserveEffect { source_key: "matrix:observe-non-running".into(), effect_id: "effect-matrix".into(), operation_digest: "a".repeat(64), success: true, result: "late".into(), payload: None, checkpoint_json: "{}".into() }).unwrap_err().to_string().contains("requires a running linked child")); store.connection.lock().unwrap().execute("UPDATE runs SET state_json=?1 WHERE run_id=?2", params![serde_json::to_string(&running_state).unwrap(), run_id.to_string()]).unwrap(); assert!(boundary(&observed.snapshot, CommitThreadRunUpdate::CompleteVerified { source_key: "matrix:verify-without-evidence".into(), summary: "not verified".into(), verification_effect_id: "missing-verification".into(), verified_manifest_digest: "missing-manifest".into(), files_changed: vec![] }).is_err()); assert!(matches!(boundary(&observed.snapshot, CommitThreadRunUpdate::UnknownEffect { source_key: "matrix:unknown-missing".into(), effect_id: "missing-effect".into(), operation_digest: "a".repeat(64), checkpoint_json: "{}".into() }), Err(StorageError::EffectFenced))); store.connection.lock().unwrap().execute("UPDATE effects SET status='unknown' WHERE effect_id='effect-matrix'", []).unwrap(); assert_eq!(boundary(&observed.snapshot, CommitThreadRunUpdate::ReconcileUnknownEffect { source_key: "matrix:reconcile-running".into(), effect_id: "effect-matrix".into(), checkpoint_json: "{}".into() }).unwrap().snapshot.lifecycle, ThreadLifecycle::Failed); let (ask_thread, ask_run, ask) = create_linked_fixture(&store, &ids, "ask", 30); let ask_lease = store.acquire_thread_lease(ask_thread, 30, 10_000).unwrap(); let ask = commit_linked(&store, &ids, &ask_lease, &ask, ask_run, CommitThreadRunUpdate::Start { source_key: "ask:start".into() }, 31).snapshot; let ask_digest = "d".repeat(64); let ask_descriptor = crate::ThreadEffectDescriptor { effect_id: "ask-matrix".into(), tool_call_id: "ask-call".into(), name: "read_file".into(), input: serde_json::json!({"path":"a.txt"}), attempt: 1 };
        let ask = commit_linked(&store, &ids, &ask_lease, &ask, ask_run, CommitThreadRunUpdate::PrepareEffect { source_key: "ask:prepare".into(), effect_id: "ask-matrix".into(), operation_digest: ask_digest.clone(), descriptor_json: "{}".into(), canonical_descriptor_json: serde_json::to_string(&ask_descriptor).unwrap(), policy: ThreadEffectPolicy::Ask, description: "ask".into(), checkpoint_json: "{}".into() }, 32).snapshot;
        let start_ask = |snapshot: &ThreadSnapshot, source: &str| store.commit_thread_run_update(&ThreadCommitRequest { thread_id: ask_thread, run_id: ask_run, expected_thread_revision: snapshot.revision, expected_run_revision: snapshot.runs[0].run_revision, command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()), request_id: None, effect_id: Some("ask-matrix".into()), update: CommitThreadRunUpdate::StartEffect { source_key: source.into(), effect_id: "ask-matrix".into(), operation_digest: ask_digest.clone(), checkpoint_json: "{}".into() } }, &ask_lease, 33);
        assert!(start_ask(&ask, "ask:while-pending").unwrap_err().to_string().contains("without a pending request"));
        let allowed = commit_linked(&store, &ids, &ask_lease, &ask, ask_run, CommitThreadRunUpdate::ResolvePermission { source_key: "ask:allow".into(), request_id: "ask-matrix".into(), allow: true, rebound_operation_digest: None }, 34).snapshot;
        store.connection.lock().unwrap().execute("UPDATE pending_permissions SET run_revision=999 WHERE effect_id='ask-matrix'", []).unwrap(); assert!(start_ask(&allowed, "ask:stale").unwrap_err().to_string().contains("stale, mismatched, or consumed"));
        store.connection.lock().unwrap().execute("DELETE FROM pending_permissions WHERE effect_id='ask-matrix'", []).unwrap(); assert!(start_ask(&allowed, "ask:missing-auth").unwrap_err().to_string().contains("no durable allow authorization")); { let conn = store.connection.lock().unwrap(); conn.execute("UPDATE effects SET status='unknown' WHERE effect_id='ask-matrix'", []).unwrap(); conn.execute("DELETE FROM thread_active_runs_v2 WHERE thread_id=?1", [ask_thread.to_string()]).unwrap(); conn.execute("UPDATE threads_v2 SET lifecycle='reconciliation_required',latest_run_id=?1 WHERE thread_id=?2", params![ask_run.to_string(), ask_thread.to_string()]).unwrap(); } assert!(store.commit_thread_run_update(&ThreadCommitRequest { thread_id: ask_thread, run_id: ask_run, expected_thread_revision: allowed.revision, expected_run_revision: allowed.runs[0].run_revision, command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()), request_id: None, effect_id: None, update: CommitThreadRunUpdate::ReconcileUnknownEffect { source_key: "ask:invalid-recovered".into(), effect_id: "ask-matrix".into(), checkpoint_json: "{}".into() } }, &ask_lease, 36).unwrap_err().to_string().contains("requires an interrupted child"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    #[rustfmt::skip]
    fn projections_and_manifest_boundaries_fail_closed_on_corrupt_durable_rows() {
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let valid_key = serde_json::to_string(&vec!["src", "lib.rs"]).unwrap();
        let baseline = std::collections::BTreeMap::from([(valid_key.clone(), "old".into())]);
        let queued = RunState::queued(run_id);
        store
            .create_thread_v2(
                thread_id,
                run_id,
                &thread_binding(),
                "/workspace",
                "inspect projection",
                &baseline,
                1,
            )
            .unwrap();
        { let conn = store.connection.lock().unwrap(); conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap(); conn.execute("UPDATE threads_v2 SET latest_run_id='bad' WHERE thread_id=?1", [thread_id.to_string()]).unwrap(); }
        assert!(store.thread_snapshot_v2(thread_id, None, 10).unwrap_err().to_string().contains("invalid stored run id"));
        { let conn = store.connection.lock().unwrap(); conn.execute("UPDATE threads_v2 SET latest_run_id=?1 WHERE thread_id=?2", params![run_id.to_string(), thread_id.to_string()]).unwrap(); conn.execute("UPDATE thread_active_runs_v2 SET run_id='bad' WHERE thread_id=?1", [thread_id.to_string()]).unwrap(); }
        assert!(store.thread_snapshot_v2(thread_id, None, 10).unwrap_err().to_string().contains("invalid active run id"));
        { let conn = store.connection.lock().unwrap(); conn.execute("UPDATE thread_active_runs_v2 SET run_id=?1 WHERE thread_id=?2", params![run_id.to_string(), thread_id.to_string()]).unwrap(); conn.execute("UPDATE thread_runs_v2 SET parent_run_id='bad' WHERE run_id=?1", [run_id.to_string()]).unwrap(); }
        assert!(store.thread_snapshot_v2(thread_id, None, 10).unwrap_err().to_string().contains("invalid parent run id"));
        store.connection.lock().unwrap().execute("UPDATE thread_runs_v2 SET parent_run_id=NULL WHERE run_id=?1", [run_id.to_string()]).unwrap();

        assert!(
            store
                .thread_changed_files(run_id, &baseline)
                .unwrap()
                .is_empty()
        );
        let current = std::collections::BTreeMap::from([(valid_key, "new".into())]);
        assert_eq!(
            store.thread_changed_files(run_id, &current).unwrap(),
            vec!["src/lib.rs"]
        );
        let missing = RunId::from_uuid(ids.next_uuid_v7());
        assert!(
            store
                .thread_changed_files(missing, &std::collections::BTreeMap::new())
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
                    "UPDATE run_baselines SET manifest_json=?1 WHERE run_id=?2",
                    params![manifest, run_id.to_string()],
                )
                .unwrap();
            assert!(matches!(
                store.thread_changed_files(run_id, &std::collections::BTreeMap::new()),
                Err(StorageError::InvalidData(_))
            ));
        }
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE run_baselines SET manifest_json='{' WHERE run_id=?1",
                [run_id.to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.thread_changed_files(run_id, &std::collections::BTreeMap::new()),
            Err(StorageError::InvalidData(_))
        ));

        let binding_json = serde_json::to_string(&thread_binding()).unwrap();
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE threads_v2 SET binding_json='{' WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.thread_snapshot_v2(thread_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE threads_v2 SET binding_json=?1,lifecycle='invalid' WHERE thread_id=?2",
                params![binding_json, thread_id.to_string()],
            )
            .unwrap();
        }
        assert!(
            store
                .thread_snapshot_v2(thread_id, None, 10)
                .unwrap_err()
                .to_string()
                .contains("invalid thread lifecycle")
        );
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE threads_v2 SET lifecycle='running' WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE runs SET state_json='{' WHERE run_id=?1",
                [run_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.thread_snapshot_v2(thread_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE runs SET state_json=?1 WHERE run_id=?2",
                params![serde_json::to_string(&queued).unwrap(), run_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE thread_runs_v2 SET ordinal=-1 WHERE run_id=?1",
                [run_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.thread_snapshot_v2(thread_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE thread_runs_v2 SET ordinal=0,completed_at_ms=-1 WHERE run_id=?1",
                [run_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.thread_snapshot_v2(thread_id, None, 10),
            Err(StorageError::InvalidData(_))
        ));
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE thread_runs_v2 SET completed_at_ms=NULL WHERE run_id=?1",
                [run_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE conversation_outbox SET entry_json='{' WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.thread_snapshot_v2(thread_id, Some(0), 0),
            Err(StorageError::InvalidData(_))
        ));

        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "DELETE FROM thread_active_runs_v2 WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
            conn.execute(
                "UPDATE threads_v2 SET lifecycle='ready',latest_run_id=NULL WHERE thread_id=?1",
                [thread_id.to_string()],
            )
            .unwrap();
        }
        assert!(
            store
                .create_thread_follow_up_v2(
                    thread_id,
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
                "UPDATE threads_v2 SET latest_run_id=?1 WHERE thread_id=?2",
                params![run_id.to_string(), thread_id.to_string()],
            )
            .unwrap();
        }
        assert!(
            store
                .create_thread_follow_up_v2(
                    thread_id,
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
            let run_id = RunId::from_uuid(ids.next_uuid_v7());
            let queued = RunState::queued(run_id);
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
                        status: RunStatus::Running,
                    },
                    now_ms + 1,
                    &lease,
                )
                .unwrap();
            running
        };
        let record = |run: RunId, id: &str, passed: bool, manifest_digest: &str, now_ms: u64| {
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
                running.run_id,
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
                running.run_id,
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
                    running.run_id,
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

        record(running.run_id, "failed", false, "manifest", 21);
        assert!(
            store
                .complete_verified(
                    running.run_id,
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
        record(running.run_id, "stale-workspace", true, "before", 23);
        assert!(
            store
                .complete_verified(
                    running.run_id,
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
        record(running.run_id, "passing", true, "manifest", 25);
        let current = std::collections::BTreeMap::from([(valid_key.clone(), "new".into())]);
        let (completed, event) = store
            .complete_verified(
                running.run_id,
                1,
                &lease,
                "verified summary".into(),
                &current,
                "manifest",
                26,
            )
            .unwrap();
        assert_eq!(completed.status, RunStatus::Completed);
        assert_eq!(event.sequence, 2);
        let handoff = completed.handoff.unwrap();
        assert_eq!(handoff.summary, "verified summary");
        assert_eq!(handoff.files_changed, vec!["src/main.rs"]);
        assert_eq!(handoff.evidence[0].status, VerificationStatus::Passed);

        let without_baseline = start(None, 30);
        record(without_baseline.run_id, "no-baseline", true, "manifest", 32);
        assert!(
            store
                .complete_verified(
                    without_baseline.run_id,
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
        record(invalid.run_id, "invalid-path", true, "manifest", 42);
        assert!(
            store
                .complete_verified(
                    invalid.run_id,
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

        let (thread_id, run_id, queued) = create_linked_fixture(&store, &ids, "recover", 11);
        let linked_lease = store.acquire_thread_lease(thread_id, 10, 10).unwrap();
        let running = commit_linked(
            &store,
            &ids,
            &linked_lease,
            &queued,
            run_id,
            CommitThreadRunUpdate::Start {
                source_key: "recover:start".into(),
            },
            12,
        )
        .snapshot;
        assert!(
            store
                .recover_thread_after_lease_loss(
                    thread_id,
                    run_id,
                    &linked_lease,
                    running.runs[0].run_revision,
                    13,
                )
                .unwrap_err()
                .to_string()
                .contains("still authoritative")
        );
        assert!(matches!(
            store.put_checkpoint(run_id, 1, &linked_lease, "{}", 13),
            Err(StorageError::LeaseLost)
        ));
        assert!(
            store
                .put_checkpoint(
                    RunId::from_uuid(ids.next_uuid_v7()),
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
                .checkpoint(RunId::from_uuid(ids.next_uuid_v7()))
                .unwrap(),
            None
        );

        let missing = RunId::from_uuid(ids.next_uuid_v7());
        assert!(matches!(
            store.recover_thread_after_lease_loss(thread_id, missing, &linked_lease, 0, 21),
            Err(StorageError::RunNotFound(id)) if id == missing
        ));
        assert!(matches!(
            store
                .recover_thread_after_lease_loss(
                    thread_id,
                    run_id,
                    &linked_lease,
                    running.runs[0].run_revision + 1,
                    21,
                )
                .unwrap(),
            ThreadLeaseLossRecovery::FencedNoop
        ));
        let recovered = store
            .recover_thread_after_lease_loss(
                thread_id,
                run_id,
                &linked_lease,
                running.runs[0].run_revision,
                21,
            )
            .unwrap();
        assert!(matches!(
            recovered,
            ThreadLeaseLossRecovery::Recovered(response)
                if response.snapshot.lifecycle == ThreadLifecycle::Interrupted
        ));
        let recovered_run = store.load_run(run_id).unwrap(); assert!(matches!(store.recover_thread_after_lease_loss(thread_id, run_id, &linked_lease, recovered_run.revision, 22).unwrap(), ThreadLeaseLossRecovery::AlreadyTerminal(_)));
        assert!(matches!(
            store
                .interrupt_after_lease_loss(run_id, &linked_lease, recovered_run.revision, 22),
            Err(StorageError::LeaseLost)
        ));
        assert!(matches!(
            store
                .interrupt_after_lease_loss(run_id, &linked_lease, recovered_run.revision + 1, 22),
            Err(StorageError::LeaseLost)
        ));

        let next = store.acquire_lease("next", 22, 100).unwrap();
        let legacy = RunId::from_uuid(ids.next_uuid_v7());
        let queued = RunState::queued(legacy);
        store.create_run(&queued, 23).unwrap();
        let running = queued.transition(0, Transition::Start).unwrap();
        store
            .append_event(
                &running,
                0,
                EventId::from_uuid(ids.next_uuid_v7()),
                &RuntimeEvent::StateChanged {
                    status: RunStatus::Running,
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

        let input_run = RunId::from_uuid(ids.next_uuid_v7());
        let input_queued = RunState::queued(input_run);
        store.create_run(&input_queued, 30).unwrap();
        let (input_running, _) = store
            .apply_transition(input_run, 0, Transition::Start, 31, &next)
            .unwrap();
        let (waiting_input, _) = store
            .apply_transition(
                input_run,
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
                .cancel_waiting(input_run, waiting_input.revision, &next, 33, true)
                .unwrap_err()
                .to_string()
                .contains("not waiting for permission")
        );
        let (cancelled, event) = store
            .cancel_waiting(input_run, waiting_input.revision, &next, 34, false)
            .unwrap();
        assert_eq!(cancelled.status, RunStatus::Failed);
        assert_eq!(cancelled.failure.unwrap().code, FailureCode::Cancelled);
        assert!(event.is_some());
        let (terminal, duplicate) = store
            .cancel_waiting(input_run, cancelled.revision, &next, 35, false)
            .unwrap();
        assert_eq!(terminal.status, RunStatus::Failed);
        assert!(duplicate.is_none());

        let running_only = RunId::from_uuid(ids.next_uuid_v7());
        let queued = RunState::queued(running_only);
        store.create_run(&queued, 40).unwrap();
        let (running, _) = store
            .apply_transition(running_only, 0, Transition::Start, 41, &next)
            .unwrap();
        assert!(
            store
                .cancel_waiting(running_only, running.revision, &next, 42, false)
                .unwrap_err()
                .to_string()
                .contains("run is not waiting")
        );
        assert!(store.append_event(&running, running.revision, EventId::from_uuid(ids.next_uuid_v7()), &RuntimeEvent::StateChanged { status: RunStatus::Running }, 43, &next).unwrap_err().to_string().contains("must increment once"));
        assert!(matches!(store.apply_transition(running_only, 0, Transition::Cancel, 43, &next), Err(StorageError::StaleRevision { .. }))); assert!(matches!(store.apply_transition(running_only, running.revision, Transition::Cancel, 43, &forged), Err(StorageError::LeaseLost)));
        store.connection.lock().unwrap().execute("UPDATE runs SET lease_token=?1 WHERE run_id=?2", params![to_i64(next.fencing_token + 1).unwrap(), running_only.to_string()]).unwrap(); assert!(matches!(store.apply_transition(running_only, running.revision, Transition::Cancel, 43, &next), Err(StorageError::LeaseLost))); let cancelling = running.transition(running.revision, Transition::Cancel).unwrap(); assert!(matches!(store.append_event(&cancelling, running.revision, EventId::from_uuid(ids.next_uuid_v7()), &RuntimeEvent::StateChanged { status: RunStatus::Cancelling }, 43, &next), Err(StorageError::LeaseLost))); store.connection.lock().unwrap().execute("UPDATE runs SET lease_token=?1 WHERE run_id=?2", params![to_i64(next.fencing_token).unwrap(), running_only.to_string()]).unwrap();
        store.start_effect("invalid-status", running_only, 44).unwrap(); store.connection.lock().unwrap().execute("UPDATE effects SET status='invalid' WHERE effect_id='invalid-status'", []).unwrap(); assert!(matches!(store.effect_status("invalid-status"), Err(StorageError::InvalidData(_)))); assert!(store.prepare_effect("missing", "digest", "{}", 45).is_err()); assert!(store.start_prepared_effect("missing", "digest", 45).is_err()); let invalid_authority = EffectAuthority { run_id: running_only, expected_revision: running.revision, lease: next.clone(), effect_id: "invalid-status".into(), digest: String::new(), attempt: 0 }; assert!(matches!(store.mark_effect_unknown(&invalid_authority, 45), Err(StorageError::EffectFenced))); assert!(matches!(store.replace_pending_effect("missing", "replacement", running_only, running.revision, 1, "{}", "digest", &next, 45), Err(StorageError::LeaseLost))); assert!(store.apply_transition(running_only, running.revision, Transition::Complete { handoff: Handoff { summary: "done".into(), files_changed: vec![], evidence: vec![] }, policy: CompletionPolicy::VerificationNotRequired }, 46, &next).is_ok()); store.release_lease(&next).unwrap();
    }

    #[test]
    fn focus_is_persisted_and_returned_in_snapshot() {
        use latte_core::ThreadId;
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_thread_lease(thread_id, 1, 100).unwrap();
        let outcome = store
            .create_started_thread_v2(
                None,
                thread_id,
                run_id,
                &thread_binding(),
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
        let loaded = store.thread_snapshot_v2(thread_id, None, 500).unwrap();
        assert_eq!(loaded.focus.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn create_started_thread_v2_is_idempotent() {
        use latte_core::ThreadId;
        let store = Storage::memory().unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = RunId::from_uuid(ids.next_uuid_v7());
        let lease = store.acquire_thread_lease(thread_id, 1, 100).unwrap();

        // First create
        let first = store
            .create_started_thread_v2(
                None,
                thread_id,
                run_id,
                &thread_binding(),
                "/workspace",
                "hello",
                &std::collections::BTreeMap::new(),
                &lease,
                2,
                None,
            )
            .unwrap();
        assert!(matches!(first, latte_core::CreateOutcome::Created(_)));

        // Second create with same thread_id should replay
        let second = store
            .create_started_thread_v2(
                None,
                thread_id,
                run_id,
                &thread_binding(),
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
}
