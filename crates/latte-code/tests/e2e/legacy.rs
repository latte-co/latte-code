use super::support::{PtySession, Scenario, json};
use latte_core::{IdSource, SessionId, SessionProviderBinding, SystemIdSource, TurnId};
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

fn sqlite_text(database: &Path, query: &str) -> String {
    Connection::open(database)
        .unwrap()
        .query_row(query, [], |row| row.get(0))
        .unwrap()
}

fn sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn state_json(turn_id: &str, revision: u64, status: &str) -> String {
    serde_json::json!({
        "run_id": turn_id,
        "revision": revision,
        "status": status,
        "pending_permission": null,
        "pending_input": null,
        "failure": null,
        "handoff": null
    })
    .to_string()
}

fn seed_v1_running_run(scenario: &Scenario, turn_id: &str) {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    let state = sql_literal(&state_json(turn_id, 1, "running"));
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
            ) VALUES('{turn_id}', '{state}', 'running', 1, 0, 7, 1, 1);
            INSERT INTO run_read_model(run_id, revision, last_seq, state_json)
              VALUES('{turn_id}', 1, 0, '{state}');
            PRAGMA user_version=1;
            "
        ),
    );
}

#[test]
fn v1_running_run_migrates_and_recovers_through_final_binary_restarts() {
    let scenario = Scenario::new();
    let turn_id = "01900000-0000-7000-8000-0000000000a1";
    seed_v1_running_run(&scenario, turn_id);

    // v2 session commands open the database through an embedded server. The
    // first open migrates the v1 schema to v12; the server's recovery sweeper
    // then marks the orphaned v1 running run as interrupted. A v1 run is not a
    // v2 session, so it is never surfaced as a session: `list` returns an empty
    // session catalogue and `show <turn_id>` fails closed with not_found. Each
    // `list` invocation is a fresh final binary, so the retry loop also proves
    // the migration/recovery is stable across restarts.
    let recovered = std::iter::repeat_with(|| {
        let migrated = scenario.output(&["--json", "list"], |command| {
            command.env("LATTE_RECOVERY_SWEEP_MS", "1");
        });
        assert!(
            migrated.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&migrated.stdout),
            String::from_utf8_lossy(&migrated.stderr)
        );
        assert_eq!(json(&migrated)["data"]["sessions"], serde_json::json!([]));
        std::thread::sleep(Duration::from_millis(20));
        sqlite_integer(
            &scenario.database_path(),
            "SELECT COUNT(*) FROM turns WHERE turn_id='01900000-0000-7000-8000-0000000000a1' AND status='interrupted' AND revision=2;",
        )
    })
    .take(100)
    .find(|&count| count == 1)
    .is_some();
    assert!(
        recovered,
        "v1 running run was not recovered to interrupted through final binary restarts"
    );

    // The v1 run is not addressable as a v2 session.
    let shown = scenario.output(&["--json", "show", turn_id], |_| {});
    assert_eq!(shown.status.code(), Some(4));
    assert_eq!(json(&shown)["status"], "failed");
    assert_eq!(json(&shown)["error"]["code"], "not_found");

    // A later final-binary open is stable: the recovered turn stays interrupted
    // at revision 2, the schema stays at v15, and no session effects were adopted.
    let reopened = scenario.output(&["--json", "list"], |_| {});
    assert!(reopened.status.success());
    assert_eq!(json(&reopened)["data"]["sessions"], serde_json::json!([]));
    assert_eq!(
        sqlite_text(
            &scenario.database_path(),
            "SELECT status FROM turns WHERE turn_id='01900000-0000-7000-8000-0000000000a1';"
        ),
        "interrupted"
    );
    assert_eq!(
        sqlite_integer(
            &scenario.database_path(),
            "SELECT revision FROM turns WHERE turn_id='01900000-0000-7000-8000-0000000000a1';"
        ),
        2
    );
    assert_eq!(
        sqlite_integer(&scenario.database_path(), "PRAGMA user_version;"),
        16
    );
    assert_eq!(
        sqlite_integer(
            &scenario.database_path(),
            "SELECT COUNT(*) FROM schema_migrations;"
        ),
        16
    );
    assert_eq!(
        sqlite_integer(
            &scenario.database_path(),
            "SELECT COUNT(*) FROM session_effect_canonical;"
        ),
        0
    );
}

// `v7_versionless_checkpoint_migrates_but_resume_fails_closed_without_provider`
// was removed in the v2 migration. It drove the v1 `resume <run-id> --allow`
// workflow, asserting that resuming a versionless checkpoint fails closed with
// the `legacy/versionless` error. That resume path lived in the v1
// `AgentRuntime` and is unreachable from the v2 session-command contract
// (`resume <session-id> <prompt>` is a session follow-up, not a checkpoint
// resume; a v1 run id is not a session and fails closed as `not_found`). The
// v7 -> v12 schema migration it also exercised is covered by the v1 -> v12
// recovery test above and by the engine storage migration tests.

#[test]
fn newer_schema_fails_as_typed_engine_initialization_error() {
    let scenario = Scenario::new();
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    sqlite_execute(&scenario.database_path(), "PRAGMA user_version=99;");

    // v2 routes every session command through the embedded HTTP server, whose
    // startup builds the workspace engine. A newer-than-supported schema fails
    // that build, which the client classifies as an internal setup failure
    // (exit 70, code `internal`), matching the standalone `serve` command's
    // `exit_for_setup` mapping; the engine's typed message is preserved.
    let output = scenario.output(&["--json", "list"], |_| {});
    assert_eq!(output.status.code(), Some(70));
    assert_eq!(json(&output)["status"], "failed");
    assert_eq!(json(&output)["error"]["code"], "internal");
    assert_eq!(
        json(&output)["error"]["message"],
        "server setup: database schema version 99 is newer than supported version 16"
    );
}

#[cfg(unix)]
#[test]
fn v9_workspace_session_imports_unchanged_then_reopens_in_final_tui() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    let legacy_path = scenario.root().join(".latte/latte-code.db");
    std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    let session_id = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let turn_id = TurnId::from_uuid(SystemIdSource::default().next_uuid_v7());
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(&legacy_path)
        .build()
        .unwrap();
    engine
        .create_session_v2(
            session_id,
            turn_id,
            SessionProviderBinding {
                binding_version: 1,
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
    // public Engine, and acceptance still comes from a fresh final TUI. The
    // database is built at schema 13, so first undo migration 13's object
    // renames (back to the v9-era `threads_v2` / `thread_id` shape), then drop
    // the v10/v11 infrastructure the v9 schema predates.
    sqlite_execute(
        &legacy_path,
        &format!(
            "{} \
             DROP TABLE legacy_imports; \
             DROP TABLE workspaces; \
             DROP TABLE projects; \
             DROP TABLE runtime_lease; \
             DROP TABLE runtime_lease_epoch; \
             CREATE TABLE runtime_lease( \
               singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL, \
               fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL \
             ); \
             DELETE FROM schema_migrations WHERE version IN (10,11,12); \
             PRAGMA user_version=9; \
             PRAGMA foreign_keys=ON;",
            super::support::REVERSE_TO_SCHEMA_12_SQL
        ),
    );

    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(
        pty.wait_for_output(b"Latte Code", Duration::from_secs(10)),
        "imported v9 TUI did not render: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(format!("/resume {session_id}\r").as_bytes());
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
        16
    );
    assert_eq!(sqlite_integer(&legacy_path, "PRAGMA user_version;"), 9);
    assert_eq!(
        Connection::open(scenario.database_path())
            .unwrap()
            .query_row(
                "SELECT title FROM sessions WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "restore the legacy session catalog title"
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn v8_titleless_database_migrates_and_incompatible_legacy_db_warns_cleanly() {
    // 1. Seed a schema-8 global `state.db` in pure SQL (zero in-process Engine calls!):
    //    opening a shared v8 catalog without a workspace identity must fail closed cleanly.
    let v8_global = Scenario::new();
    v8_global.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    std::fs::create_dir_all(v8_global.database_path().parent().unwrap()).unwrap();
    sqlite_execute(
        &v8_global.database_path(),
        r"
        CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL);
        CREATE TABLE threads_v2(
          thread_id TEXT PRIMARY KEY,
          revision INTEGER NOT NULL,
          last_seq INTEGER NOT NULL,
          lifecycle TEXT NOT NULL,
          binding_json TEXT NOT NULL,
          latest_run_id TEXT,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL
        );
        INSERT INTO threads_v2(thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms)
          VALUES('01900000-0000-7000-8000-0000000000b8',1,1,'ready','{}',NULL,1,1);
        INSERT INTO schema_migrations(version,applied_at_ms) VALUES(8,1);
        PRAGMA user_version=8;
        ",
    );
    let rejected = v8_global.output(&["--json", "list"], |_| {});
    assert_eq!(rejected.status.code(), Some(70));
    assert!(
        json(&rejected)["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("legacy Sessions have no workspace identity"),
        "unexpected v8 global error: {}",
        String::from_utf8_lossy(&rejected.stdout)
    );

    // 2. Place a post-v12 (schema 16) `.latte/latte-code.db` inside a workspace checkout
    //    (minted via a prior final-binary `list` run, zero in-process Engine calls!):
    //    human-readable `latte-code list` must emit a skip warning rather than bricking.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    let init = scenario.output(&["--json", "list"], |_| {});
    assert!(init.status.success());
    let workspace_legacy_db = scenario.root().join(".latte/latte-code.db");
    std::fs::create_dir_all(workspace_legacy_db.parent().unwrap()).unwrap();
    std::fs::copy(scenario.database_path(), &workspace_legacy_db).unwrap();

    let listed = scenario.output(&["list"], |_| {});
    assert!(
        listed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&listed.stdout),
        String::from_utf8_lossy(&listed.stderr)
    );
    let stderr = String::from_utf8_lossy(&listed.stderr);
    assert!(
        stderr.contains("warning: skipping incompatible legacy workspace database"),
        "expected incompatible legacy DB warning on stderr: {stderr}"
    );

    // 3. Migrate a schema-8 global `state.db` whose `threads_v2` rows already carry a
    //    `workspace_root` (`legacy_count == 0`, `storage.rs:595..670`) from v8 -> v16
    //    strictly through `CARGO_BIN_EXE_latte-code --json list`.
    let seed_v8 = Scenario::new();
    let v8_provider =
        super::support::ScriptedProvider::start([super::support::ProviderReply::completion(
            "v8 seed answer",
        )]);
    seed_v8.write_config(v8_provider.endpoint(), r#"["/usr/bin/true"]"#);
    let seeded = seed_v8.output(&["--json", "run", "preserve v8 session"], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "k");
    });
    assert!(seeded.status.success());

    sqlite_execute(
        &seed_v8.database_path(),
        &format!(
            "{} \
             DROP INDEX IF EXISTS thread_sessions_workspace_activity; \
             DROP INDEX IF EXISTS thread_sessions_parent; \
             ALTER TABLE threads_v2 DROP COLUMN focus; \
             ALTER TABLE threads_v2 DROP COLUMN parent_thread_id; \
             DROP TABLE legacy_imports; \
             DROP TABLE workspaces; \
             DROP TABLE projects; \
             DROP TABLE runtime_lease; \
             DROP TABLE runtime_lease_epoch; \
             CREATE TABLE runtime_lease( \
               singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL, \
               fencing_token INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL \
             ); \
             DELETE FROM schema_migrations WHERE version >= 9; \
             PRAGMA user_version=8; \
             PRAGMA foreign_keys=ON;",
            super::support::REVERSE_TO_SCHEMA_12_SQL
        ),
    );
    // 3a. Outside-workspace v8 `state.db` with empty `workspace_root` fails closed (`storage.rs:626..631`).
    let outside_v8 = Scenario::new();
    std::fs::copy(seed_v8.database_path(), outside_v8.database_path()).unwrap();
    sqlite_execute(
        &outside_v8.database_path(),
        "UPDATE threads_v2 SET workspace_root='', title='';",
    );
    let outside_err = outside_v8.output(&["--json", "list"], |_| {});
    assert!(!outside_err.status.success());

    // 3b. In-workspace v8 `state.db` (`LATTE_CODE_HOME` inside workspace -> `open_in_workspace`)
    //     without `title`/`workspace_root` columns migrates both user-titled and untitled sessions
    //     (`storage.rs:597..665`).
    let ws_v8 = Scenario::new();
    let ws_v8_home = ws_v8.root().join(".latte/latte-code");
    std::fs::create_dir_all(&ws_v8_home).unwrap();
    let ws_v8_db = ws_v8_home.join("state.db");
    std::fs::copy(seed_v8.database_path(), &ws_v8_db).unwrap();
    {
        let conn = Connection::open(&ws_v8_db).unwrap();
        let (tid, bjson): (String, String) = conn
            .query_row(
                "SELECT thread_id, binding_json FROM threads_v2 LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let user_entry = serde_json::json!({
            "entry_id": "01900000-0000-7000-8000-0000000000e8",
            "turn_id": null,
            "sequence": 99,
            "kind": "user",
            "source_key": "v8-user-seed",
            "text": "migrated in-workspace v8 title",
            "payload": null,
            "created_at_ms": 1
        })
        .to_string();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF; \
             ALTER TABLE threads_v2 DROP COLUMN title; \
             ALTER TABLE threads_v2 DROP COLUMN workspace_root;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) \
             VALUES(?1,99,'01900000-0000-7000-8000-0000000000e8',NULL,'user','v8-user-seed',?2,1)",
            rusqlite::params![tid, user_entry],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads_v2(thread_id,revision,last_seq,lifecycle,binding_json,latest_run_id,created_at_ms,updated_at_ms) \
             VALUES('01900000-0000-7000-8000-0000000000f8',1,1,'ready',?1,NULL,2,2)",
            rusqlite::params![bjson],
        )
        .unwrap();
        let sys_entry = serde_json::json!({
            "entry_id": "01900000-0000-7000-8000-0000000000e9",
            "turn_id": null,
            "sequence": 1,
            "kind": "system",
            "source_key": "v8-sys-seed",
            "text": "non-user seed entry",
            "payload": null,
            "created_at_ms": 2
        })
        .to_string();
        conn.execute(
            "INSERT INTO conversation_outbox(thread_id,seq,entry_id,run_id,kind,source_key,entry_json,created_at_ms) \
             VALUES('01900000-0000-7000-8000-0000000000f8',1,'01900000-0000-7000-8000-0000000000e9',NULL,'system','v8-sys-seed',?1,2)",
            rusqlite::params![sys_entry],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    }
    let ws_v8_out = ws_v8.output(&["--json", "list"], |cmd| {
        cmd.env("LATTE_CODE_HOME", &ws_v8_home);
    });
    assert!(
        ws_v8_out.status.success(),
        "in-workspace v8 migration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&ws_v8_out.stdout),
        String::from_utf8_lossy(&ws_v8_out.stderr),
    );
    let ws_v8_json = String::from_utf8_lossy(&ws_v8_out.stdout);
    assert!(ws_v8_json.contains("migrated in-workspace v8 title"));
    assert_eq!(
        sqlite_text(
            &ws_v8_db,
            "SELECT title FROM sessions WHERE session_id='01900000-0000-7000-8000-0000000000f8'",
        ),
        "Untitled session"
    );

    let imported_v8 = seed_v8.output(&["--json", "list"], |_| {});
    assert!(
        imported_v8.status.success(),
        "v8 state.db migration failed: stdout={} stderr={}",
        String::from_utf8_lossy(&imported_v8.stdout),
        String::from_utf8_lossy(&imported_v8.stderr)
    );
    let imported_list = serde_json::to_string(&json(&imported_v8)["data"]["sessions"]).unwrap();
    assert!(
        imported_list.contains("preserve v8 session"),
        "expected preserved v8 session in: {imported_list}"
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_header_validation_git_staged_diff_and_failed_verification_stdout() {
    use super::support::{ProviderReply, ScriptedProvider};

    // 1. Invalid provider header configurations fail closed at startup with usage exit code 2.
    let header_scenario = Scenario::new();
    std::fs::create_dir_all(header_scenario.home().join(".latte")).unwrap();
    for bad_headers in [
        r#"{"bad name": "ok"}"#,
        r#"{"authorization": "Bearer override"}"#,
        r#"{"x-custom": "unterminated-${session_id"}"#,
        r#"{"x-custom": "unknown-${other_var}"}"#,
        "{\"x-custom\": \"ctrl\\u0007val\"}",
    ] {
        std::fs::write(
            header_scenario.home().join(".latte/latte-code.jsonc"),
            format!(
                r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:"http://127.0.0.1:1",api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},headers:{bad_headers}}}}}}}"#
            ),
        )
        .unwrap();
        let out = header_scenario.output(&["--json", "run", "check header"], |cmd| {
            cmd.env("TEST_OPENAI_KEY", "k");
        });
        assert!(!out.status.success(), "expected failure for {bad_headers}");
    }

    // 2. Git staged index diff (`UnifiedDiffIndexPaths`), readonly `git status -b --porcelain`,
    //    and verification failure with empty stderr + stdout diagnostics.
    let git_scenario = Scenario::new();
    git_scenario.init_git();
    std::fs::write(git_scenario.root().join("tracked.txt"), "v1\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(git_scenario.root())
            .env("HOME", git_scenario.home())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-m", "init", "--quiet"])
            .current_dir(git_scenario.root())
            .env("HOME", git_scenario.home())
            .status()
            .unwrap()
            .success()
    );
    // Stage a modification into the index (`git diff --cached` / `UnifiedDiffIndexPaths` path!).
    std::fs::write(git_scenario.root().join("tracked.txt"), "v2-staged\n").unwrap();
    std::fs::write(
        git_scenario.root().join("Cargo.toml"),
        "[package]\nname = \"demo-manifest\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "tracked.txt", "Cargo.toml"])
            .current_dir(git_scenario.root())
            .env("HOME", git_scenario.home())
            .status()
            .unwrap()
            .success()
    );
    // Populate workspace symlinks (`CurDir`, safe `ParentDir`, escaping `ParentDir`, and `RootDir`),
    // `Cargo.toml` manifest, and search fixtures (`.env` denied, `.md` filtered by glob, >600B line,
    // and `max_results` truncation) so `workspace_manifest`, `search`, and `read_project_manifest`
    // exercise their full boundary matrix through `CARGO_BIN_EXE_latte-code`.
    std::fs::create_dir_all(git_scenario.root().join("sub")).unwrap();
    std::os::unix::fs::symlink(
        "sub/./../tracked.txt",
        git_scenario.root().join("link_cur.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "../tracked.txt",
        git_scenario.root().join("sub/link_parent_ok.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "../../escape_outside",
        git_scenario.root().join("sub/link_parent_escape.txt"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "/tmp/latte_nonexistent_abs_target",
        git_scenario.root().join("link_abs.txt"),
    )
    .unwrap();
    std::fs::write(git_scenario.root().join(".env"), "needle-hit-secret\n").unwrap();
    std::fs::write(git_scenario.root().join("ignored.md"), "needle-hit-md\n").unwrap();
    std::fs::write(
        git_scenario.root().join("notes.txt"),
        format!("needle-hit-{}\nneedle-hit-second\n", "x".repeat(650)),
    )
    .unwrap();

    let provider = ScriptedProvider::start([
        ProviderReply::tool_calls([
            (
                "g0a",
                "search",
                &serde_json::json!({"query": "needle-hit", "glob": "*.txt", "max_results": 1}),
            ),
            (
                "g0b",
                "read_project_manifest",
                &serde_json::json!({"max_output": 16}),
            ),
            (
                "g0c",
                "process",
                &serde_json::json!({"shell": "trap '' TERM; sleep 5", "timeout_ms": 60, "grace_ms": 40}),
            ),
            (
                "g0d",
                "process",
                &serde_json::json!({"shell": "(trap '' TERM; sleep 5) </dev/null >/dev/null 2>&1 & exit 0", "timeout_ms": 5000, "grace_ms": 20}),
            ),
            (
                "g0e",
                "edit_file",
                &serde_json::json!({"path": "tracked.txt", "before": "v2-staged\n", "after": "unread\n"}),
            ),
            (
                "g0f",
                "read_file",
                &serde_json::json!({"path": "tracked.txt"}),
            ),
            (
                "g0g",
                "edit_file",
                &serde_json::json!({"path": "tracked.txt", "before": "v2-staged\n", "after": "v3-edited\n"}),
            ),
            (
                "g0h",
                "write_file",
                &serde_json::json!({"path": "link_cur.txt", "content": "x", "create_intent": true}),
            ),
            (
                "g0i",
                "write_file",
                &serde_json::json!({"path": "sub", "content": "x", "create_intent": true}),
            ),
            (
                "g1",
                "process",
                &serde_json::json!({"argv": ["git", "status", "-b", "--porcelain"]}),
            ),
            ("g2", "git_diff", &serde_json::json!({})),
            (
                "g2b",
                "git_diff",
                &serde_json::json!({"path": "Cargo.toml"}),
            ),
            (
                "g3",
                "write_file",
                &serde_json::json!({"path": "mutated.txt", "content": "needs-verify\n", "create_intent": true}),
            ),
        ]),
        ProviderReply::completion("attempted completion with failing stdout-only verifier"),
    ]);
    let ep = provider.endpoint();
    std::fs::create_dir_all(git_scenario.home().join(".latte")).unwrap();
    std::fs::write(
        git_scenario.home().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{
                version: 1,
                default_model: "main/mock",
                providers: {{
                    main: {{
                        type: "openai-chat",
                        models: ["mock"],
                        endpoint: {ep:?},
                        api_key: {{ source: "env", name: "TEST_OPENAI_KEY" }}
                    }}
                }},
                verification: {{ argv: ["/bin/sh", "-c", "printf 'verify-stdout-detail'; exit 3"] }}
            }}"#
        ),
    )
    .unwrap();

    let run = git_scenario.output(&["--json", "run", "--auto", "inspect staged diff"], |cmd| {
        cmd.env("TEST_OPENAI_KEY", "k");
    });
    assert_eq!(run.status.code(), Some(1));
    let req2 = serde_json::to_string(&provider.requests()[1].body).unwrap();
    assert!(
        req2.contains("v2-staged"),
        "expected staged index diff in git_diff tool result: {req2}"
    );
    let entries =
        serde_json::to_string(&json(&run)["data"]["session"]["transcript"]["entries"]).unwrap();
    assert!(
        entries.contains("verify-stdout-detail"),
        "expected stdout fallback detail on failed verification: {entries}"
    );
    provider.assert_consumed();

    // Strip `payload.effect_id` from `tool_result` JSONL entries and re-open via `--json list`
    // so startup reconciliation exercises `find_missing_observed_effects` Pass 2 (`lib.rs:1084..1114`)!
    {
        let jsonl_path = git_scenario.session_files().remove(0);
        let rewritten = std::fs::read_to_string(&jsonl_path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
                if v["kind"] == "tool_result"
                    && let Some(obj) = v["payload"].as_object_mut()
                {
                    obj.remove("effect_id");
                }
                serde_json::to_string(&v).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&jsonl_path, rewritten).unwrap();
    }
    assert!(
        git_scenario
            .output(&["--json", "list"], |_| {})
            .status
            .success()
    );

    // 3. Human-readable `latte-code serve --port 0` startup banner + graceful SIGINT shutdown.
    let mut text_srv = git_scenario
        .command(&["serve", "--port", "0"])
        .env("TEST_OPENAI_KEY", "k")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let srv_pid = i32::try_from(text_srv.id()).unwrap();
    let mut banner = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(text_srv.stdout.take().unwrap()),
        &mut banner,
    )
    .unwrap();
    assert!(
        banner.contains("latte-code server listening on http://"),
        "unexpected human serve banner: {banner}"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(srv_pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let _ = text_srv.wait().unwrap();

    // 4. Unborn Git repo `UnifiedDiffIndexPaths` (`process.rs:972..981`), cancelling + reconciling
    //    a started `process` (`storage.rs:4548..4560`), lease-loss recovery during a started
    //    `process` (`session.rs:4130..4140`), and cancelling a started readonly `search` tool
    //    (`session.rs:5336..5366`) — all via `latte-code --json serve --port 0`!
    let eff_scenario = Scenario::new();
    eff_scenario.init_git();
    std::fs::write(eff_scenario.root().join("unborn.txt"), "staged-v1\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "unborn.txt"])
            .current_dir(eff_scenario.root())
            .env("HOME", eff_scenario.home())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(eff_scenario.root().join("unborn.txt"), "worktree-v2\n").unwrap();

    let eff_provider = ScriptedProvider::start([
        // (a) Unborn `git_diff` (`{}`) -> `IndexPathsZ` + `UnifiedDiffIndexPaths` completes cleanly.
        ProviderReply::tool_call("u_diff_1", "git_diff", &serde_json::json!({})),
        ProviderReply::completion("unborn diff done"),
        // (b) Mutating `process` cancelled mid-execution -> `reconciliation_required` -> `POST /reconcile`.
        ProviderReply::tool_call(
            "u_proc_cancel",
            "process",
            &serde_json::json!({"argv": ["/bin/sleep", "3"]}),
        ),
        // (c) Started `process` loses lease under `LATTE_LEASE_TTL_MS=2400` -> `session.rs:4130..4140`.
        ProviderReply::tool_call(
            "u_proc_lease",
            "process",
            &serde_json::json!({"argv": ["/bin/sleep", "6"]}),
        ),
        // (d) Readonly `search` cancelled mid-execution -> `mark_cancelled_started_effect_unknown` readonly branch.
        ProviderReply::tool_call(
            "u_search_cancel",
            "search",
            &serde_json::json!({
                "query": "^(?:[a-km-z]{2,5}[0-9]{2,4}){3}[x-z]$",
                "regex": true
            }),
        ),
    ]);
    eff_scenario.write_config(eff_provider.endpoint(), r#"["/usr/bin/true"]"#);

    let mut eff_srv = eff_scenario
        .command(&["--json", "serve", "--port", "0"])
        .env("TEST_OPENAI_KEY", "k")
        .env("LATTE_LEASE_TTL_MS", "2400")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let eff_srv_pid = i32::try_from(eff_srv.id()).unwrap();
    let mut ready_line = String::new();
    let mut reader = std::io::BufReader::new(eff_srv.stdout.take().unwrap());
    let ready_json = loop {
        ready_line.clear();
        std::io::BufRead::read_line(&mut reader, &mut ready_line).unwrap();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(ready_line.trim())
            && v["status"] == "listening"
        {
            break v;
        }
    };
    let port = u16::try_from(ready_json["data"]["port"].as_u64().unwrap()).unwrap();
    let tok = std::fs::read_to_string(ready_json["data"]["token_path"].as_str().unwrap())
        .unwrap()
        .trim()
        .to_owned();
    let url = format!("http://127.0.0.1:{port}");

    let send_http = |method: &str,
                     path: &str,
                     body: Option<serde_json::Value>,
                     idem: Option<&str>|
     -> (u16, serde_json::Value) {
        use std::fmt::Write as _;
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let bytes = body.map(|b| serde_json::to_vec(&b).unwrap());
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {tok}\r\nConnection: close\r\n"
        );
        if let Some(k) = idem {
            let _ = write!(req, "Idempotency-Key: {k}\r\n");
        }
        if let Some(b) = &bytes {
            let _ = write!(
                req,
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            );
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).unwrap();
        if let Some(b) = &bytes {
            s.write_all(b).unwrap();
        }
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let hdr = String::from_utf8_lossy(&raw[..split]);
        let code = hdr
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let val = serde_json::from_slice(&raw[split + 4..]).unwrap_or(serde_json::Value::Null);
        (code, val)
    };

    // (a) Unborn `git_diff` (`{}`) via CLI `--server`.
    let unborn_out = eff_scenario.output(
        &[
            "--json",
            "run",
            "--auto",
            "--server",
            &url,
            "--token",
            &tok,
            "check unborn diff",
        ],
        |cmd| {
            cmd.env("TEST_OPENAI_KEY", "k");
        },
    );
    assert!(
        unborn_out.status.success(),
        "unborn_out failed: status={:?} stdout={} stderr={}",
        unborn_out.status,
        String::from_utf8_lossy(&unborn_out.stdout),
        String::from_utf8_lossy(&unborn_out.stderr),
    );
    let unborn_req1 = serde_json::to_string(&eff_provider.requests()[1].body).unwrap();
    assert!(
        unborn_req1.contains("worktree-v2"),
        "expected unborn index-vs-worktree diff in tool result: {unborn_req1}"
    );
    let binding_val = json(&unborn_out)["data"]["session"]["binding"].clone();
    let (_, ws_val) = send_http(
        "POST",
        "/v1/workspaces",
        Some(serde_json::json!({"path": eff_scenario.root().to_string_lossy()})),
        None,
    );
    let ws_id = ws_val["workspace_id"].as_str().unwrap().to_owned();
    let eff_db = eff_scenario.database_path();

    let wait_started_effect = |sid: &str| -> (String, u64, u64) {
        for _ in 0..400 {
            let conn = Connection::open(&eff_db).unwrap();
            let _ = conn.busy_timeout(Duration::from_secs(5));
            let found: Result<String, _> = conn.query_row(
                "SELECT e.effect_id FROM effects e JOIN session_turns st ON st.turn_id=e.turn_id WHERE st.session_id=?1 AND e.status='started' LIMIT 1",
                [sid],
                |r| r.get(0),
            );
            if let Ok(eid) = found {
                let (_, snap) = send_http("GET", &format!("/v1/sessions/{sid}"), None, None);
                let srev = snap["snapshot"]["revision"].as_u64().unwrap();
                let trev = snap["snapshot"]["turns"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["turn_revision"]
                    .as_u64()
                    .unwrap();
                return (eid, srev, trev);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("effect never entered started for {sid}");
    };

    // (b) Cancel started `process` -> `reconciliation_required` -> `POST /reconcile` (`storage.rs:4548..4560`).
    let sid_pr = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7()).to_string();
    let cid_pr = latte_core::SessionCommandId::from_uuid(uuid::Uuid::now_v7()).to_string();
    let (st_pr, _) = send_http(
        "POST",
        &format!("/v1/workspaces/{ws_id}/sessions"),
        Some(serde_json::json!({
            "session_id": sid_pr,
            "command_id": cid_pr,
            "prompt": "cancel process then reconcile",
            "binding": binding_val,
            "permission_mode": "full_auto",
        })),
        Some(&cid_pr),
    );
    assert_eq!(st_pr, 202);
    let (eid_pr, srev_pr, trev_pr) = wait_started_effect(&sid_pr);
    let _ = send_http(
        "POST",
        &format!("/v1/sessions/{sid_pr}/cancel"),
        Some(serde_json::json!({
            "expected_session_revision": srev_pr,
            "expected_turn_revision": trev_pr,
        })),
        None,
    );
    assert!(super::support::wait_until(Duration::from_secs(5), || {
        send_http("GET", &format!("/v1/sessions/{sid_pr}"), None, None).1["snapshot"]["lifecycle"]
            .as_str()
            == Some("reconciliation_required")
    }));
    let (rec_eff_st, _) = send_http(
        "POST",
        &format!("/v1/sessions/{sid_pr}/effects/{eid_pr}/reconcile"),
        Some(serde_json::json!({})),
        None,
    );
    assert_eq!(rec_eff_st, 200);

    // (c) Delete lease during started `process` -> `session.rs:4130..4140`.
    let sid_ll = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7()).to_string();
    let cid_ll = latte_core::SessionCommandId::from_uuid(uuid::Uuid::now_v7()).to_string();
    let (st_ll, _) = send_http(
        "POST",
        &format!("/v1/workspaces/{ws_id}/sessions"),
        Some(serde_json::json!({
            "session_id": sid_ll,
            "command_id": cid_ll,
            "prompt": "lose lease during process",
            "binding": binding_val,
            "permission_mode": "full_auto",
        })),
        Some(&cid_ll),
    );
    assert_eq!(st_ll, 202);
    let _ = wait_started_effect(&sid_ll);
    {
        let conn = Connection::open(&eff_db).unwrap();
        let _ = conn.busy_timeout(Duration::from_secs(5));
        conn.execute(
            "DELETE FROM runtime_lease WHERE scope=?1",
            [format!("session:{sid_ll}")],
        )
        .unwrap();
    }
    assert!(super::support::wait_until(Duration::from_secs(5), || {
        send_http("GET", &format!("/v1/sessions/{sid_ll}"), None, None).1["snapshot"]["lifecycle"]
            .as_str()
            != Some("running")
    }));

    // (d) Cancel started readonly `search` -> `session.rs:5336..5366`.
    let hay_chunk = "abcd12efgh34ijkl56mnop78q\n".repeat(60_000);
    for idx in 0..2 {
        std::fs::write(
            eff_scenario.root().join(format!("haystack_{idx:02}.txt")),
            &hay_chunk,
        )
        .unwrap();
    }
    let sid_ro = SessionId::from_uuid(SystemIdSource::default().next_uuid_v7()).to_string();
    let cid_ro = latte_core::SessionCommandId::from_uuid(uuid::Uuid::now_v7()).to_string();
    let (st_ro, _) = send_http(
        "POST",
        &format!("/v1/workspaces/{ws_id}/sessions"),
        Some(serde_json::json!({
            "session_id": sid_ro,
            "command_id": cid_ro,
            "prompt": "cancel readonly search",
            "binding": binding_val,
            "permission_mode": "full_auto",
        })),
        Some(&cid_ro),
    );
    assert_eq!(st_ro, 202);
    let (_, srev_ro, trev_ro) = wait_started_effect(&sid_ro);
    let (ro_can_st, _) = send_http(
        "POST",
        &format!("/v1/sessions/{sid_ro}/cancel"),
        Some(serde_json::json!({
            "expected_session_revision": srev_ro,
            "expected_turn_revision": trev_ro,
        })),
        None,
    );
    assert_eq!(ro_can_st, 200);
    assert!(super::support::wait_until(Duration::from_secs(10), || {
        send_http("GET", &format!("/v1/sessions/{sid_ro}"), None, None).1["snapshot"]["lifecycle"]
            .as_str()
            != Some("running")
    }));
    let (_, ro_final) = send_http("GET", &format!("/v1/sessions/{sid_ro}"), None, None);
    assert!(
        ro_final.to_string().contains("cancelled"),
        "expected cancelled readonly tool observation: {ro_final}"
    );
    eff_provider.assert_consumed();

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(eff_srv_pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let _ = eff_srv.wait().unwrap();
}
