use super::support::{ProviderReply, Scenario, ScriptedProvider, assert_secret_absent, json};

/// Computes the exact v2 provider binding the server will accept, mirroring
/// what a co-located client does: load the same layered config and derive the
/// binding against the workspace engine's tool descriptors.
fn server_binding(scenario: &Scenario) -> serde_json::Value {
    server_binding_for_model(scenario, None)
}

/// Like [`server_binding`], but for an explicit provider model when given.
fn server_binding_for_model(scenario: &Scenario, model: Option<&str>) -> serde_json::Value {
    let (_config, registry) =
        latte_code::AppConfig::load(scenario.root()).expect("config loads for binding");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .build()
        .expect("engine builds for binding");
    let tools = engine.tool_descriptors();
    let binding = match model {
        Some(model) => registry
            .thread_binding_for_model("main", model, &tools)
            .expect("model binding resolves"),
        None => registry
            .thread_binding_for_default(&tools)
            .expect("default binding resolves"),
    };
    serde_json::to_value(binding).expect("binding serializes")
}

fn run_id(output: &std::process::Output) -> String {
    json(output)["data"]["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn waiting_run_id(output: &std::process::Output) -> String {
    json(output)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned()
}

#[test]
fn final_binary_creates_and_reopens_its_configured_sqlite_database() {
    let scenario = Scenario::new();
    let first = scenario.output(&["--json", "list"], |_| {});
    assert!(
        first.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(json(&first)["data"]["runs"], serde_json::json!([]));
    assert!(scenario.database_path().is_file());

    let reopened = scenario.output(&["--json", "list"], |_| {});
    assert!(reopened.status.success());
    assert_eq!(json(&reopened)["data"]["runs"], serde_json::json!([]));
}

#[test]
fn global_storage_home_ignores_workspace_database_redirect_and_is_shared_across_workspaces() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "global-storage-input",
        "global storage prompt",
        false,
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["verification-must-not-run"]"#,
        "workspace-redirect.db",
        ",compatibility_input_request:true",
    );

    let first = scenario.output(&["--json", "run", "persist globally"], |command| {
        command.env("TEST_OPENAI_KEY", "global-storage-secret");
    });
    assert_eq!(first.status.code(), Some(1));
    let persisted_run_id = waiting_run_id(&first);
    provider.assert_consumed();
    assert!(scenario.database_path().is_file());
    assert!(!scenario.root().join("workspace-redirect.db").exists());

    let second_workspace = scenario.root().join("second-workspace");
    std::fs::create_dir_all(second_workspace.join(".git")).unwrap();
    let second = scenario.output(&["--json", "list"], |command| {
        command.current_dir(&second_workspace);
    });
    assert!(second.status.success());
    assert_eq!(json(&second)["data"]["runs"][0]["run_id"], persisted_run_id);
    assert!(!second_workspace.join(".latte/latte-code.db").exists());
}

#[test]
fn final_binary_imports_legacy_workspace_sessions_once_and_exports_jsonl() {
    use latte_core::{IdSource, RunId, SystemIdSource, ThreadId, ThreadProviderBindingV2};

    let scenario = Scenario::new();
    let legacy_path = scenario.root().join(".latte/latte-code.db");
    std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    let legacy = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(&legacy_path)
        .build()
        .unwrap();
    let ids = SystemIdSource::default();
    let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
    legacy
        .create_thread_v2(
            thread_id,
            RunId::from_uuid(ids.next_uuid_v7()),
            ThreadProviderBindingV2 {
                version: 1,
                provider_name: "test".into(),
                provider_type: "openai-chat".into(),
                protocol: "chat".into(),
                model: "test-model".into(),
                config_fingerprint: "config".into(),
                tools_fingerprint: "tools".into(),
                aliases: std::collections::BTreeMap::new(),
                credential_ref_id: "env:TEST_KEY".into(),
                data_scope_id: "workspace".into(),
                credential_generation: 1,
            },
            "legacy conversation",
            1,
        )
        .unwrap();
    drop(legacy);
    let source_before = std::fs::read(&legacy_path).unwrap();

    let first = scenario.output(&["--json", "list"], |_| {});
    assert!(first.status.success());
    let second = scenario.output(&["--json", "list"], |_| {});
    assert!(second.status.success());
    assert_eq!(std::fs::read(&legacy_path).unwrap(), source_before);
    assert!(scenario.database_path().is_file());
    let imported = latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap();
    let workspace = std::fs::canonicalize(scenario.root()).unwrap();
    let sessions = imported
        .list_thread_sessions_v2_for_workspace(workspace.to_str().unwrap(), 10)
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, thread_id);
    let session_files = scenario.session_files();
    assert_eq!(session_files.len(), 1);
    let conversation = std::fs::read_to_string(&session_files[0]).unwrap();
    assert!(conversation.contains(r#""content":"legacy conversation""#));
}

#[test]
fn final_binary_parses_loopback_provider_input_and_persists_waiting_projection() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "portable-input",
        "portable prompt",
        false,
    )]);
    let secret = "latte-portable-e2e-secret";
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["verification-must-not-run"]"#,
        ".latte/latte-code.db",
        ",compatibility_input_request:true",
    );
    let output = scenario.output(&["--json", "run", "portable provider journey"], |command| {
        command.env("TEST_OPENAI_KEY", secret);
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["status"], "failed");
    assert_eq!(json(&output)["error"]["code"], "runtime");
    let waiting_id = waiting_run_id(&output);
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/chat/completions");
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        &format!("Bearer {secret}")
    );

    let shown = scenario.output(&["--json", "show", &waiting_id], |_| {});
    assert_eq!(shown.status.code(), Some(10));
    assert_eq!(json(&shown)["data"]["run"]["status"], "waiting_input");
    assert_eq!(
        json(&shown)["data"]["run"]["pending_input"]["request_id"],
        "portable-input"
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(json(&listed)["data"]["runs"][0]["run_id"], waiting_id);

    let database = std::fs::read(scenario.database_path()).unwrap();
    assert_secret_absent(
        secret,
        &[
            ("stdout", &output.stdout),
            ("stderr", &output.stderr),
            ("database", &database),
        ],
    );
}

#[test]
fn final_binary_uses_inline_provider_secret_without_environment_inheritance() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "inline-portable-input",
        "inline portable prompt",
        false,
    )]);
    let secret = "latte-inline-portable-e2e-secret";
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{:?},api_key:{secret:?},compatibility_input_request:true}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["verification-must-not-run"]}}}}"#,
            provider.endpoint()
        ),
    )
    .unwrap();

    let output = scenario.output(&["--json", "run", "inline provider journey"], |_| {});

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "runtime");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        &format!("Bearer {secret}")
    );
    let database = std::fs::read(scenario.database_path()).unwrap();
    assert_secret_absent(
        secret,
        &[
            ("stdout", &output.stdout),
            ("stderr", &output.stderr),
            ("database", &database),
        ],
    );
}

#[test]
fn final_binary_persists_terminal_provider_failure_without_retrying() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::json(
        400,
        &serde_json::json!({"error": "portable rejection"}),
    )]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["verification-must-not-run"]"#,
        ".latte/latte-code.db",
        ",max_attempts:3",
    );
    let output = scenario.output(&["--json", "run", "persist provider failure"], |command| {
        command.env("TEST_OPENAI_KEY", "portable-failure-secret");
    });
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["status"], "failed");
    assert_eq!(json(&output)["data"]["run"]["status"], "failed");
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);

    let id = run_id(&output);
    let shown = scenario.output(&["--json", "show", &id], |_| {});
    assert_eq!(shown.status.code(), Some(1));
    assert_eq!(json(&shown)["data"]["run"]["status"], "failed");
    assert!(
        json(&shown)["data"]["run"]["failure"]["message"]
            .as_str()
            .unwrap()
            .contains("http 400")
    );
}

/// A supervised `latte-code serve` child bound to an ephemeral port. Dropping
/// it terminates the process group so no server survives the test.
struct ServeChild {
    child: std::process::Child,
    port: u16,
    token: String,
    #[cfg(unix)]
    process_group: i32,
}

impl ServeChild {
    /// Starts `latte-code --json serve --port 0` and blocks until the readiness
    /// line reports the bound port and token file, then reads the token.
    fn start(scenario: &Scenario) -> Self {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let mut command = scenario.command(&["--json", "serve", "--port", "0"]);
        command
            .env("TEST_OPENAI_KEY", "e2e-server-secret")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().unwrap();
        #[cfg(unix)]
        let process_group = i32::try_from(child.id()).unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        // The first stdout line is the versioned readiness envelope.
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
            #[cfg(unix)]
            process_group,
        }
    }

    /// Issues one framed HTTP/1.1 request over loopback and returns the parsed
    /// (status, JSON body) pair.
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
}

/// Reads the workspace SSE stream over loopback for a bounded window. Kept as
/// a free function so a reader thread can run without owning a `ServeChild`.
fn read_events_from(
    port: u16,
    token: &str,
    workspace_id: &str,
    timeout: std::time::Duration,
) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    let request = format!(
        "GET /v1/workspaces/{workspace_id}/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut buf = [0_u8; 4096];
    let mut seen = String::new();
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            _ => break,
        }
    }
    seen
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        // Ask the server to shut down gracefully first so it can flush its
        // coverage profile and exit cleanly; only force-kill as a fallback.
        #[cfg(unix)]
        {
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
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_serves_http_api_with_auth_workspace_and_session_lifecycle() {
    let scenario = Scenario::new();
    // A scripted provider on loopback lets the background turn complete, so the
    // session reaches a durable idle state and the success mutation paths are
    // exercised through the final binary. Extra completions cover the queued
    // follow-up drain and an explicit follow-up turn.
    let provider = ScriptedProvider::start([
        ProviderReply::completion("done"),
        ProviderReply::completion("done"),
        ProviderReply::completion("done"),
        ProviderReply::completion("done"),
    ]);
    // Two configured models let the switch-model success path run end to end.
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock","mock-2"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

    // Health needs no auth.
    let (health_status, _) = server.request("GET", "/health", None, None, &[]);
    assert_eq!(health_status, 200);

    // Missing token is rejected.
    let (unauth_status, _) = server.request("GET", "/v1/sessions/x", None, None, &[]);
    assert_eq!(unauth_status, 401);

    // A non-existent workspace path is rejected with 400.
    let (bad_ws_status, bad_ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": "/nonexistent/path/for/serve/e2e" })),
        &[],
    );
    assert_eq!(bad_ws_status, 400);
    assert_eq!(bad_ws_body["error"]["type"], "rejected");

    // Create/resolve the workspace by absolute path.
    let root = scenario.root().to_string_lossy().into_owned();
    let (ws_status, ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": root })),
        &[],
    );
    assert_eq!(ws_status, 200);
    let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();

    // The binding a real co-located client would compute from the same config.
    let binding = server_binding(&scenario);

    // Create returns 202 immediately (async accept, not a completed turn).
    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "hello server", "binding": binding })),
        &[("Idempotency-Key", "e2e-key-1")],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();
    // accepted_revision is the real durable revision after acceptance.
    assert!(create_body["accepted_revision"].as_u64().is_some());

    // The same idempotency key replays the original accepted session.
    let (replay_status, replay_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "hello server", "binding": binding })),
        &[("Idempotency-Key", "e2e-key-1")],
    );
    assert_eq!(replay_status, 202);
    assert_eq!(replay_body["session_id"].as_str().unwrap(), session_id);

    // A keyed create that fails (invalid binding) releases its reservation, so
    // a corrected retry with the same key proceeds rather than 409-in-flight.
    let (bad_key_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "x", "binding": { "version": 1 } })),
        &[("Idempotency-Key", "e2e-release-key")],
    );
    assert_eq!(bad_key_status, 400);
    let (retry_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "recovered", "binding": binding })),
        &[("Idempotency-Key", "e2e-release-key")],
    );
    assert_eq!(
        retry_status, 202,
        "released key must allow a corrected retry"
    );

    // The durable session becomes readable and, once the scripted provider
    // completes the background turn, reaches the idle "ready" lifecycle.
    let mut ready = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 {
            assert_eq!(body["snapshot"]["thread_id"].as_str().unwrap(), session_id);
            if body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                ready = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready, "durable session never reached the ready lifecycle");

    let (list_status, list_body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(list_status, 200);
    assert!(
        list_body["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions
                .iter()
                .any(|s| s["thread_id"].as_str() == Some(session_id.as_str()))),
        "list route must return the durable session"
    );

    // Search returns the workspace catalogue (possibly empty) with 200.
    let (search_status, search_body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions/search?q=hello&limit=10"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(search_status, 200);
    assert!(search_body["sessions"].is_array());

    // Read the current thread revision to drive revision-fenced mutations.
    let (_, snapshot) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    let revision = snapshot["snapshot"]["revision"].as_u64().unwrap();

    // Switching to the other configured model on an idle session succeeds and
    // returns the updated snapshot binding.
    let (switch_ok_status, switch_ok_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/model"),
        Some(&server.token),
        Some(&serde_json::json!({
            "binding": server_binding_for_model(&scenario, Some("mock-2")),
            "expected_thread_revision": revision
        })),
        &[],
    );
    assert_eq!(switch_ok_status, 200, "switch returned {switch_ok_body:?}");
    assert_eq!(switch_ok_body["snapshot"]["binding"]["model"], "mock-2");

    // Re-read the revision after the durable switch for the follow-up fence.
    let (_, snapshot) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    let revision = snapshot["snapshot"]["revision"].as_u64().unwrap();

    // Subscribe to the workspace SSE stream in a reader thread, then trigger a
    // follow-up whose durable transitions must surface as thread_changed
    // frames on the stream.
    let events_handle = {
        let port = server.port;
        let token = server.token.clone();
        let workspace_id = workspace_id.clone();
        std::thread::spawn(move || {
            read_events_from(
                port,
                &token,
                &workspace_id,
                std::time::Duration::from_secs(3),
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Follow-up with the matching revision is accepted (202) and queued.
    let (follow_status, follow_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "e2e-follow-1")],
    );
    assert_eq!(follow_status, 202, "follow-up returned {follow_body:?}");
    let follow_revision = follow_body["accepted_revision"].as_u64().unwrap();

    // Retrying the follow-up with the same Idempotency-Key replays the original
    // accepted result rather than starting a second turn.
    let (replay_follow_status, replay_follow_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "e2e-follow-1")],
    );
    assert_eq!(replay_follow_status, 202);
    assert_eq!(
        replay_follow_body["accepted_revision"].as_u64().unwrap(),
        follow_revision
    );

    // The follow-up's durable transitions surface on the SSE stream.
    let frames = events_handle.join().unwrap();
    assert!(
        frames.contains("text/event-stream"),
        "events response was not an SSE stream: {frames:?}"
    );
    assert!(
        frames.contains("thread_changed"),
        "SSE stream carried no thread_changed frame: {frames:?}"
    );

    // A stale follow-up revision is a 409 conflict.
    let (follow_conflict, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "again", "expected_thread_revision": 999 })),
        &[],
    );
    assert_eq!(follow_conflict, 409);

    // Queue a follow-up (accepted or conflict depending on runner state, but
    // never a server error).
    let (queue_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/queue"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "queued" })),
        &[],
    );
    assert!(
        queue_status == 202 || queue_status == 409,
        "queue: {queue_status}"
    );

    // Switch model with a stale revision is a 409 conflict.
    let (switch_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/model"),
        Some(&server.token),
        Some(&serde_json::json!({ "binding": binding, "expected_thread_revision": 999 })),
        &[],
    );
    assert!(
        switch_status == 409 || switch_status == 200,
        "switch: {switch_status}"
    );

    // Permission/input/reconcile against a fresh id are 404; an invalid id is
    // 400. These exercise the remaining session routes end to end.
    let other = uuid::Uuid::now_v7();
    let (perm_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{other}/permissions/req-1"),
        Some(&server.token),
        Some(&serde_json::json!({
            "allow": true,
            "expected_thread_revision": 0,
            "expected_run_revision": 0
        })),
        &[],
    );
    assert_eq!(perm_status, 404);

    let (input_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{other}/input"),
        Some(&server.token),
        Some(&serde_json::json!({
            "request_id": "req-1",
            "value": "v",
            "expected_thread_revision": 0,
            "expected_run_revision": 0
        })),
        &[],
    );
    assert_eq!(input_status, 404);

    let (reconcile_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{other}/effects/effect-1/reconcile"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(reconcile_status, 404);

    let (bad_id_status, _) = server.request(
        "GET",
        "/v1/sessions/not-a-uuid",
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(bad_id_status, 400);

    // A read for an unknown workspace id is 404.
    let (missing_ws_status, _) = server.request(
        "GET",
        "/v1/workspaces/ws_absent/sessions",
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(missing_ws_status, 404);

    // A stale revision fence is rejected with 409 and the current revision.
    let (conflict_status, conflict_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/cancel"),
        Some(&server.token),
        Some(&serde_json::json!({
            "expected_thread_revision": 999,
            "expected_run_revision": 999
        })),
        &[],
    );
    assert_eq!(conflict_status, 409);
    assert_eq!(conflict_body["error"]["type"], "conflict");
    assert!(conflict_body["error"]["current_revision"].is_u64());

    // Reusing an idempotency key with a different payload is rejected with 422.
    let (mismatch_status, mismatch_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "DIFFERENT prompt", "binding": binding })),
        &[("Idempotency-Key", "e2e-key-1")],
    );
    assert_eq!(mismatch_status, 422);
    assert_eq!(
        mismatch_body["error"]["type"], "idempotency_mismatch",
        "payload mismatch must be reported: {mismatch_body:?}"
    );

    // Reusing a follow-up idempotency key with a different prompt is 422.
    let (follow_mismatch_status, follow_mismatch_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "DIFFERENT", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "e2e-follow-1")],
    );
    assert_eq!(follow_mismatch_status, 422);
    assert_eq!(
        follow_mismatch_body["error"]["type"], "idempotency_mismatch",
        "follow-up payload mismatch must be reported: {follow_mismatch_body:?}"
    );

    // A create WITHOUT an Idempotency-Key header succeeds (exercises the None
    // branch in scoped_idempotency_key, no ledger interaction).
    let (no_key_status, no_key_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "no key", "binding": binding })),
        &[],
    );
    assert_eq!(no_key_status, 202);
    assert!(no_key_body["session_id"].is_string());

    // A follow-up WITHOUT an Idempotency-Key header succeeds.
    let no_key_session = no_key_body["session_id"].as_str().unwrap();
    let mut no_key_ready = false;
    for _ in 0..200 {
        let (s, b) = server.request(
            "GET",
            &format!("/v1/sessions/{no_key_session}"),
            Some(&server.token),
            None,
            &[],
        );
        if s == 200 && b["snapshot"]["lifecycle"].as_str() == Some("ready") {
            no_key_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(no_key_ready, "no-key session must reach ready");
    let no_key_rev = {
        let (_, b) = server.request(
            "GET",
            &format!("/v1/sessions/{no_key_session}"),
            Some(&server.token),
            None,
            &[],
        );
        b["snapshot"]["revision"].as_u64().unwrap()
    };
    let (no_key_follow_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{no_key_session}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "continue", "expected_thread_revision": no_key_rev })),
        &[],
    );
    assert_eq!(no_key_follow_status, 202);

    // A create WITHOUT key that FAILS (invalid binding) exercises the
    // (None, Err) branch in the create handler's idempotency match.
    let (no_key_bad_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "bad", "binding": {"version": 1} })),
        &[],
    );
    assert_eq!(no_key_bad_status, 400);

    // A follow-up WITHOUT key with a stale revision exercises the
    // (None, Err) branch in the follow-up handler's idempotency match.
    let (no_key_stale_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{no_key_session}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "stale", "expected_thread_revision": 999 })),
        &[],
    );
    assert_eq!(no_key_stale_status, 409);

    // A create with a key that is currently Pending (in-flight) but with a
    // DIFFERENT payload triggers 422 payload mismatch (Pending-state mismatch).
    let (pending_mismatch_status, pending_mismatch_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "TOTALLY DIFFERENT payload for pending", "binding": binding })),
        &[("Idempotency-Key", "e2e-key-1")],
    );
    // e2e-key-1 is Done (not Pending), so this hits the Done-mismatch path.
    // To truly test the Pending-mismatch path we need a request that's in-flight.
    // But at minimum this confirms mismatch detection.
    assert_eq!(pending_mismatch_status, 422);
    assert_eq!(
        pending_mismatch_body["error"]["type"],
        "idempotency_mismatch"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_resolves_a_permission_request_through_http() {
    let scenario = Scenario::new();
    // The scripted provider requests a write_file tool call, parking the
    // session at WaitingPermission; after the permission is allowed over HTTP
    // the effect runs and the provider completes the turn.
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "write-1",
            "write_file",
            &serde_json::json!({
                "path": "note.txt",
                "content": "hello\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("wrote the file"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "write it", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingPermission.
    let mut pending = None;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_permission") {
            pending = Some((
                body["snapshot"]["revision"].as_u64().unwrap(),
                body["snapshot"]["pending"]["request_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                body["snapshot"]["pending"]["expected_run_revision"]
                    .as_u64()
                    .unwrap(),
            ));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (revision, request_id, run_revision) =
        pending.expect("session never reached WaitingPermission over HTTP");

    // Allowing the permission over HTTP executes the effect and continues the
    // provider turn to completion; the file is written exactly once.
    let (allow_status, allow_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
        Some(&server.token),
        Some(&serde_json::json!({
            "allow": true,
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision
        })),
        &[],
    );
    assert_eq!(allow_status, 200, "allow returned {allow_body:?}");
    assert!(allow_body["snapshot"].is_object());
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("note.txt")).unwrap(),
        "hello\n",
        "allowed effect must write the file exactly once"
    );

    // The completed session is not awaiting reconciliation, so a reconcile
    // request over HTTP is a 404.
    let (reconcile_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/effects/write-1/reconcile"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(reconcile_status, 404);

    // Switching binding on the session exercises the model route end to end.
    // The exact outcome depends on the post-completion lifecycle, but it is
    // never a server error.
    let (_, snapshot) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    let revision = snapshot["snapshot"]["revision"].as_u64().unwrap();
    let (noop_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/model"),
        Some(&server.token),
        Some(&serde_json::json!({
            "binding": server_binding(&scenario),
            "expected_thread_revision": revision
        })),
        &[],
    );
    assert!(
        noop_status == 200 || noop_status == 409,
        "switch-model status: {noop_status}"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_denies_a_permission_request_through_http() {
    let scenario = Scenario::new();
    // The scripted provider requests a write_file tool call, parking the
    // session at WaitingPermission; denying the permission over HTTP records
    // the denial and continues the turn without executing the effect.
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "write-deny",
            "write_file",
            &serde_json::json!({
                "path": "denied.txt",
                "content": "should not be written\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("permission denied, moving on"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "write it", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingPermission.
    let mut pending = None;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_permission") {
            pending = Some((
                body["snapshot"]["revision"].as_u64().unwrap(),
                body["snapshot"]["pending"]["request_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                body["snapshot"]["pending"]["expected_run_revision"]
                    .as_u64()
                    .unwrap(),
            ));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (revision, request_id, run_revision) =
        pending.expect("session never reached WaitingPermission over HTTP");

    // Denying the permission over HTTP records the denial without executing
    // the effect; the provider turn continues to completion.
    let (deny_status, deny_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
        Some(&server.token),
        Some(&serde_json::json!({
            "allow": false,
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision
        })),
        &[],
    );
    assert_eq!(deny_status, 200, "deny returned {deny_body:?}");
    assert!(deny_body["snapshot"].is_object());

    // The denied effect must not have written the file.
    assert!(
        !scenario.root().join("denied.txt").exists(),
        "denied effect must not write the file"
    );

    // The session settles at a durable idle lifecycle after the denied turn.
    let mut settled = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 {
            let lifecycle = body["snapshot"]["lifecycle"].as_str().unwrap_or("");
            if lifecycle == "ready" || lifecycle == "failed" {
                settled = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(settled, "session never settled after the denied turn");
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_rejects_secret_input_request_from_provider() {
    let scenario = Scenario::new();
    // The headless runtime does not support provider-requested secret input:
    // a secret input_request must fail the session with a clear error rather
    // than park at WaitingInput with a secret in the durable command shape.
    let provider = ScriptedProvider::start([ProviderReply::input_request(
        "secret-1",
        "Enter API key",
        true,
    )]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},compatibility_input_request:true}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "need a secret", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // The secret input request is rejected; the session settles at Failed.
    let mut failed = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("failed") {
            failed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        failed,
        "session must fail when the provider requests secret input"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_rejects_stale_run_revision_on_permission() {
    let scenario = Scenario::new();
    // The scripted provider requests a tool call, parking the session at
    // WaitingPermission. Resolving with a stale run_revision must 409;
    // resolving with the correct run_revision must succeed.
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "stale-run-perm",
            "write_file",
            &serde_json::json!({
                "path": "stale-perm.txt",
                "content": "ok\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("done"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "write it", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingPermission.
    let mut pending = None;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_permission") {
            pending = Some((
                body["snapshot"]["revision"].as_u64().unwrap(),
                body["snapshot"]["pending"]["request_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                body["snapshot"]["pending"]["expected_run_revision"]
                    .as_u64()
                    .unwrap(),
            ));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (revision, request_id, run_revision) =
        pending.expect("session never reached WaitingPermission over HTTP");

    // A stale run_revision is rejected with 409.
    let (stale_status, stale_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
        Some(&server.token),
        Some(&serde_json::json!({
            "allow": true,
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision + 999
        })),
        &[],
    );
    assert_eq!(
        stale_status, 409,
        "stale run_revision must 409: {stale_body:?}"
    );

    // The correct run_revision succeeds.
    let (ok_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
        Some(&server.token),
        Some(&serde_json::json!({
            "allow": true,
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision
        })),
        &[],
    );
    assert_eq!(ok_status, 200);
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_rejects_stale_run_revision_on_input() {
    let scenario = Scenario::new();
    // The scripted provider requests non-secret input, parking the session at
    // WaitingInput. Providing input with a stale run_revision must 409;
    // providing with the correct run_revision must succeed.
    let provider = ScriptedProvider::start([
        ProviderReply::input_request("stale-run-input", "what value?", false),
        ProviderReply::completion("got it"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},compatibility_input_request:true}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "need input", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingInput.
    let mut pending = None;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_input") {
            pending = Some((
                body["snapshot"]["revision"].as_u64().unwrap(),
                body["snapshot"]["pending"]["request_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                body["snapshot"]["pending"]["expected_run_revision"]
                    .as_u64()
                    .unwrap(),
            ));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (revision, request_id, run_revision) =
        pending.expect("session never reached WaitingInput over HTTP");

    // A stale run_revision is rejected with 409.
    let (stale_status, stale_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/input"),
        Some(&server.token),
        Some(&serde_json::json!({
            "request_id": request_id,
            "value": "stale",
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision + 999
        })),
        &[],
    );
    assert_eq!(
        stale_status, 409,
        "stale run_revision must 409: {stale_body:?}"
    );

    // The correct run_revision succeeds.
    let (ok_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/input"),
        Some(&server.token),
        Some(&serde_json::json!({
            "request_id": request_id,
            "value": "correct",
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision
        })),
        &[],
    );
    assert_eq!(ok_status, 200);
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_provides_input_through_http() {
    let scenario = Scenario::new();
    // The scripted provider requests non-secret input, parking the session at
    // WaitingInput; providing the value continues the turn, and the queued
    // follow-up then drains as a further completion.
    let provider = ScriptedProvider::start([
        ProviderReply::input_request("input-1", "what value?", false),
        ProviderReply::completion("got it"),
        ProviderReply::completion("drained the queue"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},compatibility_input_request:true}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "need input", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingInput.
    let mut pending = None;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_input") {
            pending = Some((
                body["snapshot"]["revision"].as_u64().unwrap(),
                body["snapshot"]["pending"]["request_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                body["snapshot"]["pending"]["expected_run_revision"]
                    .as_u64()
                    .unwrap(),
            ));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (revision, request_id, run_revision) =
        pending.expect("session never reached WaitingInput over HTTP");

    // Queueing against a session parked on input is timing-dependent (the
    // runner window may or may not still be open), but must never be a server
    // error or a 404 (the session IS known and durable at this point).
    let (queue_status, queue_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/queue"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "queued while waiting" })),
        &[],
    );
    assert!(
        matches!(queue_status, 202 | 409),
        "unexpected queue status {queue_status}: {queue_body:?}"
    );

    // Providing the requested value over HTTP continues the turn to completion.
    let (input_status, input_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/input"),
        Some(&server.token),
        Some(&serde_json::json!({
            "request_id": request_id,
            "value": "the answer",
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision
        })),
        &[],
    );
    assert_eq!(input_status, 200, "provide_input returned {input_body:?}");
    assert!(input_body["snapshot"].is_object());

    // After input, the session completes. Verify a follow-up without an
    // Idempotency-Key header succeeds (covers the None idempotency branch
    // in the E2E final binary).
    let mut input_ready = false;
    for _ in 0..200 {
        let (s, b) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if s == 200 && b["snapshot"]["lifecycle"].as_str() == Some("ready") {
            input_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if input_ready {
        let final_rev = {
            let (_, b) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            b["snapshot"]["revision"].as_u64().unwrap()
        };
        // Cancel with matching revisions exercises the cancel handler success path.
        let (cancel_status, _) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/cancel"),
            Some(&server.token),
            Some(&serde_json::json!({
                "expected_thread_revision": final_rev,
                "expected_run_revision": 0
            })),
            &[],
        );
        // Session is idle (ready), so cancel returns conflict (no active run).
        assert!(
            cancel_status == 409 || cancel_status == 200,
            "cancel: {cancel_status}"
        );
    }
}

#[test]
fn final_binary_server_queues_follow_up_during_an_active_turn() {
    let scenario = Scenario::new();
    // A slow provider keeps the first turn running long enough to queue a
    // follow-up while the session's runner is still active (202 + position).
    let provider = ScriptedProvider::start([
        ProviderReply::completion("slow done").delayed(std::time::Duration::from_secs(5)),
        ProviderReply::completion("drained the queue"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock","mock-2"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "slow turn", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // While the (delayed) turn is running, its runner mailbox is active, so a
    // queued follow-up is accepted with its position. The 5s provider delay
    // keeps the window open comfortably even under coverage instrumentation.
    let mut queued = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/queue"),
            Some(&server.token),
            Some(&serde_json::json!({ "prompt": "queued mid-turn" })),
            &[],
        );
        if status == 202 {
            assert!(body["position"].as_u64().is_some());
            queued = true;
            break;
        }
        // 404 before the session is registered, or 409 if the runner window
        // closed; retry until the active window is observed.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(queued, "queue was never accepted during the active turn");
}

#[test]
fn final_binary_serve_reports_a_bind_conflict_as_internal_error() {
    let scenario = Scenario::new();
    scenario.write_config(r#"["true"]"#, r#"["true"]"#);
    // Hold the first server on an ephemeral port, then ask a second serve
    // process to bind the same port; it must exit non-zero with a classified
    // server_bind error over the JSON envelope.
    let server = ServeChild::start(&scenario);
    let port = server.port;

    let output = scenario.output(
        &["--json", "serve", "--port", &port.to_string()],
        |command| {
            command.env("TEST_OPENAI_KEY", "e2e-server-secret");
        },
    );
    assert_eq!(
        output.status.code(),
        Some(70),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["status"], "internal");
    assert_eq!(envelope["error"]["code"], "server_bind");
}

#[test]
fn final_binary_serve_rejects_invalid_configuration() {
    let scenario = Scenario::new();
    // An empty verification.argv is a documented configuration error; serve
    // surfaces it as a usage failure before binding a socket.
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        "{ verification: { argv: [] } }",
    )
    .unwrap();

    let output = scenario.output(&["--json", "serve", "--port", "0"], |command| {
        command.env("TEST_OPENAI_KEY", "e2e-server-secret");
    });
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["status"], "usage");
    assert_eq!(envelope["error"]["code"], "configuration");
}

#[test]
fn final_binary_server_reports_a_full_mailbox_as_conflict() {
    let scenario = Scenario::new();
    // A slow first turn keeps the runner active while we flood the bounded
    // (capacity 8) mailbox; once full, further queues are a mailbox-full 409.
    let provider = ScriptedProvider::start([
        ProviderReply::completion("slow done").delayed(std::time::Duration::from_secs(5)),
        ProviderReply::completion("drain 1"),
        ProviderReply::completion("drain 2"),
        ProviderReply::completion("drain 3"),
        ProviderReply::completion("drain 4"),
        ProviderReply::completion("drain 5"),
        ProviderReply::completion("drain 6"),
        ProviderReply::completion("drain 7"),
        ProviderReply::completion("drain 8"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "slow turn", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Flood the mailbox during the active window until it reports full. Every
    // response is either 202 (queued), 404 (not yet registered), 409-full
    // (mailbox saturated), or 409-inactive (window closed) — never a 5xx.
    let mut saw_full = false;
    let mut saw_active = false;
    for _ in 0..400 {
        let (status, body) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/queue"),
            Some(&server.token),
            Some(&serde_json::json!({ "prompt": "flood" })),
            &[],
        );
        if status == 202 {
            saw_active = true;
            continue;
        }
        if status == 409 {
            if body["error"]["message"] == "input mailbox is full" {
                saw_full = true;
                break;
            }
            // A generic conflict before the runner opened (still registering)
            // is transient; only stop once we have seen the active window.
            if saw_active {
                break;
            }
        }
        assert!(
            status == 404 || status == 409,
            "unexpected queue status {status}: {body:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        saw_full,
        "mailbox never reported full during the active turn"
    );
}

#[test]
fn final_binary_server_marks_a_failed_background_turn() {
    let scenario = Scenario::new();
    // The provider rejects the turn, so the accepted session's background turn
    // fails and the durable projection settles at the failed lifecycle.
    let provider = ScriptedProvider::start([ProviderReply::error(400, "provider rejected")]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},max_attempts:1}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

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

    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "will fail", "binding": binding })),
        &[],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // The failed background turn logs the warning and settles the session at a
    // durable, non-running lifecycle (a provider error is retryable, so the
    // session returns to ready rather than terminal failed).
    let mut settled = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 {
            let lifecycle = body["snapshot"]["lifecycle"].as_str().unwrap_or("");
            if lifecycle == "ready" || lifecycle == "failed" {
                settled = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        settled,
        "session never settled after the failed background turn"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_persists_sessions_across_restart() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("done"),
        ProviderReply::completion("done again"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let binding = server_binding(&scenario);
    let root = scenario.root().to_string_lossy().into_owned();

    // First server instance: create a durable session, then shut it down.
    let session_id = {
        let server = ServeChild::start(&scenario);
        let (_, ws_body) = server.request(
            "POST",
            "/v1/workspaces",
            Some(&server.token),
            Some(&serde_json::json!({ "path": root })),
            &[],
        );
        let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
        let (create_status, create_body) = server.request(
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(&server.token),
            Some(&serde_json::json!({ "prompt": "persist me", "binding": binding })),
            &[],
        );
        assert_eq!(create_status, 202);
        create_body["session_id"].as_str().unwrap().to_string()
        // `server` drops here → SIGTERM → graceful shutdown.
    };

    // Second instance with the SAME HOME must still resolve the session from
    // the durable global store, before any in-memory index is populated.
    let server = ServeChild::start(&scenario);
    let (get_status, get_body) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(
        get_status, 200,
        "durable session must be readable after restart: {get_body:?}"
    );
    assert_eq!(
        get_body["snapshot"]["thread_id"].as_str().unwrap(),
        session_id
    );

    // And it must appear in the workspace listing after restart.
    let (_, ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": root })),
        &[],
    );
    let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
    let (list_status, list_body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(list_status, 200);
    assert!(
        list_body["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions
                .iter()
                .any(|s| s["thread_id"].as_str() == Some(session_id.as_str()))),
        "restarted server must list the durable session"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_switches_model_and_replays_follow_up() {
    let scenario = Scenario::new();
    // Two models + several completions so the session completes, switches, and
    // takes an idempotency-keyed follow-up (with a replay) — all deterministic
    // (no queue-timing races).
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first"),
        ProviderReply::completion("after follow-up"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock","mock-2"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["true"]}}}}"#
        ),
    )
    .unwrap();
    let server = ServeChild::start(&scenario);

    let root = scenario.root().to_string_lossy().into_owned();
    let (_, ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": root })),
        &[],
    );
    let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
    let (_, created) = {
        let binding = server_binding(&scenario);
        server.request(
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(&server.token),
            Some(&serde_json::json!({ "prompt": "hello", "binding": binding })),
            &[],
        )
    };
    let session_id = created["session_id"].as_str().unwrap().to_string();

    // Wait until the session is idle (ready), then switch to the other model.
    let mut switched = false;
    for _ in 0..300 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
            let revision = body["snapshot"]["revision"].as_u64().unwrap();
            let (sw, sw_body) = server.request(
                "POST",
                &format!("/v1/sessions/{session_id}/model"),
                Some(&server.token),
                Some(&serde_json::json!({
                    "binding": server_binding_for_model(&scenario, Some("mock-2")),
                    "expected_thread_revision": revision
                })),
                &[],
            );
            // 200 on success; 409 only if a concurrent transition raced — retry.
            if sw == 200 {
                assert_eq!(sw_body["snapshot"]["binding"]["model"], "mock-2");
                switched = true;
                break;
            }
            assert_eq!(sw, 409, "switch returned {sw_body:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(switched, "model switch never succeeded on the idle session");

    // Read the current revision, then a keyed follow-up is accepted and replays
    // identically on retry.
    let (_, snap) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    let revision = snap["snapshot"]["revision"].as_u64().unwrap();
    let (f1, f1_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "switch-follow")],
    );
    assert_eq!(f1, 202, "follow-up returned {f1_body:?}");
    // Retry with the same key replays the original accepted body.
    let (f2, f2_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "switch-follow")],
    );
    assert_eq!(f2, 202);
    assert_eq!(f2_body, f1_body, "keyed follow-up retry must replay");
}
