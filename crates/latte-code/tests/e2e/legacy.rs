use super::support::{PtySession, Scenario, ScriptedProvider, json};
use latte_core::{IdSource, RunId, SystemIdSource, ThreadId, ThreadProviderBindingV2};
use rusqlite::Connection;
use std::{collections::BTreeMap, path::Path, time::Duration};

const F10: &[u8] = b"\x1b[21~";

fn sqlite_execute(database: &Path, sql: &str) {
    Connection::open(database)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

fn sqlite_integer(database: &Path, query: &str) -> i64 {
    Connection::open(database)
        .unwrap()
        .query_row(query, [], |row| row.get(0))
        .unwrap()
}

fn sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn state_json(run_id: &str, revision: u64, status: &str) -> String {
    serde_json::json!({
        "run_id": run_id,
        "revision": revision,
        "status": status,
        "pending_permission": null,
        "pending_input": null,
        "failure": null,
        "handoff": null
    })
    .to_string()
}

fn seed_v1_running_run(scenario: &Scenario, run_id: &str) {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    let state = sql_literal(&state_json(run_id, 1, "running"));
    sqlite_execute(
        &scenario.database_path(),
        &format!(
            r"
            PRAGMA foreign_keys=ON;
            CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL);
            CREATE TABLE sessions(id TEXT PRIMARY KEY, created_at_ms INTEGER NOT NULL);
            CREATE TABLE runs(
              run_id TEXT PRIMARY KEY, state_json TEXT NOT NULL, status TEXT NOT NULL,
              revision INTEGER NOT NULL, last_seq INTEGER NOT NULL DEFAULT 0,
              lease_token INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL,
              updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE events(
              run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
              seq INTEGER NOT NULL, event_id TEXT NOT NULL UNIQUE, revision INTEGER NOT NULL,
              event_json TEXT NOT NULL, created_at_ms INTEGER NOT NULL, PRIMARY KEY(run_id, seq)
            );
            CREATE TABLE command_dedup(
              command_id TEXT PRIMARY KEY, result_json TEXT NOT NULL, created_at_ms INTEGER NOT NULL
            );
            CREATE TABLE effects(
              effect_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
              status TEXT NOT NULL, started_at_ms INTEGER NOT NULL, observed_at_ms INTEGER
            );
            CREATE TABLE effect_attempts(
              effect_id TEXT NOT NULL REFERENCES effects(effect_id) ON DELETE CASCADE,
              attempt INTEGER NOT NULL, status TEXT NOT NULL, metadata_json TEXT NOT NULL,
              PRIMARY KEY(effect_id, attempt)
            );
            CREATE TABLE evidence(
              id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
              metadata_json TEXT NOT NULL, blob_ref TEXT
            );
            CREATE TABLE run_read_model(
              run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE CASCADE,
              revision INTEGER NOT NULL, last_seq INTEGER NOT NULL, state_json TEXT NOT NULL
            );
            CREATE TABLE runtime_lease(
              singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL,
              fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at_ms) VALUES(1, 1);
            INSERT INTO runs(
              run_id, state_json, status, revision, last_seq, lease_token,
              created_at_ms, updated_at_ms
            ) VALUES('{run_id}', '{state}', 'running', 1, 0, 7, 1, 1);
            INSERT INTO run_read_model(run_id, revision, last_seq, state_json)
              VALUES('{run_id}', 1, 0, '{state}');
            PRAGMA user_version=1;
            "
        ),
    );
}

#[test]
fn v1_running_run_migrates_and_recovers_through_final_binary_restarts() {
    let scenario = Scenario::new();
    let run_id = "01900000-0000-7000-8000-0000000000a1";
    seed_v1_running_run(&scenario, run_id);

    let migrated = scenario.output(&["--json", "list"], |_| {});
    assert!(
        migrated.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&migrated.stdout),
        String::from_utf8_lossy(&migrated.stderr)
    );
    assert_eq!(json(&migrated)["data"]["runs"][0]["run_id"], run_id);
    assert_eq!(json(&migrated)["data"]["runs"][0]["status"], "interrupted");
    assert_eq!(json(&migrated)["data"]["runs"][0]["revision"], 2);

    let shown = scenario.output(&["--json", "show", run_id], |_| {});
    assert_eq!(shown.status.code(), Some(130));
    assert_eq!(json(&shown)["status"], "interrupted");
    assert_eq!(json(&shown)["data"]["run"]["status"], "interrupted");
    assert_eq!(json(&shown)["data"]["run"]["revision"], 2);

    let reopened = scenario.output(&["--json", "list"], |_| {});
    assert!(reopened.status.success());
    assert_eq!(json(&reopened)["data"]["runs"][0]["revision"], 2);
    assert_eq!(
        sqlite_integer(&scenario.database_path(), "PRAGMA user_version;"),
        11
    );
    assert_eq!(
        sqlite_integer(
            &scenario.database_path(),
            "SELECT COUNT(*) FROM schema_migrations;"
        ),
        11
    );
    assert_eq!(
        sqlite_integer(
            &scenario.database_path(),
            "SELECT COUNT(*) FROM thread_effect_canonical_v2;"
        ),
        0
    );
}

#[test]
fn v7_versionless_checkpoint_migrates_but_resume_fails_closed_without_provider() {
    let scenario = Scenario::new();
    let initialized = scenario.output(&["--json", "list"], |_| {});
    assert!(initialized.status.success());

    let run_id = "01900000-0000-7000-8000-0000000000b7";
    let state = sql_literal(&state_json(run_id, 2, "interrupted"));
    let checkpoint = sql_literal(
        &serde_json::json!({
            "messages": [],
            "pending": null,
            "final_message": null,
            "baseline": {},
            "tool_queue": [],
            "tool_cursor": 0,
            "pending_input": null
        })
        .to_string(),
    );
    sqlite_execute(
        &scenario.database_path(),
        &format!(
            r"
            PRAGMA foreign_keys=ON;
            DROP TABLE thread_effect_canonical_v2;
            DROP TABLE legacy_imports;
            DROP TABLE workspaces;
            DROP TABLE projects;
            DROP TABLE runtime_lease;
            DROP TABLE runtime_lease_epoch;
            CREATE TABLE runtime_lease(
              singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL,
              fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL
            );
            DELETE FROM schema_migrations WHERE version IN (8,9,10,11);
            PRAGMA user_version=7;
            INSERT INTO runs(
              run_id, state_json, status, revision, last_seq, lease_token,
              created_at_ms, updated_at_ms
            ) VALUES('{run_id}', '{state}', 'interrupted', 2, 0, 0, 1, 1);
            INSERT INTO run_read_model(run_id, revision, last_seq, state_json)
              VALUES('{run_id}', 2, 0, '{state}');
            INSERT INTO runtime_checkpoints(run_id, payload_json, updated_at_ms)
              VALUES('{run_id}', '{checkpoint}', 1);
            "
        ),
    );

    let provider = ScriptedProvider::start([]);
    scenario.write_config(provider.endpoint(), r#"["/usr/bin/true"]"#);
    let resume = |scenario: &Scenario| {
        scenario.output(&["--json", "resume", run_id, "--allow"], |command| {
            command.env("TEST_OPENAI_KEY", "local-fixture-only");
        })
    };

    let first = resume(&scenario);
    assert_eq!(first.status.code(), Some(1));
    assert_eq!(json(&first)["status"], "failed");
    assert_eq!(json(&first)["error"]["code"], "runtime");
    assert!(
        json(&first)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("legacy/versionless")
    );

    let shown = scenario.output(&["--json", "show", run_id], |_| {});
    assert_eq!(
        shown.status.code(),
        Some(130),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&shown.stdout),
        String::from_utf8_lossy(&shown.stderr)
    );
    assert_eq!(json(&shown)["data"]["run"]["status"], "interrupted");
    assert_eq!(json(&shown)["data"]["run"]["revision"], 2);

    let second = resume(&scenario);
    assert_eq!(second.status.code(), Some(1));
    assert!(
        json(&second)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("legacy/versionless")
    );
    assert!(provider.requests().is_empty());
    provider.assert_consumed();
    assert_eq!(
        sqlite_integer(&scenario.database_path(), "PRAGMA user_version;"),
        11
    );
    assert_eq!(
        sqlite_integer(
            &scenario.database_path(),
            "SELECT COUNT(*) FROM schema_migrations WHERE version=11;"
        ),
        1
    );
}

#[test]
fn newer_schema_fails_as_typed_engine_initialization_error() {
    let scenario = Scenario::new();
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    sqlite_execute(&scenario.database_path(), "PRAGMA user_version=99;");

    let output = scenario.output(&["--json", "list"], |_| {});
    assert_eq!(output.status.code(), Some(70));
    assert_eq!(json(&output)["status"], "internal");
    assert_eq!(json(&output)["error"]["code"], "engine_initialization");
    assert_eq!(
        json(&output)["error"]["message"],
        "database schema version 99 is newer than supported version 11"
    );
}

#[cfg(unix)]
#[test]
fn v9_workspace_session_imports_unchanged_then_reopens_in_final_tui() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    let legacy_path = scenario.root().join(".latte/latte-code.db");
    std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(&legacy_path)
        .build()
        .unwrap();
    engine
        .create_thread_v2(
            thread_id,
            run_id,
            ThreadProviderBindingV2 {
                version: 1,
                provider_name: "main".into(),
                provider_type: "openai-chat".into(),
                protocol: "openai-chat-completions-v1".into(),
                model: "mock".into(),
                config_fingerprint: "legacy-v8-config".into(),
                tools_fingerprint: "legacy-v8-tools".into(),
                aliases: BTreeMap::new(),
                credential_ref_id: "env:TEST_OPENAI_KEY".into(),
                data_scope_id: "workspace".into(),
                credential_generation: 1,
            },
            "restore the legacy session catalog title",
            latte_core::wall_time_ms(),
        )
        .unwrap();
    drop(engine);

    // Reconstruct the historical v9 authority schema. This is deliberately a
    // compatibility fixture: current state was first created through the
    // public Engine, and acceptance still comes from a fresh final TUI.
    sqlite_execute(
        &legacy_path,
        r"
        PRAGMA foreign_keys=OFF;
        DROP TABLE legacy_imports;
        DROP TABLE workspaces;
        DROP TABLE projects;
        DROP TABLE runtime_lease;
        DROP TABLE runtime_lease_epoch;
        CREATE TABLE runtime_lease(
          singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL,
          fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL
        );
        DELETE FROM schema_migrations WHERE version IN (10,11);
        PRAGMA user_version=9;
        PRAGMA foreign_keys=ON;
        ",
    );

    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(
        pty.wait_for_output(b"Latte Code", Duration::from_secs(10)),
        "imported v9 TUI did not render: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(format!("/resume {thread_id}\r").as_bytes());
    assert!(
        pty.wait_for_output(
            b"restore the legacy session catalog title",
            Duration::from_secs(5)
        ),
        "imported v9 session did not render: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
    assert_eq!(
        sqlite_integer(&scenario.database_path(), "PRAGMA user_version;"),
        11
    );
    assert_eq!(sqlite_integer(&legacy_path, "PRAGMA user_version;"), 9);
    assert_eq!(
        Connection::open(scenario.database_path())
            .unwrap()
            .query_row(
                "SELECT title FROM threads_v2 WHERE thread_id=?1",
                [thread_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "restore the legacy session catalog title"
    );
}
