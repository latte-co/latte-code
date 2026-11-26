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
    assert!(
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(git_scenario.root())
            .env("HOME", git_scenario.home())
            .status()
            .unwrap()
            .success()
    );

    let provider = ScriptedProvider::start([
        ProviderReply::tool_calls([
            (
                "g1",
                "process",
                &serde_json::json!({"argv": ["git", "status", "-b", "--porcelain"]}),
            ),
            ("g2", "git_diff", &serde_json::json!({})),
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
}
