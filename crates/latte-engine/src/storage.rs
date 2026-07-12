//! Private `SQLite` authority for durable engine state.
use crate::VerificationEvidence;
use latte_core::{
    CompletionPolicy, EventEnvelope, EventId, Evidence, FailureCode, Handoff, PROTOCOL_VERSION,
    Retryability, RunFailure, RunId, RunState, RunStatus, RuntimeEvent, Transition,
    VerificationStatus,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{path::Path, sync::Mutex};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 6;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub sequence: u64,
    pub envelope: EventEnvelope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub(crate) owner: String,
    pub(crate) fencing_token: u64,
    pub(crate) expires_at_ms: u64,
}
impl Lease {
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
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Self::bootstrap(&connection)?;
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
        Self::bootstrap(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn bootstrap(connection: &Connection) -> Result<(), StorageError> {
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owns_lease: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2 AND expires_at_ms>?3)",
            params![lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2 AND expires_at_ms>?3)",params![lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT owner,fencing_token,expires_at_ms FROM runtime_lease WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let current = current
            .map(|(owner, token, expires)| -> Result<_, StorageError> {
                Ok((owner, from_i64(token)?, from_i64(expires)?))
            })
            .transpose()?;
        let token = match current {
            None => 1,
            Some((_, token, expires)) if expires <= now_ms => token
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidData("fencing token overflow".into()))?,
            Some((ref held, token, _)) if held == owner => token,
            Some(_) => return Err(StorageError::EngineUnavailable),
        };
        let expires = now_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| StorageError::InvalidData("lease expiry overflow".into()))?;
        tx.execute("INSERT INTO runtime_lease(singleton,owner,fencing_token,expires_at_ms) VALUES(1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET owner=excluded.owner,fencing_token=excluded.fencing_token,expires_at_ms=excluded.expires_at_ms", params![owner,to_i64(token)?,to_i64(expires)?])?;
        tx.execute(
            "UPDATE runs SET lease_token=?1 WHERE status='queued'",
            [to_i64(token)?],
        )?;
        tx.commit()?;
        Ok(Lease {
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
        let changed = conn.execute("UPDATE runtime_lease SET expires_at_ms=?1 WHERE singleton=1 AND owner=?2 AND fencing_token=?3 AND expires_at_ms>?4", params![to_i64(expires)?,lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?])?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        Ok(Lease {
            expires_at_ms: expires,
            ..lease.clone()
        })
    }

    pub(crate) fn release_lease(&self, lease: &Lease) -> Result<(), StorageError> {
        let conn = self.connection.lock().expect("storage mutex poisoned");
        let changed = conn.execute(
            "DELETE FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2",
            params![lease.owner, to_i64(lease.fencing_token)?],
        )?;
        if changed != 1 {
            return Err(StorageError::LeaseLost);
        }
        Ok(())
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
        serde_json::from_str::<serde_json::Value>(post_evidence_json).map_err(invalid_json)?;
        let status = if success {
            "observed_success"
        } else {
            "observed_failed"
        };
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE effects SET status=?1,post_evidence_json=?2,observed_at_ms=?3 WHERE effect_id=?4 AND run_id=?5 AND status='started' AND attempt=?6 AND approval_digest=?7 AND EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?8 AND fencing_token=?9 AND expires_at_ms>?3) AND EXISTS(SELECT 1 FROM runs WHERE run_id=?5 AND revision=?10 AND lease_token=?9)",params![status,post_evidence_json,to_i64(now_ms)?,authority.effect_id,authority.run_id.to_string(),to_i64(authority.attempt)?,authority.digest,authority.lease.owner,to_i64(authority.lease.fencing_token)?,to_i64(authority.expected_revision)?])?;
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE effects SET status='unknown',observed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND status='started' AND attempt=?4 AND approval_digest=?5 AND EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?6 AND fencing_token=?7 AND expires_at_ms>?1) AND EXISTS(SELECT 1 FROM runs WHERE run_id=?3 AND revision=?8 AND lease_token=?7)",params![to_i64(now_ms)?,authority.effect_id,authority.run_id.to_string(),to_i64(authority.attempt)?,authority.digest,authority.lease.owner,to_i64(authority.lease.fencing_token)?,to_i64(authority.expected_revision)?])?;
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
        serde_json::from_str::<serde_json::Value>(descriptor).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease l JOIN runs r ON r.run_id=?1 WHERE l.singleton=1 AND l.owner=?2 AND l.fencing_token=?3 AND l.expires_at_ms>?4 AND r.revision=?5 AND r.lease_token=?3)",params![run_id.to_string(),lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?,to_i64(expected_revision)?],|r|r.get(0))?;
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE pending_permissions SET consumed_at_ms=?1 WHERE effect_id=?2 AND run_id=?3 AND run_revision=?4 AND lease_owner=?5 AND lease_token=?6 AND approval_digest=?7 AND consumed_at_ms IS NULL AND EXISTS(SELECT 1 FROM runs WHERE run_id=?3 AND revision=?4) AND EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?5 AND fencing_token=?6 AND expires_at_ms>?1)",params![to_i64(now_ms)?,effect_id,run_id.to_string(),to_i64(run_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest])?;
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
        serde_json::from_str::<serde_json::Value>(descriptor_json).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expected_revision = run_revision.saturating_sub(2);
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease l JOIN runs r ON r.run_id=?1 JOIN pending_permissions p ON p.run_id=r.run_id JOIN effects e ON e.effect_id=p.effect_id AND e.run_id=p.run_id WHERE l.singleton=1 AND l.owner=?2 AND l.fencing_token=?3 AND l.expires_at_ms>?4 AND r.revision=?5 AND r.lease_token=?3 AND p.effect_id=?6 AND p.consumed_at_ms IS NULL AND e.status='prepared' AND e.approval_digest=p.approval_digest AND (p.lease_owner<>?2 OR p.lease_token<>?3))",params![run_id.to_string(),lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?,to_i64(expected_revision)?,old_effect_id],|r|r.get(0))?;
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
        let conn = self.connection.lock().expect("storage mutex poisoned");
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM pending_permissions p JOIN runs r ON r.run_id=p.run_id JOIN runtime_lease l ON l.singleton=1 WHERE p.effect_id=?1 AND p.run_id=?2 AND p.run_revision=?3 AND p.lease_owner=?4 AND p.lease_token=?5 AND p.approval_digest=?6 AND p.consumed_at_ms IS NULL AND r.revision+1=?3 AND l.owner=?4 AND l.fencing_token=?5 AND l.expires_at_ms>?7)",params![effect_id,run_id.to_string(),to_i64(expected_revision)?,lease.owner,to_i64(lease.fencing_token)?,digest,to_i64(now_ms)?],|r|r.get(0))?)
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
    pub(crate) fn interrupt_after_lease_loss(
        &self,
        run_id: RunId,
        lost_lease: &Lease,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<LeaseLossRecovery, StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2 AND expires_at_ms>?3)",params![lost_lease.owner,to_i64(lost_lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authoritative:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2 AND expires_at_ms>?3)",params![lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|r|r.get(0))?;
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "INSERT INTO evidence(id,run_id,metadata_json,blob_ref) \
             SELECT ?1,?2,?3,?4 \
             WHERE EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?5 AND fencing_token=?6 AND expires_at_ms>?7) \
             AND EXISTS(SELECT 1 FROM runs WHERE run_id=?2 AND revision=?8 AND effect_epoch=?9 AND lease_token=?6)",
            params![
                evidence.id,
                run_id.to_string(),
                evidence.metadata_json,
                evidence.blob_ref,
                lease.owner,
                to_i64(lease.fencing_token)?,
                to_i64(now_ms)?,
                to_i64(expected_revision)?,
                to_i64(serde_json::from_str::<VerificationRecord>(evidence.metadata_json).map_err(invalid_json)?.effect_epoch)?,
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2 AND expires_at_ms>?3)",
            params![lease.owner, to_i64(lease.fencing_token)?, to_i64(now_ms)?],
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
        serde_json::from_str::<serde_json::Value>(payload).map_err(invalid_json)?;
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("INSERT INTO runtime_checkpoints(run_id,payload_json,updated_at_ms) SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?4 AND fencing_token=?5 AND expires_at_ms>?3) AND EXISTS(SELECT 1 FROM runs WHERE run_id=?1 AND revision=?6 AND lease_token=?5) ON CONFLICT(run_id) DO UPDATE SET payload_json=excluded.payload_json,updated_at_ms=excluded.updated_at_ms",params![run_id.to_string(),payload,to_i64(now_ms)?,lease.owner,to_i64(lease.fencing_token)?,to_i64(expected_revision)?])?;
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
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_lease WHERE singleton=1 AND owner=?1 AND fencing_token=?2 AND expires_at_ms>?3)",params![lease.owner,to_i64(lease.fencing_token)?,to_i64(now_ms)?],|row|row.get(0))?;
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

    fn recover_at(&self, now_ms: u64) -> Result<(), StorageError> {
        let mut conn = self.connection.lock().expect("storage mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut stmt = tx.prepare(
            "SELECT r.run_id,r.state_json,r.last_seq FROM runs r WHERE r.status IN ('running','cancelling') AND NOT EXISTS(SELECT 1 FROM runtime_lease l WHERE l.singleton=1 AND l.fencing_token=r.lease_token AND l.expires_at_ms>?1)",
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
        Ok(())
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
}
