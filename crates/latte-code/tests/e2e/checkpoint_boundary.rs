use super::support::{ProviderReply, Scenario, ScriptedProvider};

/// A supervised `latte-code serve` child bound to an ephemeral port. Dropping
/// it terminates the process group so no server survives the test.
#[cfg(unix)]
struct ServeChild {
    child: std::process::Child,
    port: u16,
    token: String,
    process_group: i32,
}

#[cfg(unix)]
impl ServeChild {
    fn start(scenario: &Scenario) -> Self {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let mut command = scenario.command(&["--json", "serve", "--port", "0"]);
        command
            .env("TEST_OPENAI_KEY", "e2e-server-secret")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let process_group = i32::try_from(child.id()).unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ready = loop {
            line.clear();
            let read = reader.read_line(&mut line).unwrap();
            if read > 0
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim())
                && value["status"] == "listening"
            {
                break value;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "server did not report readiness; last line: {line:?}"
            );
        };
        let port = u16::try_from(ready["data"]["port"].as_u64().unwrap()).unwrap();
        let token_path = ready["data"]["token_path"].as_str().unwrap();
        let token = std::fs::read_to_string(token_path).unwrap();
        assert!(!token.is_empty(), "server token must not be empty");

        Self {
            child,
            port,
            token,
            process_group,
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<&serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> (u16, serde_json::Value) {
        use std::fmt::Write as _;
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let payload = body.map(|value| serde_json::to_vec(value).unwrap());
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        if let Some(token) = auth {
            let _ = write!(request, "Authorization: Bearer {token}\r\n");
        }
        for (name, value) in extra_headers {
            let _ = write!(request, "{name}: {value}\r\n");
        }
        if let Some(payload) = &payload {
            request.push_str("Content-Type: application/json\r\n");
            let _ = write!(request, "Content-Length: {}\r\n", payload.len());
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        if let Some(payload) = &payload {
            stream.write_all(payload).unwrap();
        }
        stream.flush().unwrap();

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP response must have a header terminator");
        let header_text = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status = header_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .expect("status line");
        let body_bytes = &raw[split + 4..];
        let value = if body_bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(body_bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, value)
    }

    fn create_session(
        &self,
        workspace_id: &str,
        prompt: &str,
        binding: &serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let (body, command_id) = create_request(prompt, binding);
        self.request(
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(&self.token),
            Some(&body),
            &[("Idempotency-Key", &command_id)],
        )
    }
}

#[cfg(unix)]
impl Drop for ServeChild {
    fn drop(&mut self) {
        let group = nix::unistd::Pid::from_raw(self.process_group);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGTERM);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                _ => break,
            }
        }
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server_binding(scenario: &Scenario) -> serde_json::Value {
    let (_config, registry) =
        latte_code::AppConfig::load(scenario.root()).expect("config loads for binding");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .build()
        .expect("engine builds for binding");
    let tools = engine.tool_descriptors();
    let binding = registry
        .thread_binding_for_default(&tools)
        .expect("default binding resolves");
    serde_json::to_value(binding).expect("binding serializes")
}

fn create_request(prompt: &str, binding: &serde_json::Value) -> (serde_json::Value, String) {
    let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7()).to_string();
    let command_id = latte_core::ThreadCommandId::from_uuid(uuid::Uuid::now_v7()).to_string();
    let body = serde_json::json!({
        "thread_id": thread_id,
        "command_id": command_id,
        "prompt": prompt,
        "binding": binding,
    });
    (body, command_id)
}

#[cfg(unix)]
fn wait_for_lifecycle(
    server: &ServeChild,
    session_id: &str,
    expected: &[&str],
) -> serde_json::Value {
    for _ in 0..300 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 {
            let lifecycle = body["snapshot"]["lifecycle"].as_str().unwrap_or("");
            if expected.contains(&lifecycle) {
                return body["snapshot"].clone();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "session {session_id} never reached {:?}; last: {:?}",
        expected,
        server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        )
    );
}

/// Directly overwrites the canonical thread-effect descriptor via SQL.
#[cfg(unix)]
fn corrupt_canonical_descriptor(scenario: &Scenario, effect_id: &str, descriptor_json: &str) {
    use rusqlite::Connection;
    let conn = Connection::open(scenario.database_path()).unwrap();
    conn.execute(
        "UPDATE thread_effect_canonical_v2 SET descriptor_json=?1 WHERE effect_id=?2",
        rusqlite::params![descriptor_json, effect_id],
    )
    .unwrap();
}

/// Reads the canonical thread-effect descriptor ID for a run via SQL.
#[cfg(unix)]
fn read_effect_id_for_run(scenario: &Scenario, run_id: &str) -> String {
    use rusqlite::Connection;
    let conn = Connection::open(scenario.database_path()).unwrap();
    conn.query_row(
        "SELECT effect_id FROM thread_effect_canonical_v2 WHERE run_id=?1",
        [run_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// v2 migration: the v1 test corrupted the full runtime checkpoint (messages,
/// `tool_queue`, `pending`, etc.) and drove resume through CLI `resume --allow`.
/// v2 stores a compact `thread_effect` checkpoint in `runtime_checkpoints` and
/// a canonical descriptor in `thread_effect_canonical_v2`; the engine validates
/// the canonical descriptor during permission resolution.  This test starts a
/// `ServeChild`, creates a session that parks at `waiting_permission`, corrupts
/// the durable canonical descriptor through direct SQL, and then resolves the
/// permission over HTTP — the engine must reject the corrupt descriptor before
/// executing the effect.  A validation failure does not terminalize the run,
/// so the same session is reused for every corruption case.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn public_checkpoint_corruption_matrix_fails_closed_in_fresh_final_cli_processes() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "checkpoint-write",
        "write_file",
        &serde_json::json!({
            "path": "must-not-be-written.txt",
            "content": "checkpoint boundary\n",
            "create_intent": true
        }),
    )]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let server = ServeChild::start(&scenario);

    // Create/resolve the workspace.
    let root = scenario.root().to_string_lossy().into_owned();
    let (_, ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": root })),
        &[],
    );
    let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
    let binding = server_binding(&scenario);

    // Create the session; it parks at waiting_permission.
    let (create_status, create_body) = server.create_session(
        &workspace_id,
        "create a publicly persisted permission checkpoint",
        &binding,
    );
    assert_eq!(create_status, 202, "create: {create_body:?}");
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    let snapshot = wait_for_lifecycle(&server, &session_id, &["waiting_permission"]);
    let run_id_str = snapshot["active_run_id"].as_str().unwrap().to_string();
    let thread_revision = snapshot["revision"].as_u64().unwrap();
    let request_id = snapshot["pending"]["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run_revision = snapshot["pending"]["expected_run_revision"]
        .as_u64()
        .unwrap();

    let effect_id = read_effect_id_for_run(&scenario, &run_id_str);
    assert!(
        effect_id.contains("checkpoint-write"),
        "effect_id should reference the tool call, got: {effect_id}"
    );

    // Corruption cases: each mutates the canonical descriptor and must cause
    // the engine to fail closed without writing the file.
    let cases: Vec<(&str, String)> = vec![
        ("invalid json descriptor", "{".to_string()),
        ("null descriptor", serde_json::Value::Null.to_string()),
        (
            "wrong tool name",
            serde_json::json!({
                "effect_id": effect_id,
                "tool_call_id": "checkpoint-write",
                "name": "read_file",
                "input": {"path": "must-not-be-written.txt"},
                "attempt": 1
            })
            .to_string(),
        ),
        (
            "wrong tool call id",
            serde_json::json!({
                "effect_id": effect_id,
                "tool_call_id": "wrong-call-id",
                "name": "write_file",
                "input": {
                    "path": "must-not-be-written.txt",
                    "content": "checkpoint boundary\n",
                    "create_intent": true
                },
                "attempt": 1
            })
            .to_string(),
        ),
        (
            "wrong input path",
            serde_json::json!({
                "effect_id": effect_id,
                "tool_call_id": "checkpoint-write",
                "name": "write_file",
                "input": {
                    "path": "different-file.txt",
                    "content": "checkpoint boundary\n",
                    "create_intent": true
                },
                "attempt": 1
            })
            .to_string(),
        ),
        (
            "missing name field",
            serde_json::json!({
                "effect_id": effect_id,
                "tool_call_id": "checkpoint-write",
                "input": {"path": "must-not-be-written.txt"},
                "attempt": 1
            })
            .to_string(),
        ),
    ];

    for (name, descriptor) in cases {
        corrupt_canonical_descriptor(&scenario, &effect_id, &descriptor);

        // Resolve the permission over HTTP; the engine must reject the
        // corrupt descriptor before executing the effect.
        let (resolve_status, _resolve_body) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            Some(&server.token),
            Some(&serde_json::json!({
                "allow": true,
                "expected_thread_revision": thread_revision,
                "expected_run_revision": run_revision
            })),
            &[],
        );
        // The engine must fail closed: either the HTTP request is rejected
        // (status >= 400) or the session reaches a terminal state without
        // writing the file.
        if resolve_status < 400 {
            let terminal = wait_for_lifecycle(
                &server,
                &session_id,
                &["failed", "interrupted", "reconciliation_required", "ready"],
            );
            let lifecycle = terminal["lifecycle"].as_str().unwrap_or("");
            assert!(
                lifecycle != "ready" || !scenario.root().join("must-not-be-written.txt").exists(),
                "case {name}: file was written despite corrupt descriptor (lifecycle={lifecycle})"
            );
        }
        assert!(
            !scenario.root().join("must-not-be-written.txt").exists(),
            "case {name}: file was written despite corrupt descriptor"
        );
    }

    // Deny the permission: the session must be terminal and the file absent.
    // Fetch the current snapshot because the corruption cases may have
    // advanced the session revision.
    let (_, current_body) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    let current_snapshot = &current_body["snapshot"];
    let current_lifecycle = current_snapshot["lifecycle"].as_str().unwrap_or("");
    let current_revision = current_snapshot["revision"]
        .as_u64()
        .unwrap_or(thread_revision);
    let current_run_revision = current_snapshot["runs"][0]["run_revision"]
        .as_u64()
        .unwrap_or(run_revision);

    if current_lifecycle == "waiting_permission" {
        let (deny_status, deny_body) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            Some(&server.token),
            Some(&serde_json::json!({
                "allow": false,
                "expected_thread_revision": current_revision,
                "expected_run_revision": current_run_revision
            })),
            &[],
        );
        assert_eq!(deny_status, 200, "deny: {deny_body:?}");
        let _snapshot = wait_for_lifecycle(
            &server,
            &session_id,
            &["failed", "interrupted", "reconciliation_required"],
        );
    }
    assert!(!scenario.root().join("must-not-be-written.txt").exists());
}
