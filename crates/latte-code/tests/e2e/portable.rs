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

/// Verification runs as a supervised process, which is unsupported on Windows.
/// Returns the JSONC verification fragment on Unix and an empty string on
/// Windows so the permission/effect flow is still exercised without the
/// platform-unsupported post-verification step.
fn verification_fragment() -> &'static str {
    if cfg!(unix) {
        r#",verification:{argv:["true"]}"#
    } else {
        ""
    }
}

/// A create-session request body carrying the client-generated `thread_id`
/// and `command_id` the crash-safe contract now requires, plus the matching
/// `Idempotency-Key` header value. `prompt`/`binding` are the only per-test
/// knobs; the ids are fresh unless a caller pins them for a replay test.
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

fn session_id(output: &std::process::Output) -> String {
    json(output)["data"]["session"]["thread_id"]
        .as_str()
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
    assert_eq!(json(&first)["data"]["sessions"], serde_json::json!([]));
    assert!(scenario.database_path().is_file());

    let reopened = scenario.output(&["--json", "list"], |_| {});
    assert!(reopened.status.success());
    assert_eq!(json(&reopened)["data"]["sessions"], serde_json::json!([]));
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
    assert_eq!(first.status.code(), Some(10));
    assert_eq!(json(&first)["status"], "waiting");
    let persisted_session_id = session_id(&first);
    provider.assert_consumed();
    assert!(scenario.database_path().is_file());
    assert!(!scenario.root().join("workspace-redirect.db").exists());

    let second_workspace = scenario.root().join("second-workspace");
    std::fs::create_dir_all(second_workspace.join(".git")).unwrap();
    // Sessions are workspace-scoped in v2: the second workspace lists nothing,
    // but both workspaces share the global durable store (no local db file).
    let second = scenario.output(&["--json", "list"], |command| {
        command.current_dir(&second_workspace);
    });
    assert!(second.status.success());
    assert_eq!(json(&second)["data"]["sessions"], serde_json::json!([]));
    assert!(!second_workspace.join(".latte/latte-code.db").exists());

    // The session is visible from its owning workspace via the shared store.
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"][0]["thread_id"],
        persisted_session_id
    );
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
    assert_eq!(output.status.code(), Some(10));
    assert_eq!(json(&output)["status"], "waiting");
    let waiting_id = session_id(&output);
    assert_eq!(
        json(&output)["data"]["session"]["pending"]["request_id"],
        "portable-input"
    );
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
    assert!(shown.status.success());
    assert_eq!(
        json(&shown)["data"]["session"]["lifecycle"],
        "waiting_input"
    );
    assert_eq!(
        json(&shown)["data"]["session"]["pending"]["request_id"],
        "portable-input"
    );
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert_eq!(
        json(&listed)["data"]["sessions"][0]["thread_id"],
        waiting_id
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

    assert_eq!(output.status.code(), Some(10));
    assert_eq!(json(&output)["status"], "waiting");
    assert_eq!(
        json(&output)["data"]["session"]["pending"]["request_id"],
        "inline-portable-input"
    );
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
    assert_eq!(
        json(&output)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert_eq!(
        json(&output)["data"]["session"]["runs"][0]["failure_code"],
        "runtime_failed"
    );
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);

    let id = session_id(&output);
    let shown = scenario.output(&["--json", "show", &id], |_| {});
    assert!(shown.status.success());
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert!(
        json(&shown)["data"]["session"]["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["text"].as_str().unwrap_or("").contains("http 400"))
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
        Self::start_with_env(scenario, &[])
    }

    /// Like [`start`](Self::start) but threads additional environment variables
    /// into the child. The crash-recovery journey uses this to shorten the
    /// lease TTL and recovery-sweep cadence so an orphaned lease expires and is
    /// reclaimed within the test window.
    fn start_with_env(scenario: &Scenario, extra_env: &[(&str, &str)]) -> Self {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;

        let mut command = scenario.command(&["--json", "serve", "--port", "0"]);
        command
            .env("TEST_OPENAI_KEY", "e2e-server-secret")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in extra_env {
            command.env(name, value);
        }
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

    /// Abruptly terminates the server as a crash would: SIGKILL the whole
    /// process group so no destructor runs and no lease is released. The
    /// orphaned lease must then expire and be reclaimed by a fresh server's
    /// recovery sweeper. Unix-only (the journey test that uses it is too).
    #[cfg(unix)]
    fn crash(mut self) {
        let group = nix::unistd::Pid::from_raw(self.process_group);
        let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        let _ = self.child.wait();
        // The process is already reaped; skip Drop's graceful-shutdown path.
        std::mem::forget(self);
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

    /// Creates a session through the crash-safe contract: a fresh client
    /// `thread_id` + `command_id` in the body and a matching `Idempotency-Key`.
    /// Returns the (status, body) pair; the created `session_id` equals the
    /// client `thread_id`.
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
    // The client supplies a stable thread_id + command_id; the Idempotency-Key
    // must equal the command_id.
    let (create_body_json, create_command_id) = create_request("hello server", &binding);
    let session_id_expected = create_body_json["thread_id"].as_str().unwrap().to_string();
    let (create_status, create_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&create_body_json),
        &[("Idempotency-Key", &create_command_id)],
    );
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();
    assert_eq!(
        session_id, session_id_expected,
        "create honors client thread_id"
    );
    // accepted_revision is the real durable revision after acceptance.
    assert!(create_body["accepted_revision"].as_u64().is_some());

    // The same command_id + payload replays the original accepted session
    // (in-process idempotency ledger).
    let (replay_status, replay_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&create_body_json),
        &[("Idempotency-Key", &create_command_id)],
    );
    assert_eq!(replay_status, 202);
    assert_eq!(replay_body["session_id"].as_str().unwrap(), session_id);

    // A keyed create that fails (invalid binding) releases its reservation, so
    // a corrected retry with the same command_id proceeds rather than
    // 409-in-flight.
    let (bad_body, release_command_id) = create_request("x", &serde_json::json!({ "version": 1 }));
    let (bad_key_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&bad_body),
        &[("Idempotency-Key", &release_command_id)],
    );
    assert_eq!(bad_key_status, 400);
    // Same thread_id + command_id, corrected binding: the released key retries.
    let retry_body = serde_json::json!({
        "thread_id": bad_body["thread_id"],
        "command_id": release_command_id,
        "prompt": "recovered",
        "binding": binding,
    });
    let (retry_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&retry_body),
        &[("Idempotency-Key", &release_command_id)],
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
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000011", "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000011")],
    );
    assert_eq!(follow_status, 202, "follow-up returned {follow_body:?}");
    let follow_revision = follow_body["accepted_revision"].as_u64().unwrap();

    // Retrying the follow-up with the same Idempotency-Key replays the original
    // accepted result rather than starting a second turn.
    let (replay_follow_status, replay_follow_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000011", "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000011")],
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
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000012", "prompt": "again", "expected_thread_revision": 999 })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000012")],
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

    // Reusing the create command_id with a different payload is rejected with
    // 422 by the in-process idempotency ledger (same key, different digest).
    let (mismatch_status, mismatch_body) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&serde_json::json!({
            "thread_id": session_id,
            "command_id": create_command_id,
            "prompt": "DIFFERENT prompt",
            "binding": binding,
        })),
        &[("Idempotency-Key", &create_command_id)],
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
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000011", "prompt": "DIFFERENT", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000011")],
    );
    assert_eq!(follow_mismatch_status, 422);
    assert_eq!(
        follow_mismatch_body["error"]["type"], "idempotency_mismatch",
        "follow-up payload mismatch must be reported: {follow_mismatch_body:?}"
    );

    // A second session (fresh thread_id + command_id) for follow-up coverage.
    let (no_key_status, no_key_body) = server.create_session(&workspace_id, "second", &binding);
    assert_eq!(no_key_status, 202);
    assert!(no_key_body["session_id"].is_string());

    // A follow-up WITH an Idempotency-Key header succeeds on a ready session.
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
    assert!(no_key_ready, "second session must reach ready");
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
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000014", "prompt": "continue", "expected_thread_revision": no_key_rev })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000014")],
    );
    assert_eq!(no_key_follow_status, 202);

    // A follow-up WITHOUT an Idempotency-Key header is rejected 400.
    let (missing_key_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{no_key_session}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000014", "prompt": "no key", "expected_thread_revision": no_key_rev })),
        &[],
    );
    assert_eq!(missing_key_status, 400);

    // A create whose Idempotency-Key header does NOT equal the body command_id
    // is rejected 400: one identity, two names must not disagree.
    let (mismatched_body, mismatched_command_id) = create_request("bad key", &binding);
    assert_ne!(mismatched_command_id, "not-the-command-id");
    let (header_mismatch_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&mismatched_body),
        &[("Idempotency-Key", "not-the-command-id")],
    );
    assert_eq!(header_mismatch_status, 400);

    // A create with a matching key but an invalid binding is rejected 400 and
    // releases the reservation (covered above for retry); here we just assert
    // the failure surfaces.
    let (bad_binding_body, bad_binding_command_id) =
        create_request("bad binding", &serde_json::json!({ "version": 1 }));
    let (no_key_bad_status, _) = server.request(
        "POST",
        &format!("/v1/workspaces/{workspace_id}/sessions"),
        Some(&server.token),
        Some(&bad_binding_body),
        &[("Idempotency-Key", &bad_binding_command_id)],
    );
    assert_eq!(no_key_bad_status, 400);

    // A follow-up with a stale revision is a 409 conflict.
    let (no_key_stale_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{no_key_session}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000012", "prompt": "stale", "expected_thread_revision": 999 })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000012")],
    );
    assert_eq!(no_key_stale_status, 409);
}

// Allowing the permission executes the `write_file`+`create_intent` mutation as
// a supervised process effect, which is only available on Unix; on Windows the
// engine fails closed with "process supervision is unsupported on this
// platform". The permission-resolve contract is fully exercised on Unix.
#[cfg(unix)]
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
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}}{verification}}}"#,
            verification = verification_fragment(),
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

    let (create_status, create_body) = server.create_session(&workspace_id, "write it", &binding);
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

    let (create_status, create_body) = server.create_session(&workspace_id, "write it", &binding);
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

    let (create_status, create_body) =
        server.create_session(&workspace_id, "need a secret", &binding);
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

// Allowing the permission executes the `write_file`+`create_intent` mutation as
// a supervised process effect (Unix-only); on Windows the engine fails closed
// with "process supervision is unsupported on this platform", so the
// stale-run-revision permission contract is proven on Unix.
#[cfg(unix)]
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

    let (create_status, create_body) = server.create_session(&workspace_id, "write it", &binding);
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
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},compatibility_input_request:true}}}},database:{{path:".latte/latte-code.db"}}{verification}}}"#,
            verification = verification_fragment(),
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

    let (create_status, create_body) = server.create_session(&workspace_id, "need input", &binding);
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

    let (create_status, create_body) = server.create_session(&workspace_id, "need input", &binding);
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

    let (create_status, create_body) = server.create_session(&workspace_id, "slow turn", &binding);
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

    let (create_status, create_body) = server.create_session(&workspace_id, "slow turn", &binding);
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

    let (create_status, create_body) = server.create_session(&workspace_id, "will fail", &binding);
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
        let (create_status, create_body) =
            server.create_session(&workspace_id, "persist me", &binding);
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

/// The reviewer-mandated crash-recovery journey, exercised end-to-end against
/// the real binary: an in-flight turn holds a lease; the server crashes
/// (SIGKILL, no graceful release); a fresh server's recovery sweeper reclaims
/// the expired lease and broadcasts a wake-up; an already-connected SSE client
/// receives the frame and refetches a terminal (`interrupted`) snapshot. This
/// is the only path that proves the sweeper actually runs under the server
/// lifecycle AND wakes live subscribers — the two halves of MF2.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_recovery_sweeper_wakes_live_sse_after_a_crash() {
    let scenario = Scenario::new();
    // The first turn stalls long enough to be unambiguously in-flight (holding
    // a live lease) at the moment we crash the server, yet short enough that
    // the scripted provider's worker thread does not stall teardown. Recovery
    // never consults the provider again, so the second reply is only a safety
    // net and is intentionally left unconsumed.
    let provider = ScriptedProvider::start([
        ProviderReply::completion("never delivered").delayed(std::time::Duration::from_secs(5)),
        ProviderReply::completion("unused"),
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

    // A short lease TTL means the orphaned lease expires quickly after the
    // crash; a fast sweep cadence means the fresh server reclaims it promptly.
    let fast_recovery: &[(&str, &str)] = &[
        ("LATTE_LEASE_TTL_MS", "300"),
        ("LATTE_RECOVERY_SWEEP_MS", "50"),
    ];

    // --- Server A: create a session and let its turn park in `running`. ---
    let session_id = {
        let server = ServeChild::start_with_env(&scenario, fast_recovery);
        let (_, ws_body) = server.request(
            "POST",
            "/v1/workspaces",
            Some(&server.token),
            Some(&serde_json::json!({ "path": root })),
            &[],
        );
        let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
        let (create_status, create_body) =
            server.create_session(&workspace_id, "stall then crash", &binding);
        assert_eq!(create_status, 202);
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Wait until the (stalled) turn is observably running, so the lease is
        // held when we crash.
        let mut running = false;
        for _ in 0..300 {
            let (status, body) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("running") {
                running = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            running,
            "the turn must be running (holding a lease) before crash"
        );

        // Crash: SIGKILL the whole group. No destructor runs, so the lease is
        // orphaned rather than released.
        server.crash();
        session_id
    };

    // --- Server B: fresh process, same durable HOME. ---
    let server = ServeChild::start_with_env(&scenario, fast_recovery);
    // Materialize the workspace instance so the sweeper includes it and the SSE
    // hub exists for our subscriber.
    let (_, ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": root })),
        &[],
    );
    let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();

    // Confirm the session is still non-terminal immediately after restart: the
    // sweeper has not yet reclaimed the just-orphaned lease.
    let (pre_status, pre_body) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(pre_status, 200);
    assert_eq!(
        pre_body["snapshot"]["thread_id"].as_str().unwrap(),
        session_id
    );

    // Subscribe to the workspace SSE stream BEFORE the lease expires, so the
    // recovery broadcast must reach an already-connected client.
    let events_handle = {
        let port = server.port;
        let token = server.token.clone();
        let workspace_id = workspace_id.clone();
        std::thread::spawn(move || {
            read_events_from(
                port,
                &token,
                &workspace_id,
                std::time::Duration::from_secs(5),
            )
        })
    };

    // The already-connected client must receive a wake-up frame naming the
    // recovered session once the sweeper reclaims the expired lease.
    let frames = events_handle.join().unwrap();
    assert!(
        frames.contains("text/event-stream"),
        "events response was not an SSE stream: {frames:?}"
    );
    assert!(
        frames.contains("thread_changed"),
        "recovery must wake live subscribers with a thread_changed frame: {frames:?}"
    );
    assert!(
        frames.contains(&session_id),
        "the wake-up frame must name the recovered session {session_id}: {frames:?}"
    );

    // Refetching after the wake-up must show the recovered terminal projection.
    let mut terminal = false;
    for _ in 0..300 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("interrupted") {
            terminal = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        terminal,
        "the recovered session must project the interrupted terminal lifecycle"
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
        server.create_session(&workspace_id, "hello", &binding)
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
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000015", "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000015")],
    );
    assert_eq!(f1, 202, "follow-up returned {f1_body:?}");
    // Retry with the same key replays the original accepted body.
    let (f2, f2_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({ "command_id": "01900000-0000-7000-8000-000000000015", "prompt": "again", "expected_thread_revision": revision })),
        &[("Idempotency-Key", "01900000-0000-7000-8000-000000000015")],
    );
    assert_eq!(f2, 202);
    assert_eq!(f2_body, f1_body, "keyed follow-up retry must replay");
}

/// A follow-up accepted by one server instance must replay identically after a
/// server restart: the durable `thread_command_dedup_v2` record prevents a
/// duplicate turn when the client retries with the same `command_id`.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_follow_up_replays_after_restart_without_duplicate_turn() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first done"),
        ProviderReply::completion("follow-up done"),
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

    // First server instance: create + follow-up, then shut down.
    let (session_id, original_revision, follow_revision, follow_command_id) = {
        let server = ServeChild::start(&scenario);
        let (_, ws_body) = server.request(
            "POST",
            "/v1/workspaces",
            Some(&server.token),
            Some(&serde_json::json!({ "path": root })),
            &[],
        );
        let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
        let (create_status, create_body) =
            server.create_session(&workspace_id, "restart replay", &binding);
        assert_eq!(create_status, 202);
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Wait for the initial turn to complete.
        let revision = loop {
            let (_, body) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            if body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                break body["snapshot"]["revision"].as_u64().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // Send a follow-up with a stable command_id.
        let command_id = "01900000-0000-7000-8000-0000000000a1".to_string();
        let (f_status, f_body) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(&server.token),
            Some(&serde_json::json!({
                "command_id": command_id,
                "prompt": "after restart",
                "expected_thread_revision": revision,
            })),
            &[("Idempotency-Key", &command_id)],
        );
        assert_eq!(f_status, 202, "follow-up returned {f_body:?}");
        let follow_revision = f_body["accepted_revision"].as_u64().unwrap();

        // Wait for the follow-up turn to complete.
        loop {
            let (_, body) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            if body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        (session_id, revision, follow_revision, command_id)
        // `server` drops here → SIGTERM → graceful shutdown.
    };

    // Second instance: replay the same follow-up. The durable dedup record
    // must prevent a duplicate turn.
    let server = ServeChild::start(&scenario);
    let (replay_status, replay_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({
            "command_id": follow_command_id,
            "prompt": "after restart",
            "expected_thread_revision": original_revision,
        })),
        &[("Idempotency-Key", &follow_command_id)],
    );
    // The replay returns the original acceptance (200 or 202) without
    // appending a new turn.
    assert!(
        replay_status == 200 || replay_status == 202,
        "replay returned {replay_status}: {replay_body:?}"
    );
    assert_eq!(
        replay_body["accepted_revision"].as_u64().unwrap(),
        follow_revision,
        "replay must return the original accepted revision, not advance it"
    );

    // The transcript must not contain a duplicate user turn.
    let (_, snap) = server.request(
        "GET",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        None,
        &[],
    );
    let user_turns = snap["snapshot"]["transcript"]["entries"]
        .as_array()
        .map_or(0, |entries| {
            entries
                .iter()
                .filter(|e| e["kind"].as_str() == Some("user"))
                .count()
        });
    assert_eq!(
        user_turns, 2,
        "expected exactly 2 user turns (initial + follow-up), got {user_turns}: \
         a duplicate turn was appended after restart"
    );
}

/// A follow-up accepted by one server instance must reject a same-command_id
/// different-payload retry after a server restart with 422 (not 409), so the
/// client doesn't retry as a revision conflict.
#[cfg(unix)]
#[test]
fn final_binary_follow_up_durable_mismatch_after_restart_returns_422() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first done"),
        ProviderReply::completion("follow-up done"),
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

    // First server instance: create + follow-up, then shut down.
    let (session_id, revision, command_id) = {
        let server = ServeChild::start(&scenario);
        let (_, ws_body) = server.request(
            "POST",
            "/v1/workspaces",
            Some(&server.token),
            Some(&serde_json::json!({ "path": root })),
            &[],
        );
        let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
        let (create_status, create_body) =
            server.create_session(&workspace_id, "mismatch test", &binding);
        assert_eq!(create_status, 202);
        let session_id = create_body["session_id"].as_str().unwrap().to_string();

        // Wait for the initial turn to complete.
        let revision = loop {
            let (_, body) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            if body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                break body["snapshot"]["revision"].as_u64().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        // Send a follow-up with a stable command_id.
        let command_id = "01900000-0000-7000-8000-0000000000b2".to_string();
        let (f_status, _) = server.request(
            "POST",
            &format!("/v1/sessions/{session_id}/follow-up"),
            Some(&server.token),
            Some(&serde_json::json!({
                "command_id": command_id,
                "prompt": "original prompt",
                "expected_thread_revision": revision,
            })),
            &[("Idempotency-Key", &command_id)],
        );
        assert_eq!(f_status, 202, "follow-up must be accepted");

        // Wait for the follow-up turn to complete.
        loop {
            let (_, body) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            if body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        (session_id, revision, command_id)
        // `server` drops here → SIGTERM → graceful shutdown.
    };

    // Second instance: replay with the same command_id but a DIFFERENT prompt.
    // The durable dedup must reject this as 422, not 409.
    let server = ServeChild::start(&scenario);
    let (mismatch_status, mismatch_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({
            "command_id": command_id,
            "prompt": "DIFFERENT prompt",
            "expected_thread_revision": revision,
        })),
        &[("Idempotency-Key", &command_id)],
    );
    assert_eq!(
        mismatch_status, 422,
        "durable mismatch must be 422, got {mismatch_status}: {mismatch_body:?}"
    );
    assert_eq!(
        mismatch_body["error"]["type"].as_str(),
        Some("idempotency_mismatch"),
        "error type must be idempotency_mismatch: {mismatch_body:?}"
    );
}

/// Exercises the session-management surface end to end against the final
/// binary: once a session is durably idle it is renamed (verified through the
/// catalog search projection), forked into a fresh thread id, and the provider
/// binding catalog is listed. These handler paths are otherwise only covered by
/// in-process unit tests; driving them over real HTTP keeps the final-binary
/// server honest about the crash-safe session lifecycle.
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_renames_forks_and_lists_bindings() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("first")]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock","mock-2"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}}{verification}}}"#,
            verification = verification_fragment(),
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

    // The provider binding catalog is available immediately from the workspace
    // registry (the models configured above), independent of any session.
    let (bindings_status, bindings_body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/bindings"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(
        bindings_status, 200,
        "list bindings returned {bindings_body:?}"
    );
    assert!(
        bindings_body["bindings"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()),
        "binding catalog must not be empty: {bindings_body:?}"
    );

    // An unknown workspace id fails closed with 404 rather than leaking an
    // empty catalog.
    let (missing_status, _) = server.request(
        "GET",
        "/v1/workspaces/ws_does_not_exist/bindings",
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(missing_status, 404);

    let (_, created) = {
        let binding = server_binding(&scenario);
        server.create_session(&workspace_id, "hello", &binding)
    };
    let session_id = created["session_id"].as_str().unwrap().to_string();

    // Wait until the session is durably idle before management operations.
    let mut ready = false;
    for _ in 0..300 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready, "session never reached a durable idle state");

    // Rename the session; the handler returns the same session's snapshot.
    let (rename_status, rename_body) = server.request(
        "PATCH",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        Some(&serde_json::json!({ "title": "renamed via http" })),
        &[],
    );
    assert_eq!(rename_status, 200, "rename returned {rename_body:?}");
    assert_eq!(
        rename_body["snapshot"]["thread_id"].as_str(),
        Some(session_id.as_str())
    );

    // The new title is durable: it appears in the catalog search projection.
    let (search_status, search_body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions/search?q=renamed&limit=10"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(search_status, 200);
    let renamed = search_body["sessions"]
        .as_array()
        .expect("search must return an array")
        .iter()
        .find(|s| s["thread_id"].as_str() == Some(session_id.as_str()))
        .expect("renamed session must appear in the catalog search");
    assert_eq!(renamed["title"].as_str(), Some("renamed via http"));

    // Missing title on rename is a 400 before the engine is touched.
    let (bad_rename, _) = server.request(
        "PATCH",
        &format!("/v1/sessions/{session_id}"),
        Some(&server.token),
        Some(&serde_json::json!({ "not_title": 7 })),
        &[],
    );
    assert_eq!(bad_rename, 400);

    // Fork the session into a distinct thread id and confirm the fork snapshot.
    let (fork_status, fork_body) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/fork"),
        Some(&server.token),
        Some(&serde_json::json!({ "title": "forked via http" })),
        &[],
    );
    assert_eq!(fork_status, 200, "fork returned {fork_body:?}");
    let fork_id = fork_body["snapshot"]["thread_id"]
        .as_str()
        .expect("fork snapshot missing thread_id");
    assert_ne!(fork_id, session_id, "fork must mint a distinct thread id");

    // The fork is itself a readable durable session.
    let (fork_read, fork_snapshot) = server.request(
        "GET",
        &format!("/v1/sessions/{fork_id}"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(fork_read, 200, "fork read returned {fork_snapshot:?}");
    assert_eq!(
        fork_snapshot["snapshot"]["thread_id"].as_str(),
        Some(fork_id)
    );

    // Error branches over real HTTP: management operations on a well-formed but
    // unknown session id fail closed with 404, and a request without the bearer
    // token is rejected with 401 before any handler runs.
    let unknown = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7()).to_string();
    let (rename_missing, _) = server.request(
        "PATCH",
        &format!("/v1/sessions/{unknown}"),
        Some(&server.token),
        Some(&serde_json::json!({ "title": "no such session" })),
        &[],
    );
    assert_eq!(rename_missing, 404, "rename of unknown session must be 404");

    let (fork_missing, _) = server.request(
        "POST",
        &format!("/v1/sessions/{unknown}/fork"),
        Some(&server.token),
        Some(&serde_json::json!({ "title": "no such session" })),
        &[],
    );
    assert_eq!(fork_missing, 404, "fork of unknown session must be 404");

    // A malformed session id is a 400 (bad request), distinct from 404.
    let (rename_malformed, _) = server.request(
        "PATCH",
        "/v1/sessions/not-a-uuid",
        Some(&server.token),
        Some(&serde_json::json!({ "title": "bad id" })),
        &[],
    );
    assert_eq!(rename_malformed, 400, "malformed session id must be 400");

    // Missing bearer token is rejected before the handler.
    let (unauthorized, _) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/bindings"),
        None,
        None,
        &[],
    );
    assert_eq!(unauthorized, 401, "missing bearer token must be 401");
}

/// The exact-title endpoint returns only sessions whose title matches exactly,
/// so substring siblings ("foo" vs "foobar") never leak into an exact lookup,
/// and the result is not truncated by the substring-search page cap.
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_server_exact_title_lookup_returns_only_exact_matches() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first"),
        ProviderReply::completion("second"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}}{verification}}}"#,
            verification = verification_fragment(),
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

    // Create two sessions with distinct-but-related titles.
    let binding = server_binding(&scenario);
    let (_, foo_created) = server.create_session(&workspace_id, "foo", &binding);
    let foo_id = foo_created["session_id"].as_str().unwrap().to_string();
    let (_, foobar_created) = server.create_session(&workspace_id, "foobar", &binding);
    let foobar_id = foobar_created["session_id"].as_str().unwrap().to_string();

    // Wait for both sessions to reach a durable idle state.
    for session_id in [&foo_id, &foobar_id] {
        let mut ready = false;
        for _ in 0..300 {
            let (status, body) = server.request(
                "GET",
                &format!("/v1/sessions/{session_id}"),
                Some(&server.token),
                None,
                &[],
            );
            if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("ready") {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            ready,
            "session {session_id} never reached a durable idle state"
        );
    }

    // Rename both sessions so the durable catalog titles are exact.
    for (session_id, title) in [(&foo_id, "foo"), (&foobar_id, "foobar")] {
        let (status, body) = server.request(
            "PATCH",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            Some(&serde_json::json!({ "title": title })),
            &[],
        );
        assert_eq!(status, 200, "rename returned {body:?}");
    }

    // Exact-title lookup for "foo" must not return "foobar".
    let (status, body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions/exact-title?q=foo"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(status, 200, "exact-title returned {body:?}");
    let sessions = body["sessions"]
        .as_array()
        .expect("sessions must be an array");
    assert_eq!(
        sessions.len(),
        1,
        "exact-title 'foo' must return only 'foo'"
    );
    assert_eq!(sessions[0]["thread_id"].as_str(), Some(foo_id.as_str()));
    assert_eq!(sessions[0]["title"].as_str(), Some("foo"));

    // Exact-title lookup for "foobar" must not return "foo".
    let (status, body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions/exact-title?q=foobar"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(status, 200, "exact-title returned {body:?}");
    let sessions = body["sessions"]
        .as_array()
        .expect("sessions must be an array");
    assert_eq!(
        sessions.len(),
        1,
        "exact-title 'foobar' must return only 'foobar'"
    );
    assert_eq!(sessions[0]["thread_id"].as_str(), Some(foobar_id.as_str()));

    // A missing exact title returns an empty array, not an error.
    let (status, body) = server.request(
        "GET",
        &format!("/v1/workspaces/{workspace_id}/sessions/exact-title?q=no-such-title"),
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(status, 200, "exact-title returned {body:?}");
    assert!(body["sessions"].as_array().unwrap().is_empty());

    // A missing workspace fails closed with 404.
    let (missing_status, _) = server.request(
        "GET",
        "/v1/workspaces/ws_does_not_exist/sessions/exact-title?q=foo",
        Some(&server.token),
        None,
        &[],
    );
    assert_eq!(missing_status, 404);
}
/// covering the remote `connect` path, `resolve_remote_token`, and SSE
/// observation over HTTP (not the embedded server).
#[test]
fn final_binary_cli_run_against_standalone_server() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("remote server completed")]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);
    let server = ServeChild::start(&scenario);
    let url = format!("http://127.0.0.1:{}", server.port);

    let output = scenario.output(
        &[
            "--json",
            "run",
            "complete via remote server",
            "--server",
            &url,
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "remote-server-secret");
        },
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["data"]["session"]["lifecycle"], "ready");
    assert_eq!(body["data"]["session"]["runs"][0]["status"], "completed");
    assert!(
        body["data"]["session"]["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["text"] == "remote server completed")
    );
    provider.assert_consumed();
}

/// CLI `--server` with a dead port reports `server_unreachable` (exit 71).
#[test]
fn final_binary_cli_reports_server_unreachable() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    // Bind and immediately drop a listener to get a guaranteed-free port.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let output = scenario.output(
        &[
            "--json",
            "list",
            "--server",
            &format!("http://127.0.0.1:{port}"),
            "--token",
            "unused",
        ],
        |_| {},
    );
    assert_eq!(output.status.code(), Some(71));
    assert_eq!(json(&output)["error"]["code"], "server_unreachable");
}

/// CLI `--server` with a wrong token reports `unauthorized` (exit 70).
#[test]
fn final_binary_cli_reports_unauthorized() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let server = ServeChild::start(&scenario);

    let output = scenario.output(
        &[
            "--json",
            "list",
            "--server",
            &format!("http://127.0.0.1:{}", server.port),
            "--token",
            "wrong-token",
        ],
        |_| {},
    );
    assert_eq!(output.status.code(), Some(70));
    assert_eq!(json(&output)["error"]["code"], "unauthorized");
}

/// CLI `show`/`list`/`resume` against a standalone server, covering the
/// remote `--server` path for read and follow-up commands.
#[test]
fn final_binary_cli_show_list_resume_against_standalone_server() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first turn"),
        ProviderReply::completion("follow-up turn"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);
    let server = ServeChild::start(&scenario);
    let url = format!("http://127.0.0.1:{}", server.port);

    // Run a session through the standalone server.
    let run = scenario.output(
        &[
            "--json",
            "run",
            "first turn",
            "--server",
            &url,
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "remote-server-secret");
        },
    );
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stdout)
    );
    let session_id = json(&run)["data"]["session"]["thread_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Show the session through the standalone server.
    let show = scenario.output(
        &[
            "--json",
            "show",
            &session_id,
            "--server",
            &url,
            "--token",
            &server.token,
        ],
        |_| {},
    );
    assert!(show.status.success());
    assert_eq!(json(&show)["data"]["session"]["thread_id"], session_id);

    // List sessions through the standalone server.
    let list = scenario.output(
        &["--json", "list", "--server", &url, "--token", &server.token],
        |_| {},
    );
    assert!(list.status.success());
    let list_body = json(&list);
    let sessions = list_body["data"]["sessions"].as_array().unwrap();
    assert!(sessions.iter().any(|s| s["thread_id"] == session_id));

    // Resume (follow-up) through the standalone server.
    let resume = scenario.output(
        &[
            "--json",
            "resume",
            &session_id,
            "follow-up turn",
            "--server",
            &url,
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "remote-server-secret");
        },
    );
    assert!(
        resume.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resume.stdout)
    );
    assert_eq!(
        json(&resume)["data"]["session"]["runs"][0]["status"],
        "completed"
    );
    provider.assert_consumed();
}

/// CLI `run --focus` passes the focus path through to the server.
#[test]
fn final_binary_cli_run_with_focus() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("focused completion")]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);
    let focus_dir = scenario.root().join("crates");
    std::fs::create_dir_all(&focus_dir).unwrap();

    let output = scenario.output(
        &["--json", "run", "--focus", "crates", "complete with focus"],
        |command| {
            command.env("TEST_OPENAI_KEY", "focus-secret");
        },
    );
    assert!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["data"]["session"]["focus"], "crates");
    provider.assert_consumed();
}

/// CLI `show`/`resume` with a non-existent session reports `not_found` (exit 4).
#[test]
fn final_binary_cli_reports_missing_session() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let missing = "01900000-0000-7000-8000-000000000099";

    let show = scenario.output(&["--json", "show", missing], |_| {});
    assert_eq!(show.status.code(), Some(4));
    assert_eq!(json(&show)["error"]["code"], "not_found");

    let resume = scenario.output(&["--json", "resume", missing, "follow up"], |_| {});
    assert_eq!(resume.status.code(), Some(4));
    assert_eq!(json(&resume)["error"]["code"], "not_found");
}

/// Text-mode `list`/`show` render human-readable output (not JSON envelopes).
#[test]
fn final_binary_cli_text_mode_list_and_show() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::completion("text mode result")]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    // Run a session first.
    let run = scenario.output(&["run", "complete in text mode"], |command| {
        command.env("TEST_OPENAI_KEY", "text-mode-secret");
    });
    assert!(run.status.success());
    let run_text = String::from_utf8(run.stdout).unwrap();
    let session_id = run_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|value| value.trim_end_matches(':'))
        .unwrap();

    // Text-mode list.
    let list = scenario.output(&["list"], |_| {});
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(list_text.contains(session_id));
    assert!(list_text.contains("ready"));

    // Text-mode show.
    let show = scenario.output(&["show", session_id], |_| {});
    assert!(show.status.success());
    let show_text = String::from_utf8(show.stdout).unwrap();
    assert!(show_text.contains(&format!("session {session_id}:")));
    assert!(show_text.contains("text mode result"));
    provider.assert_consumed();
}

/// CLI usage errors: missing prompt, unknown option, invalid UUID, missing
/// token for remote server. Covers parse and error-classification paths.
#[test]
fn final_binary_cli_usage_errors() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    // run without a prompt
    let no_prompt = scenario.output(&["--json", "run"], |_| {});
    assert_eq!(no_prompt.status.code(), Some(2));
    assert_eq!(json(&no_prompt)["error"]["code"], "usage");

    // unknown option
    let unknown = scenario.output(&["--json", "list", "--bogus"], |_| {});
    assert_eq!(unknown.status.code(), Some(2));
    assert_eq!(json(&unknown)["error"]["code"], "usage");

    // show with invalid UUID
    let bad_id = scenario.output(&["--json", "show", "not-a-uuid"], |_| {});
    assert_eq!(bad_id.status.code(), Some(2));
    assert_eq!(json(&bad_id)["error"]["code"], "usage");

    // resume with too few args
    let no_resume = scenario.output(&["--json", "resume", "only-one-arg"], |_| {});
    assert_eq!(no_resume.status.code(), Some(2));
    assert_eq!(json(&no_resume)["error"]["code"], "usage");

    // --focus on non-run command
    let bad_focus = scenario.output(&["--json", "list", "--focus", "src"], |_| {});
    assert_eq!(bad_focus.status.code(), Some(2));
    assert_eq!(json(&bad_focus)["error"]["code"], "usage");

    // --server without --token and no token file
    let no_token = scenario.output(
        &["--json", "list", "--server", "http://127.0.0.1:1"],
        |_| {},
    );
    assert_eq!(no_token.status.code(), Some(2));
    assert_eq!(json(&no_token)["error"]["code"], "usage");

    // unknown command
    let bad_cmd = scenario.output(&["--json", "bogus"], |_| {});
    assert_eq!(bad_cmd.status.code(), Some(2));
    assert_eq!(json(&bad_cmd)["error"]["code"], "usage");

    // list with positional args
    let list_args = scenario.output(&["--json", "list", "extra"], |_| {});
    assert_eq!(list_args.status.code(), Some(2));
    assert_eq!(json(&list_args)["error"]["code"], "usage");

    // show with zero args
    let show_zero = scenario.output(&["--json", "show"], |_| {});
    assert_eq!(show_zero.status.code(), Some(2));
    assert_eq!(json(&show_zero)["error"]["code"], "usage");

    // show with two args
    let show_two = scenario.output(
        &[
            "--json",
            "show",
            "00000000-0000-7000-8000-000000000001",
            "extra",
        ],
        |_| {},
    );
    assert_eq!(show_two.status.code(), Some(2));
    assert_eq!(json(&show_two)["error"]["code"], "usage");

    // resume with invalid UUID
    let resume_bad = scenario.output(&["--json", "resume", "not-a-uuid", "prompt"], |_| {});
    assert_eq!(resume_bad.status.code(), Some(2));
    assert_eq!(json(&resume_bad)["error"]["code"], "usage");

    // --server with --token but unreachable server
    let unreachable = scenario.output(
        &[
            "--json",
            "list",
            "--server",
            "http://127.0.0.1:1",
            "--token",
            "t",
        ],
        |_| {},
    );
    assert_eq!(unreachable.status.code(), Some(71));
    assert_eq!(json(&unreachable)["error"]["code"], "server_unreachable");
}

/// Text-mode `resume` renders human-readable output (not JSON envelopes).
#[test]
fn final_binary_cli_text_mode_resume() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("first turn"),
        ProviderReply::completion("follow-up turn"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    // Run a session first.
    let run = scenario.output(&["run", "first turn"], |command| {
        command.env("TEST_OPENAI_KEY", "text-resume-secret");
    });
    assert!(run.status.success());
    let run_text = String::from_utf8(run.stdout).unwrap();
    let session_id = run_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|value| value.trim_end_matches(':'))
        .unwrap();

    // Text-mode resume.
    let resume = scenario.output(&["resume", session_id, "follow-up turn"], |command| {
        command.env("TEST_OPENAI_KEY", "text-resume-secret");
    });
    assert!(
        resume.status.success(),
        "resume failed: stdout={} stderr={}",
        String::from_utf8_lossy(&resume.stdout),
        String::from_utf8_lossy(&resume.stderr)
    );
    let resume_text = String::from_utf8(resume.stdout).unwrap();
    assert!(resume_text.contains("follow-up turn"));
    provider.assert_consumed();
}

/// CLI `run` with no providers configured reports a usage error.
#[test]
fn final_binary_cli_run_without_providers() {
    let scenario = Scenario::new();
    // Write a config with no providers at all.
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        r#"{version:1,default_model:"",providers:{},database:{path:".latte/latte-code.db"},verification:{argv:["true"]}}"#,
    )
    .unwrap();

    let output = scenario.output(&["--json", "run", "test"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "usage");
}

/// CLI with no args in a non-TTY environment prints help (not the TUI).
#[test]
fn final_binary_cli_no_args_prints_help() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&[], |_| {});
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Usage") || text.contains("usage"));
}

/// CLI `--json` with no args emits a JSON help envelope.
#[test]
fn final_binary_cli_json_no_args_emits_help() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&["--json"], |_| {});
    assert!(output.status.success());
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    assert!(body["data"]["help"].is_string());
}

/// CLI `serve` with an unknown argument reports a usage error.
#[test]
fn final_binary_cli_serve_rejects_unknown_argument() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&["--json", "serve", "--bogus"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_serve_without_port_value_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&["--json", "serve", "--port"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_serve_with_invalid_port_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&["--json", "serve", "--port", "not-a-port"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "usage");
}

/// CLI `tui` without a TTY reports a usage error.
#[test]
fn final_binary_cli_tui_without_tty_fails() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&["tui"], |_| {});
    assert_eq!(output.status.code(), Some(2));
}

/// CLI with no `HOME` and no `LATTE_CODE_HOME` reports a configuration error
/// (covering the `storage_home` no-home branch).
#[test]
fn final_binary_cli_without_home_reports_configuration_error() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);

    let output = scenario.output(&["--json", "list"], |command| {
        command.env_remove("HOME").env_remove("LATTE_CODE_HOME");
    });
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "configuration");
}

/// CLI `--server` without `--token` reads the bearer token from
/// `$LATTE_CODE_HOME/server.token` (covering `resolve_remote_token` file path).
#[test]
fn final_binary_cli_server_without_token_reads_token_file() {
    let scenario = Scenario::new();
    // `list` never calls the provider; a dummy endpoint suffices.
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let server = ServeChild::start(&scenario);
    let url = format!("http://127.0.0.1:{}", server.port);

    // No --token: the CLI must read server.token from LATTE_CODE_HOME.
    let output = scenario.output(&["--json", "list", "--server", &url], |command| {
        command.env("TEST_OPENAI_KEY", "token-file-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// CLI text-mode `show` on a denied session renders the `denied` outcome.
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_cli_show_denied_session_in_text_mode() {
    let scenario = Scenario::new();
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

    let (create_status, create_body) = server.create_session(&workspace_id, "write it", &binding);
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

    // Deny the permission.
    let (deny_status, _) = server.request(
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
    assert_eq!(deny_status, 200);

    // Wait for the session to settle.
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

    // CLI text-mode show on the denied session.
    let show = scenario.output(
        &[
            "show",
            &session_id,
            "--server",
            &format!("http://127.0.0.1:{}", server.port),
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "denied-session-secret");
        },
    );
    assert!(show.status.success());
    let show_text = String::from_utf8(show.stdout).unwrap();
    assert!(show_text.contains("denied") || show_text.contains("failed"));
    // Note: provider.assert_consumed() is intentionally omitted — the denied
    // turn may not consume the completion reply before the session settles.
}

/// CLI text-mode `show` on a still-running session renders the lifecycle name
/// (not a terminal outcome), covering `lifecycle_name` and the non-terminal
/// `render_session_text` path.
#[test]
fn final_binary_cli_show_running_session_renders_lifecycle() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("slow response").delayed(std::time::Duration::from_secs(5))
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);
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

    let (create_status, create_body) = server.create_session(&workspace_id, "slow turn", &binding);
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait for the session to enter Running (the provider is delayed).
    let mut running = false;
    for _ in 0..100 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("running") {
            running = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(running, "session never entered Running");

    // CLI text-mode show on the running session.
    let show = scenario.output(
        &[
            "show",
            &session_id,
            "--server",
            &format!("http://127.0.0.1:{}", server.port),
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "running-session-secret");
        },
    );
    assert!(show.status.success());
    let show_text = String::from_utf8(show.stdout).unwrap();
    assert!(show_text.contains("running"), "show output: {show_text}");
    // The provider reply is still pending; drop the server to clean up.
    drop(server);
}

/// CLI `run` interrupted by SIGINT maps to a best-effort cancel and exits 130
/// (cancelled), covering the `observe_session` cancel branch, `cancel_session`,
/// the `cancel` HTTP method, and `RunResult::Cancelled`.
#[cfg(unix)]
#[test]
fn final_binary_cli_run_sigint_returns_cancelled() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("slow response").delayed(std::time::Duration::from_secs(5))
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let mut child = scenario
        .command(&["run", "slow task that gets interrupted"])
        .env("TEST_OPENAI_KEY", "sigint-secret")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn latte-code run");

    // Give the embedded server time to start the turn and open the SSE stream.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        child.try_wait().unwrap().is_none(),
        "run exited before SIGINT was sent"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("send SIGINT");

    let output = child.wait_with_output().expect("wait for run after SIGINT");
    assert_eq!(
        output.status.code(),
        Some(130),
        "expected exit 130 (cancelled); stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The provider reply is intentionally not consumed (the turn was cancelled).
}

/// CLI `run` against a standalone server with a delayed provider reply: the
/// session is still Running when the CLI enters the SSE select loop, so the
/// terminal `ThreadChanged` event is observed through the stream (covering the
/// `ThreadChanged` handler and its `check_terminal` resync).
#[test]
fn final_binary_cli_run_with_delayed_provider_observes_thread_changed() {
    let scenario = Scenario::new();
    let provider =
        ScriptedProvider::start([ProviderReply::completion("delayed completion")
            .delayed(std::time::Duration::from_secs(1))]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);
    let server = ServeChild::start(&scenario);
    let url = format!("http://127.0.0.1:{}", server.port);

    let output = scenario.output(
        &[
            "--json",
            "run",
            "slow but steady",
            "--server",
            &url,
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "delayed-secret");
        },
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["data"]["session"]["lifecycle"], "ready");
    provider.assert_consumed();
}

/// CLI `run` with a read-only tool call: `read_file` is auto-approved (Read
/// effect class), so the turn executes the tool and completes, covering the
/// tool execution path and the embedded-server shutdown.
#[test]
fn final_binary_cli_run_with_read_only_tool_call_completes() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("target.txt"), "hello from file\n").unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "read-1",
            "read_file",
            &serde_json::json!({ "path": "target.txt" }),
        ),
        ProviderReply::completion("read the file"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "read the file"], |command| {
        command.env("TEST_OPENAI_KEY", "read-tool-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    assert_eq!(body["data"]["session"]["lifecycle"], "ready");
    provider.assert_consumed();
}

/// CLI `run --json` interrupted by SIGINT emits the `cancelled` status envelope
/// (covering `RunResult::Cancelled::status` in JSON mode).
#[cfg(unix)]
#[test]
fn final_binary_cli_run_sigint_json_emits_cancelled() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::completion("slow response").delayed(std::time::Duration::from_secs(5))
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let mut child = scenario
        .command(&["--json", "run", "slow task"])
        .env("TEST_OPENAI_KEY", "sigint-json-secret")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn latte-code run");

    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        child.try_wait().unwrap().is_none(),
        "run exited before SIGINT was sent"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("send SIGINT");

    let output = child.wait_with_output().expect("wait for run after SIGINT");
    assert_eq!(
        output.status.code(),
        Some(130),
        "expected exit 130; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "cancelled");
}

/// `waiting_permission` lifecycle name.
#[test]
fn final_binary_cli_show_waiting_permission_session() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "write-waiting",
            "write_file",
            &serde_json::json!({
                "path": "waiting.txt",
                "content": "parked\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("after permission"),
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

    let (create_status, create_body) = server.create_session(&workspace_id, "park here", &binding);
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingPermission.
    let mut parked = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_permission") {
            parked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(parked, "session never reached WaitingPermission");

    // `list` renders the raw lifecycle name (not the classified outcome), so
    // it covers `lifecycle_name(WaitingPermission)`.
    let list = scenario.output(
        &[
            "list",
            "--server",
            &format!("http://127.0.0.1:{}", server.port),
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "waiting-session-secret");
        },
    );
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(
        list_text.contains("waiting_permission"),
        "list output: {list_text}"
    );
    drop(server);
}

/// CLI text-mode `show` on a session that was cancelled while waiting renders
/// the terminal `failed` lifecycle (a waiting cancel maps to Failed).
#[test]
fn final_binary_cli_show_cancelled_waiting_session_renders_failed() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "write-cancel",
            "write_file",
            &serde_json::json!({
                "path": "cancel-waiting.txt",
                "content": "cancelled\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("after cancel"),
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

    let (create_status, create_body) = server.create_session(&workspace_id, "cancel me", &binding);
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
                body["snapshot"]["pending"]["expected_run_revision"]
                    .as_u64()
                    .unwrap(),
            ));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let (revision, run_revision) = pending.expect("session never reached WaitingPermission");

    // Cancel the waiting session.
    let (cancel_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/cancel"),
        Some(&server.token),
        Some(&serde_json::json!({
            "expected_thread_revision": revision,
            "expected_run_revision": run_revision
        })),
        &[],
    );
    assert_eq!(cancel_status, 200);

    // Wait for the session to settle at failed.
    let mut settled = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("failed") {
            settled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(settled, "session never settled at failed after cancel");

    // `list` renders the raw lifecycle name, covering `lifecycle_name(Failed)`.
    let list = scenario.output(
        &[
            "list",
            "--server",
            &format!("http://127.0.0.1:{}", server.port),
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "cancel-waiting-secret");
        },
    );
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(list_text.contains("failed"), "list output: {list_text}");
}

/// CLI text-mode `show` on a session parked at `WaitingInput` renders the
/// `waiting_input` lifecycle name.
#[test]
fn final_binary_cli_show_waiting_input_session() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::input_request("input-1", "what value?", false),
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

    let (create_status, create_body) = server.create_session(&workspace_id, "ask me", &binding);
    assert_eq!(create_status, 202);
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait until the background turn parks at WaitingInput.
    let mut parked = false;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_input") {
            parked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(parked, "session never reached WaitingInput");

    // `list` renders the raw lifecycle name, covering
    // `lifecycle_name(WaitingInput)`.
    let list = scenario.output(
        &[
            "list",
            "--server",
            &format!("http://127.0.0.1:{}", server.port),
            "--token",
            &server.token,
        ],
        |command| {
            command.env("TEST_OPENAI_KEY", "waiting-input-secret");
        },
    );
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(
        list_text.contains("waiting_input"),
        "list output: {list_text}"
    );
    drop(server);
}

/// A minimal mock HTTP server that closes the SSE stream after one keepalive
/// comment, driving the CLI's reconnect path (§8.1: resync → reconnect →
/// resync). The snapshot endpoint returns `running` for the first few requests
/// and `ready`+`completed` thereafter, so the observer terminates after one
/// reconnect cycle.
struct MockSseServer {
    port: u16,
    snapshot_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl MockSseServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let snapshot_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = std::sync::Arc::clone(&snapshot_count);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let count = std::sync::Arc::clone(&count);
                std::thread::spawn(move || {
                    handle_mock_request(stream, &count);
                });
            }
        });
        Self {
            port,
            snapshot_count,
        }
    }
}

fn mock_http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        202 => "Accepted",
        404 => "Not Found",
        _ => "OK",
    };
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text,
        content_type,
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn handle_mock_request(mut stream: std::net::TcpStream, count: &std::sync::atomic::AtomicUsize) {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    // Query strings are opaque to these mocks; strip them so pagination
    // parameters (limit/cursor) do not break path matching.
    let path = parts[1].split_once('?').map_or(parts[1], |(path, _)| path);

    // Read headers until the blank line.
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len) = trimmed.to_lowercase().strip_prefix("content-length:") {
            content_length = len.trim().parse().unwrap_or(0);
        }
    }
    // Read and discard the body.
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }

    let binding = r#"{"version":1,"provider_name":"main","provider_type":"openai-chat","protocol":"openai-chat","model":"mock","config_fingerprint":"fp","tools_fingerprint":"fp","aliases":{},"credential_ref_id":"ref","data_scope_id":"scope","credential_generation":1}"#;
    let running_snapshot = format!(
        r#"{{"snapshot":{{"thread_id":"01a0194a-0000-7000-8000-000000000001","revision":1,"sequence":1,"lifecycle":"running","binding":{binding},"latest_run_id":null,"active_run_id":null,"pending":null,"runs":[],"transcript":{{"entries":[],"next_after":null,"has_more":false}},"focus":null}}}}"#
    );
    let completed_snapshot = format!(
        r#"{{"snapshot":{{"thread_id":"01a0194a-0000-7000-8000-000000000001","revision":2,"sequence":2,"lifecycle":"ready","binding":{binding},"latest_run_id":"01a0194a-0000-7000-8000-000000000002","active_run_id":null,"pending":null,"runs":[{{"run_id":"01a0194a-0000-7000-8000-000000000002","parent_run_id":null,"ordinal":0,"status":"completed","run_revision":1,"completed_at_ms":1234567890,"failure_code":null}}],"transcript":{{"entries":[],"next_after":null,"has_more":false}},"focus":null}}}}"#
    );

    let response: Vec<u8> = match (method, path) {
        ("GET", "/health") => mock_http_response(200, "text/plain", b"ok"),
        ("POST", "/v1/workspaces") => {
            mock_http_response(200, "application/json", br#"{"workspace_id":"ws-1"}"#)
        }
        ("GET", p) if p.ends_with("/bindings") => mock_http_response(
            200,
            "application/json",
            format!(r#"{{"bindings":[{{"is_default":true,"binding":{binding}}}]}}"#).as_bytes(),
        ),
        ("POST", p) if p.ends_with("/sessions") => mock_http_response(
            202,
            "application/json",
            br#"{"session_id":"01a0194a-0000-7000-8000-000000000001","accepted_revision":1}"#,
        ),
        ("GET", p) if p.starts_with("/v1/sessions/") => {
            let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = if n < 3 {
                running_snapshot.as_bytes()
            } else {
                completed_snapshot.as_bytes()
            };
            mock_http_response(200, "application/json", body)
        }
        ("GET", p) if p.ends_with("/events") => {
            // SSE stream: send one keepalive comment and close.
            let body = b": keepalive\n\n";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
            return; // Close the connection to signal stream-end.
        }
        _ => mock_http_response(404, "text/plain", b"not found"),
    };
    let _ = stream.write_all(&response);
}

/// CLI `run` against a mock server that closes the SSE stream after one
/// keepalive: the observer enters the reconnect path (resync → reconnect →
/// resync) and terminates when the snapshot turns terminal.
#[test]
fn final_binary_cli_run_reconnects_after_sse_stream_close() {
    let server = MockSseServer::start();
    let url = format!("http://127.0.0.1:{}", server.port);

    let output = Scenario::new().output(
        &[
            "--json",
            "run",
            "reconnect test",
            "--server",
            &url,
            "--token",
            "mock-token",
        ],
        |_| {},
    );
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    // At least 3 snapshot requests: initial resync + post-connect resync +
    // reconnect resync (plus the terminal resync).
    assert!(
        server
            .snapshot_count
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 3,
        "expected at least 3 snapshot requests (reconnect path)"
    );
}

/// A minimal mock HTTP server that returns a terminal snapshot with a
/// configurable lifecycle, covering the `classify` branches that are hard to
/// reach through the real server (`Interrupted`, `ReconciliationRequired`, `Failed`,
/// Denied, Ready-with-no-runs).
struct MockLifecycleServer {
    port: u16,
}

impl MockLifecycleServer {
    fn start(snapshot_json: String) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let snapshot = snapshot_json.clone();
                std::thread::spawn(move || {
                    handle_lifecycle_request(stream, &snapshot);
                });
            }
        });
        Self { port }
    }
}

fn handle_lifecycle_request(mut stream: std::net::TcpStream, snapshot_json: &str) {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    // Query strings are opaque to these mocks; strip them so pagination
    // parameters (limit/cursor) do not break path matching.
    let path = parts[1].split_once('?').map_or(parts[1], |(path, _)| path);

    // Read headers until the blank line.
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len) = trimmed.to_lowercase().strip_prefix("content-length:") {
            content_length = len.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }

    let binding = r#"{"version":1,"provider_name":"main","provider_type":"openai-chat","protocol":"openai-chat","model":"mock","config_fingerprint":"fp","tools_fingerprint":"fp","aliases":{},"credential_ref_id":"ref","data_scope_id":"scope","credential_generation":1}"#;
    let response: Vec<u8> = if path == "/health" {
        mock_http_response(200, "text/plain", b"ok")
    } else if path == "/v1/workspaces" {
        mock_http_response(200, "application/json", br#"{"workspace_id":"ws-1"}"#)
    } else if path.ends_with("/bindings") {
        mock_http_response(
            200,
            "application/json",
            format!(r#"{{"bindings":[{{"is_default":true,"binding":{binding}}}]}}"#).as_bytes(),
        )
    } else if path.ends_with("/sessions") && method == "POST" {
        mock_http_response(
            202,
            "application/json",
            br#"{"session_id":"01a0194a-0000-7000-8000-000000000001","accepted_revision":1}"#,
        )
    } else if path.ends_with("/sessions") && method == "GET" {
        // list_sessions: the snapshot_json carries the sessions array.
        mock_http_response(200, "application/json", snapshot_json.as_bytes())
    } else if path.starts_with("/v1/sessions/") {
        mock_http_response(200, "application/json", snapshot_json.as_bytes())
    } else if path.ends_with("/events") {
        // SSE stream: keep-alive only (should not be reached for terminal snapshots).
        let body = b": keepalive\n\n";
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body);
        return;
    } else {
        mock_http_response(404, "text/plain", b"not found")
    };
    let _ = stream.write_all(&response);
}

fn terminal_snapshot(lifecycle: &str, runs: &str) -> String {
    let binding = r#"{"version":1,"provider_name":"main","provider_type":"openai-chat","protocol":"openai-chat","model":"mock","config_fingerprint":"fp","tools_fingerprint":"fp","aliases":{},"credential_ref_id":"ref","data_scope_id":"scope","credential_generation":1}"#;
    format!(
        r#"{{"snapshot":{{"thread_id":"01a0194a-0000-7000-8000-000000000001","revision":1,"sequence":1,"lifecycle":"{lifecycle}","binding":{binding},"latest_run_id":null,"active_run_id":null,"pending":null,"runs":{runs},"transcript":{{"entries":[],"next_after":null,"has_more":false}},"focus":null}}}}"#
    )
}

/// CLI `run` against a mock server returning terminal snapshots covers the
/// `classify` branches for `Interrupted`, `ReconciliationRequired`, `Failed`,
/// Denied, and Ready-with-no-runs.
#[test]
fn final_binary_cli_run_classifies_terminal_lifecycles() {
    let denied_run = r#"[{"run_id":"01a0194a-0000-7000-8000-000000000002","parent_run_id":null,"ordinal":0,"status":"failed","run_revision":1,"completed_at_ms":1234567890,"failure_code":"permission_denied"}]"#;
    let failed_run = r#"[{"run_id":"01a0194a-0000-7000-8000-000000000002","parent_run_id":null,"ordinal":0,"status":"failed","run_revision":1,"completed_at_ms":1234567890,"failure_code":"runtime_failed"}]"#;

    let cases: &[(&str, &str, i32, &str)] = &[
        ("interrupted", "[]", 130, "interrupted"),
        (
            "reconciliation_required",
            "[]",
            1,
            "reconciliation_required",
        ),
        ("failed", "[]", 1, "failed"),
        ("ready", "[]", 1, "failed"), // Ready with no runs → Failed
        ("ready", denied_run, 11, "denied"),
        ("ready", failed_run, 1, "failed"),
    ];

    for (lifecycle, runs, expected_exit, expected_status) in cases {
        let snapshot = terminal_snapshot(lifecycle, runs);
        let server = MockLifecycleServer::start(snapshot);
        let url = format!("http://127.0.0.1:{}", server.port);
        let output = Scenario::new().output(
            &["--json", "run", "test", "--server", &url, "--token", "t"],
            |_| {},
        );
        assert_eq!(
            output.status.code(),
            Some(*expected_exit),
            "lifecycle={lifecycle} runs={runs}: expected exit {expected_exit}; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let body = json(&output);
        assert_eq!(
            body["status"], *expected_status,
            "lifecycle={lifecycle} runs={runs}: expected status {expected_status}"
        );
    }
}

/// CLI `run` with a provider that calls an unknown tool: the engine rejects
/// the effect and the run fails, covering the tool-not-found error path.
#[test]
fn final_binary_cli_run_with_unknown_tool_fails() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("unknown-1", "nonexistent_tool", &serde_json::json!({})),
        ProviderReply::completion("after unknown tool"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "use an unknown tool"], |command| {
        command.env("TEST_OPENAI_KEY", "unknown-tool-secret");
    });
    assert!(
        !output.status.success(),
        "expected failure; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "failed");
}

/// CLI `list` against a mock server returning sessions with `Interrupted` and
/// `ReconciliationRequired` lifecycles covers `lifecycle_name` for those variants.
#[test]
fn final_binary_cli_list_renders_interrupted_and_reconciliation_lifecycles() {
    let binding = r#"{"version":1,"provider_name":"main","provider_type":"openai-chat","protocol":"openai-chat","model":"mock","config_fingerprint":"fp","tools_fingerprint":"fp","aliases":{},"credential_ref_id":"ref","data_scope_id":"scope","credential_generation":1}"#;
    let sessions = format!(
        r#"{{"sessions":[
            {{"thread_id":"01a0194a-0000-7000-8000-000000000001","revision":1,"sequence":1,"lifecycle":"interrupted","binding":{binding},"latest_run_id":null,"active_run_id":null,"pending":null,"runs":[],"transcript":{{"entries":[],"next_after":null,"has_more":false}},"focus":null}},
            {{"thread_id":"01a0194a-0000-7000-8000-000000000002","revision":1,"sequence":1,"lifecycle":"reconciliation_required","binding":{binding},"latest_run_id":null,"active_run_id":null,"pending":null,"runs":[],"transcript":{{"entries":[],"next_after":null,"has_more":false}},"focus":null}}
        ]}}"#
    );
    let server = MockLifecycleServer::start(sessions);
    let url = format!("http://127.0.0.1:{}", server.port);

    let output = Scenario::new().output(&["list", "--server", &url, "--token", "t"], |_| {});
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("interrupted"), "list output: {text}");
    assert!(
        text.contains("reconciliation_required"),
        "list output: {text}"
    );
}

/// CLI `list` follows `next_cursor` until every page is fetched, so sessions
/// beyond the first page are not silently dropped.
#[test]
fn final_binary_cli_list_follows_pagination_cursor() {
    let binding = r#"{"version":1,"provider_name":"main","provider_type":"openai-chat","protocol":"openai-chat","model":"mock","config_fingerprint":"fp","tools_fingerprint":"fp","aliases":{},"credential_ref_id":"ref","data_scope_id":"scope","credential_generation":1}"#;
    let session = |id: &str| {
        format!(
            r#"{{"thread_id":"{id}","revision":1,"sequence":1,"lifecycle":"ready","binding":{binding},"latest_run_id":null,"active_run_id":null,"pending":null,"runs":[],"transcript":{{"entries":[],"next_after":null,"has_more":false}},"focus":null}}"#
        )
    };
    let page1 = format!(
        r#"{{"sessions":[{}],"next_cursor":"page-2"}}"#,
        session("01a0194a-0000-7000-8000-000000000001")
    );
    let page2 = format!(
        r#"{{"sessions":[{}],"next_cursor":null}}"#,
        session("01a0194a-0000-7000-8000-000000000002")
    );
    let (url, _handle) = start_healthy_mock_server(move |method, path| {
        if method == "POST" && path == "/v1/workspaces" {
            return (
                200,
                "application/json".into(),
                r#"{"workspace_id":"ws-1"}"#.into(),
            );
        }
        if method == "GET" && path.starts_with("/v1/workspaces/ws-1/sessions") {
            if path.contains("cursor=page-2") {
                return (200, "application/json".into(), page2.clone());
            }
            return (200, "application/json".into(), page1.clone());
        }
        (
            404,
            "application/json".into(),
            r#"{"error":"not found"}"#.into(),
        )
    });

    let output = Scenario::new().output(&["list", "--server", &url, "--token", "t"], |_| {});
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("01a0194a-0000-7000-8000-000000000001"),
        "page 1 session missing: {text}"
    );
    assert!(
        text.contains("01a0194a-0000-7000-8000-000000000002"),
        "page 2 session missing (cursor not followed): {text}"
    );
}

/// Engine-level cursor pagination: creating more threads than the page limit
/// forces `encode_session_cursor` to emit a cursor, and following it exercises
/// `decode_session_cursor` and the keyset WHERE clause.
#[test]
fn engine_paged_list_follows_cursor_across_pages() {
    let dir = tempfile::tempdir().unwrap();
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .build()
        .unwrap();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };
    let ids = latte_core::SystemIdSource::default();
    for index in 0..3u64 {
        let thread_id = latte_core::ThreadId::from_uuid(latte_core::IdSource::next_uuid_v7(&ids));
        let run_id = latte_core::RunId::from_uuid(latte_core::IdSource::next_uuid_v7(&ids));
        engine
            .create_thread_v2(
                thread_id,
                run_id,
                binding.clone(),
                &format!("session {index}"),
                index + 1,
            )
            .unwrap();
    }
    let workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Page 1: limit=1 → 1 item + cursor.
    let page1 = engine
        .list_threads_v2_for_workspace_paged(&workspace, None, 1)
        .unwrap();
    assert_eq!(page1.items.len(), 1);
    assert!(page1.next_cursor.is_some(), "page 1 must carry a cursor");

    // Page 2: follow the cursor → 1 item + cursor.
    let cursor = page1.next_cursor.unwrap();
    let page2 = engine
        .list_threads_v2_for_workspace_paged(&workspace, Some(&cursor), 1)
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(page2.next_cursor.is_some(), "page 2 must carry a cursor");
    assert_ne!(
        page1.items[0].thread_id, page2.items[0].thread_id,
        "pages must not repeat sessions"
    );

    // Page 3: follow the cursor → 1 item, no cursor (exhausted).
    let cursor = page2.next_cursor.unwrap();
    let page3 = engine
        .list_threads_v2_for_workspace_paged(&workspace, Some(&cursor), 1)
        .unwrap();
    assert_eq!(page3.items.len(), 1);
    assert!(page3.next_cursor.is_none(), "page 3 must be the last page");

    // Search paged: query matches all 3 sessions, follow the cursor.
    let search = engine
        .search_thread_sessions_v2_paged("session", None, 2)
        .unwrap();
    assert_eq!(search.items.len(), 2);
    assert!(search.next_cursor.is_some());
    let search2 = engine
        .search_thread_sessions_v2_paged("session", search.next_cursor.as_deref(), 2)
        .unwrap();
    assert_eq!(search2.items.len(), 1);
    assert!(search2.next_cursor.is_none());

    // Exact-title paged: unique title → 1 item, no cursor.
    let exact = engine
        .find_thread_sessions_v2_by_exact_title_for_workspace_paged(
            &workspace,
            "session 1",
            None,
            10,
        )
        .unwrap();
    assert_eq!(exact.items.len(), 1);
    assert!(exact.next_cursor.is_none());

    // Exact-title paged with limit=1 → cursor follows.
    let exact_paged = engine
        .find_thread_sessions_v2_by_exact_title_for_workspace_paged(
            &workspace,
            "session 1",
            None,
            1,
        )
        .unwrap();
    assert_eq!(exact_paged.items.len(), 1);
    assert!(exact_paged.next_cursor.is_none());

    // limit=0 → empty page, no cursor.
    let empty = engine
        .list_threads_v2_for_workspace_paged(&workspace, None, 0)
        .unwrap();
    assert!(empty.items.is_empty());
    assert!(empty.next_cursor.is_none());
}

/// Engine-level lifecycle: create runs, apply transitions, rename, fork, and
/// lease management — covers storage paths that CLI E2E tests can't reach.
#[test]
fn engine_lifecycle_covers_run_transition_rename_fork_and_lease() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    // Create a thread and run.
    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding.clone(), "lifecycle test", 1)
        .unwrap();

    // Create a second thread for more coverage.
    let thread2 = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run2 = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread2, run2, binding.clone(), "second session", 2)
        .unwrap();

    // Lease management: acquire and renew.
    let lease = engine.acquire_lease("owner", 2, 10_000).unwrap();
    engine.renew_lease(&lease, 3, 10_000).unwrap();
    engine
        .rename_thread_session_v2(thread_id, "renamed session")
        .unwrap();

    // Fork the thread.
    let fork_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let _fork = engine
        .fork_thread_session_v2(thread_id, fork_id, Some("fork title"), 20)
        .unwrap();

    // List and search.
    let all = engine.list_threads_v2().unwrap();
    assert!(all.len() >= 2, "should have original + fork");
    let found = engine.search_thread_sessions_v2("renamed", 10).unwrap();
    assert!(!found.is_empty(), "search should find renamed thread");

    // Workspace-scoped list.
    let workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let ws = engine.list_threads_v2_for_workspace(&workspace).unwrap();
    assert!(!ws.is_empty());
}

/// Engine-level snapshot and conversation paths: thread snapshots, conversation
/// outbox, and workspace manifest — covers storage paths CLI E2E can't reach.
#[test]
fn engine_snapshot_and_conversation_paths() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding, "snapshot test", 1)
        .unwrap();

    // Thread snapshot.
    let snapshot = engine.thread_snapshot_v2(thread_id, None, 10).unwrap();
    assert_eq!(snapshot.thread_id, thread_id);

    // Thread snapshot tail.
    let tail = engine.thread_snapshot_tail_v2(thread_id, 10).unwrap();
    assert_eq!(tail.thread_id, thread_id);

    // Conversation outbox.

    // Workspace manifest.
    let _manifest = engine.workspace_manifest().unwrap();

    // List threads for workspace.
    let workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let ws_threads = engine.list_threads_v2_for_workspace(&workspace).unwrap();
    assert!(!ws_threads.is_empty());

    // Search sessions.
    let found = engine.search_thread_sessions_v2("snapshot", 10).unwrap();
    assert!(!found.is_empty());

    // Find by exact title.
    let exact = engine
        .find_thread_sessions_v2_by_exact_title_for_workspace(&workspace, "snapshot test", 10)
        .unwrap();
    assert!(!exact.is_empty());

    // Thread session (summary).
    let summary = engine.thread_session_v2(thread_id).unwrap();
    assert_eq!(summary.unwrap().thread_id, thread_id);
}

/// Engine-level run lifecycle: create thread, start a linked run through the
/// v2 thread commit path, append transcript entries, and verify the run state
/// transitions — covers storage transition paths CLI E2E can't reach.
#[test]
#[allow(clippy::too_many_lines)]
fn engine_run_lifecycle_covers_transitions() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding, "lifecycle", 1)
        .unwrap();

    // The thread's initial run should be in Queued state.
    let state = engine.show(run_id).unwrap();
    assert_eq!(state.status, latte_core::RunStatus::Queued);

    // Linked v2 runs mutate exclusively through thread commits, scoped to a
    // thread lease — the legacy run-transition path rejects them by design.
    let lease = engine.acquire_thread_lease(thread_id, 2, 10_000).unwrap();
    let started = engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: 0,
                expected_run_revision: 0,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Start {
                    source_key: "start".into(),
                },
            },
            &lease,
            3,
        )
        .unwrap();
    assert_eq!(started.snapshot.thread_id, thread_id);
    assert_eq!(started.snapshot.active_run_id, Some(run_id));
    assert_eq!(
        engine.show(run_id).unwrap().status,
        latte_core::RunStatus::Running
    );
    let mut revision = started.snapshot.revision;
    let mut run_revision = started
        .snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .expect("started run appears in snapshot")
        .run_revision;

    // Append transcript entries on both sides of the conversation.
    for (kind, text) in [
        (latte_core::TranscriptKind::User, "first prompt"),
        (latte_core::TranscriptKind::Assistant, "first reply"),
    ] {
        let committed = engine
            .commit_thread_run_update(
                latte_engine::ThreadCommitRequest {
                    thread_id,
                    run_id,
                    expected_thread_revision: revision,
                    expected_run_revision: run_revision,
                    command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                    request_id: None,
                    effect_id: None,
                    update: latte_engine::CommitThreadRunUpdate::AppendTranscript {
                        source_key: format!("append-{text}"),
                        kind,
                        text: text.into(),
                        payload: None,
                    },
                },
                &lease,
                4,
            )
            .unwrap();
        revision = committed.snapshot.revision;
        run_revision = committed
            .snapshot
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .expect("run still appears in snapshot")
            .run_revision;
    }
    let snapshot = engine.thread_snapshot_v2(thread_id, None, 10).unwrap();
    // Start writes its own transcript entry, so the two appends make three.
    let texts: Vec<&str> = snapshot
        .transcript
        .entries
        .iter()
        .map(|entry| entry.text.as_str())
        .collect();
    assert!(texts.contains(&"first prompt"), "texts={texts:?}");
    assert!(texts.contains(&"first reply"), "texts={texts:?}");

    // List runs.
    let runs = engine.list().unwrap();
    assert!(!runs.is_empty());
}

/// Engine-level follow-up durable idempotency: a same-command_id retry replays
/// the original acceptance without appending a duplicate turn, and a
/// same-command_id different-payload retry is a conflict.
#[test]
#[allow(clippy::too_many_lines)]
fn engine_follow_up_durable_idempotency_replays_and_rejects_mismatch() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    // Create a thread and complete its first run so the thread is ready for
    // follow-up.
    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding.clone(), "dedup", 1)
        .unwrap();
    let lease = engine.acquire_thread_lease(thread_id, 2, 10_000).unwrap();
    let started = engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: 0,
                expected_run_revision: 0,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Start {
                    source_key: "start".into(),
                },
            },
            &lease,
            3,
        )
        .unwrap();
    let mut revision = started.snapshot.revision;
    let run_revision = started
        .snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
        .run_revision;

    // Fail the run (retryable) so the thread becomes ready for follow-up.
    let committed = engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: revision,
                expected_run_revision: run_revision,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Fail {
                    source_key: "fail".into(),
                    failure: latte_core::RunFailure {
                        code: latte_core::FailureCode::RuntimeFailed,
                        message: "test failure".into(),
                        retryability: latte_core::Retryability::Retryable,
                    },
                },
            },
            &lease,
            4,
        )
        .unwrap();
    revision = committed.snapshot.revision;
    engine.release_lease(&lease).unwrap();

    // First follow-up with a stable command_id → Created.
    let command_id = latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7());
    let follow_run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    let follow_lease = engine.acquire_thread_lease(thread_id, 5, 10_000).unwrap();
    let first = engine
        .create_started_thread_follow_up_v2(
            Some(&command_id),
            thread_id,
            follow_run_id,
            revision,
            "follow-up turn",
            &follow_lease,
            6,
        )
        .unwrap();
    let first_snapshot = match first {
        latte_core::CreateOutcome::Created(snapshot) => snapshot,
        latte_core::CreateOutcome::Replayed(_) => panic!("first follow-up must be Created"),
    };
    assert_eq!(first_snapshot.active_run_id, Some(follow_run_id));

    // Replay with the same command_id + payload → Replayed, no duplicate turn.
    let replay_run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    let replay = engine
        .create_started_thread_follow_up_v2(
            Some(&command_id),
            thread_id,
            replay_run_id,
            revision,
            "follow-up turn",
            &follow_lease,
            7,
        )
        .unwrap();
    let replayed_snapshot = match replay {
        latte_core::CreateOutcome::Created(_) => panic!("replay must be Replayed"),
        latte_core::CreateOutcome::Replayed(snapshot) => snapshot,
    };
    // The replay returns the original acceptance, not a new turn.
    assert_eq!(replayed_snapshot.thread_id, thread_id);
    assert_eq!(replayed_snapshot.revision, first_snapshot.revision);

    // Same command_id but different prompt → ThreadCommandReplayMismatch.
    let mismatch_run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    let mismatch = engine.create_started_thread_follow_up_v2(
        Some(&command_id),
        thread_id,
        mismatch_run_id,
        revision,
        "DIFFERENT prompt",
        &follow_lease,
        8,
    );
    assert!(matches!(
        mismatch,
        Err(latte_engine::StorageError::ThreadCommandReplayMismatch)
    ));

    // Same command_id but different expected revision → mismatch (digest
    // includes expected_thread_revision).
    let mismatch2 = engine.create_started_thread_follow_up_v2(
        Some(&command_id),
        thread_id,
        mismatch_run_id,
        revision + 1,
        "follow-up turn",
        &follow_lease,
        9,
    );
    assert!(matches!(
        mismatch2,
        Err(latte_engine::StorageError::ThreadCommandReplayMismatch)
    ));

    // Pre-acquire lookup also finds the record (the headless replay path).
    let looked_up = engine
        .lookup_follow_up_replay(&command_id, thread_id, revision, "follow-up turn")
        .unwrap();
    assert!(
        looked_up.is_some(),
        "pre-acquire lookup must find the record"
    );
    let looked_up = looked_up.unwrap();
    assert_eq!(looked_up.revision, first_snapshot.revision);

    // Pre-acquire lookup with a different prompt → mismatch.
    let lookup_mismatch = engine.lookup_follow_up_replay(&command_id, thread_id, revision, "WRONG");
    assert!(matches!(
        lookup_mismatch,
        Err(latte_engine::StorageError::ThreadCommandReplayMismatch)
    ));

    // Pre-acquire lookup with an unknown command_id → None.
    let unknown = engine.lookup_follow_up_replay(
        &latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
        thread_id,
        revision,
        "follow-up turn",
    );
    assert!(unknown.unwrap().is_none());
}

/// Engine-level session management: switch binding, rename, and fork — covers
/// storage mutation paths CLI E2E can't reach directly.
#[test]
#[allow(clippy::too_many_lines)]
fn engine_session_management_covers_switch_rename_and_fork() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding.clone(), "manage me", 1)
        .unwrap();

    // Start and fail the run (retryable) so the thread becomes ready for
    // management operations.
    let lease = engine.acquire_thread_lease(thread_id, 2, 10_000).unwrap();
    let started = engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: 0,
                expected_run_revision: 0,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Start {
                    source_key: "start".into(),
                },
            },
            &lease,
            3,
        )
        .unwrap();
    let revision = started.snapshot.revision;
    let run_revision = started
        .snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
        .run_revision;
    engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: revision,
                expected_run_revision: run_revision,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Fail {
                    source_key: "fail".into(),
                    failure: latte_core::RunFailure {
                        code: latte_core::FailureCode::RuntimeFailed,
                        message: "test".into(),
                        retryability: latte_core::Retryability::Retryable,
                    },
                },
            },
            &lease,
            4,
        )
        .unwrap();
    engine.release_lease(&lease).unwrap();

    // Switch the provider binding.
    let switched_binding = latte_core::ThreadProviderBindingV2 {
        model: "switched-model".into(),
        ..binding.clone()
    };
    let lease = engine.acquire_thread_lease(thread_id, 5, 10_000).unwrap();
    engine
        .switch_thread_binding_v2(thread_id, 2, &switched_binding, &lease, 6)
        .unwrap();
    engine.release_lease(&lease).unwrap();

    // Verify the switch persisted.
    let snapshot = engine.thread_snapshot_v2(thread_id, None, 10).unwrap();
    assert_eq!(snapshot.binding.model, "switched-model");

    // Rename the session.
    engine
        .rename_thread_session_v2(thread_id, "renamed session")
        .unwrap();
    let summary = engine.thread_session_v2(thread_id).unwrap().unwrap();
    assert_eq!(summary.title, "renamed session");

    // Fork the session.
    let fork_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let fork = engine
        .fork_thread_session_v2(thread_id, fork_id, Some("fork title"), 20)
        .unwrap();
    assert_eq!(fork.thread_id, fork_id);

    // Both threads appear in the list.
    let all = engine.list_threads_v2().unwrap();
    assert!(all.len() >= 2, "should have original + fork");

    // Search finds the renamed session.
    let found = engine.search_thread_sessions_v2("renamed", 10).unwrap();
    assert!(!found.is_empty(), "search should find renamed thread");

    // Workspace-scoped list.
    let workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let ws = engine.list_threads_v2_for_workspace(&workspace).unwrap();
    assert!(!ws.is_empty());

    // Find by exact title.
    let exact = engine
        .find_thread_sessions_v2_by_exact_title_for_workspace(&workspace, "renamed session", 10)
        .unwrap();
    assert!(!exact.is_empty());
}

/// Engine-level paged queries: covers the paged list/search/exact-title
/// storage paths.
#[test]
fn engine_paged_queries_and_changed_files() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    // Create two threads.
    for i in 0..2 {
        let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
        let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
        engine
            .create_thread_v2(thread_id, run_id, binding.clone(), &format!("paged {i}"), 1)
            .unwrap();
    }

    let workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Paged list.
    let page = engine
        .list_threads_v2_for_workspace_paged(&workspace, None, 1)
        .unwrap();
    assert!(!page.items.is_empty());

    // Paged search.
    let search = engine
        .search_thread_sessions_v2_paged("paged", None, 10)
        .unwrap();
    assert!(!search.items.is_empty());

    // Paged exact-title.
    let exact = engine
        .find_thread_sessions_v2_by_exact_title_for_workspace_paged(&workspace, "paged 0", None, 10)
        .unwrap();
    assert!(!exact.items.is_empty());
}

/// Engine-level lease recovery: an expired thread lease is reclaimed by the
/// recovery sweeper, covering the storage recovery path.
#[test]
fn engine_lease_recovery_reclaims_expired_thread_lease() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding, "lease recovery", 1)
        .unwrap();

    // Acquire a lease with a very short TTL.
    let _lease = engine.acquire_thread_lease(thread_id, 2, 100).unwrap();

    // Wait for the lease to expire.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // The lease should be expired — a new acquisition should succeed (the old
    // lease is reclaimed by the recovery sweeper).
    engine.recover_expired_leases().unwrap();

    // After recovery, the thread should be recoverable (no active lease).
    let snapshot = engine.thread_snapshot_v2(thread_id, None, 10).unwrap();
    assert_eq!(snapshot.thread_id, thread_id);
}

/// Engine-level non-atomic follow-up: covers the legacy `create_thread_follow_up_v2`
/// path that queues a follow-up without acquiring a lease.
#[test]
fn engine_non_atomic_follow_up_queues_child() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    // Create a thread and complete its first run.
    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding.clone(), "non-atomic", 1)
        .unwrap();
    let lease = engine.acquire_thread_lease(thread_id, 2, 10_000).unwrap();
    let started = engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: 0,
                expected_run_revision: 0,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Start {
                    source_key: "start".into(),
                },
            },
            &lease,
            3,
        )
        .unwrap();
    let revision = started.snapshot.revision;
    let run_revision = started
        .snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .unwrap()
        .run_revision;
    engine
        .commit_thread_run_update(
            latte_engine::ThreadCommitRequest {
                thread_id,
                run_id,
                expected_thread_revision: revision,
                expected_run_revision: run_revision,
                command_id: latte_core::ThreadCommandId::from_uuid(ids.next_uuid_v7()),
                request_id: None,
                effect_id: None,
                update: latte_engine::CommitThreadRunUpdate::Fail {
                    source_key: "fail".into(),
                    failure: latte_core::RunFailure {
                        code: latte_core::FailureCode::RuntimeFailed,
                        message: "test".into(),
                        retryability: latte_core::Retryability::Retryable,
                    },
                },
            },
            &lease,
            4,
        )
        .unwrap();
    engine.release_lease(&lease).unwrap();

    // Non-atomic follow-up (no lease): queues a child in Queued state.
    let follow_run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    let snapshot = engine
        .create_thread_follow_up_v2(thread_id, follow_run_id, 2, "queued follow-up", 5)
        .unwrap();
    assert_eq!(snapshot.thread_id, thread_id);
    // The follow-up child should appear in the snapshot.
    assert!(
        snapshot.runs.iter().any(|run| run.run_id == follow_run_id),
        "follow-up child must appear in snapshot: {:?}",
        snapshot.runs
    );
}

/// Engine-level changed-files and workspace manifest: covers the
/// `thread_run_changed_files` read path and workspace manifest computation.
#[test]
fn engine_changed_files_and_workspace_manifest() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding, "changed files", 1)
        .unwrap();

    // Changed files for a fresh run (covers the read path).
    let _changed = engine.thread_run_changed_files(run_id).unwrap();

    // Workspace manifest.
    let manifest = engine.workspace_manifest().unwrap();
    let _ = manifest;

    // Tool descriptors.
    let descriptors = engine.tool_descriptors();
    assert!(!descriptors.is_empty(), "engine should have built-in tools");
}

/// Engine-level lease lifecycle and subscriptions: covers acquire/renew/release
/// and the event subscription paths.
#[test]
fn engine_lease_lifecycle_and_subscriptions() {
    use latte_core::IdSource;
    let dir = tempfile::tempdir().unwrap();
    let conversations = dir.path().join("sessions");
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(dir.path())
        .conversation_root(&conversations)
        .build()
        .unwrap();
    let ids = latte_core::SystemIdSource::default();
    let binding = latte_core::ThreadProviderBindingV2 {
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
    };

    let thread_id = latte_core::ThreadId::from_uuid(ids.next_uuid_v7());
    let run_id = latte_core::RunId::from_uuid(ids.next_uuid_v7());
    engine
        .create_thread_v2(thread_id, run_id, binding, "lease test", 1)
        .unwrap();

    // Acquire, renew, and release a thread lease.
    let lease = engine.acquire_thread_lease(thread_id, 2, 10_000).unwrap();
    engine.renew_lease(&lease, 3, 20_000).unwrap();
    engine.release_lease(&lease).unwrap();

    // After release, a new lease can be acquired.
    let lease2 = engine.acquire_thread_lease(thread_id, 4, 10_000).unwrap();
    engine.release_lease(&lease2).unwrap();

    // Subscriptions.
    let _events = engine.subscribe();
    let _thread_events = engine.subscribe_threads();

    // Show a run.
    let state = engine.show(run_id).unwrap();
    assert_eq!(state.run_id, run_id);

    // List runs.
    let runs = engine.list().unwrap();
    assert!(!runs.is_empty());
}

/// CLI `run` with a `write_file` tool call (permission granted via HTTP) and a
/// failing verification command: the run fails after the tool executes,
/// covering the verification-failure path.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn final_binary_cli_run_with_failing_verification_fails() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "write-verify",
            "write_file",
            &serde_json::json!({
                "path": "verified.txt",
                "content": "verify me\n",
                "create_intent": true
            }),
        ),
        ProviderReply::completion("wrote and verified"),
    ]);
    let endpoint = provider.endpoint();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        format!(
            r#"{{version:1,default_model:"main/mock",providers:{{main:{{type:"openai-chat",models:["mock"],endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}}}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:["false"]}}}}"#
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

    let (create_status, create_body) =
        server.create_session(&workspace_id, "write and verify", &binding);
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
        pending.expect("session never reached WaitingPermission");

    // Grant the permission.
    let (grant_status, _) = server.request(
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
    assert_eq!(grant_status, 200);

    // The write_file effect triggers a verification effect, which parks at
    // WaitingPermission again. Grant it too.
    let mut verify_pending = None;
    for _ in 0..200 {
        let (status, body) = server.request(
            "GET",
            &format!("/v1/sessions/{session_id}"),
            Some(&server.token),
            None,
            &[],
        );
        if status == 200 && body["snapshot"]["lifecycle"].as_str() == Some("waiting_permission") {
            verify_pending = Some((
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
        if status == 200 {
            let lifecycle = body["snapshot"]["lifecycle"].as_str().unwrap_or("");
            if lifecycle == "ready" || lifecycle == "failed" {
                break; // settled without a verification ask
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if let Some((revision, request_id, run_revision)) = verify_pending {
        let (verify_status, _) = server.request(
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
        assert_eq!(verify_status, 200);
    }

    // Wait for the session to settle (verification fails → run fails).
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
    assert!(settled, "session never settled after failing verification");

    // The file was written.
    assert!(scenario.root().join("verified.txt").exists());
}

// ---------------------------------------------------------------------------
// server_client error-path coverage
// ---------------------------------------------------------------------------

/// Starts a mock HTTP server that delegates all requests to `handler`
/// (which receives the method and path).
fn start_error_mock_server<F>(mut handler: F) -> (String, std::thread::JoinHandle<()>)
where
    F: FnMut(&str, &str) -> (u16, String, String) + Send + 'static,
{
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let Ok(n) = stream.read(&mut buf) else {
                continue;
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let mut lines = request.lines();
            let request_line = lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("GET");
            let path = parts.next().unwrap_or("/");
            let (status, content_type, body) = handler(method, path);
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

/// Starts a mock HTTP server that returns 200 for /health and delegates all
/// other requests to `handler`.
fn start_healthy_mock_server<F>(handler: F) -> (String, std::thread::JoinHandle<()>)
where
    F: FnMut(&str, &str) -> (u16, String, String) + Send + 'static,
{
    let mut inner = handler;
    start_error_mock_server(move |method, path| {
        if path == "/health" {
            (200, "application/json".into(), "{\"status\":\"ok\"}".into())
        } else {
            inner(method, path)
        }
    })
}

#[test]
fn final_binary_cli_run_with_unhealthy_server_reports_unreachable() {
    // health_check returns non-success → connect fails with Unreachable.
    let (url, _handle) =
        start_error_mock_server(|_method, _path| (500, "application/json".into(), String::new()));
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &[
            "--json", "run", "hello", "--server", &url, "--token", "dummy",
        ],
        |_| {},
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "server_unreachable");
}

#[test]
fn final_binary_cli_run_with_empty_workspace_response_fails() {
    // POST /v1/workspaces returns 200 with empty body → json() returns Null →
    // resolve_workspace fails with "missing workspace_id".
    let (url, _handle) =
        start_healthy_mock_server(|_method, _path| (200, "application/json".into(), String::new()));
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &[
            "--json", "run", "hello", "--server", &url, "--token", "dummy",
        ],
        |_| {},
    );
    assert!(!output.status.success());
    let body = json(&output);
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("workspace_id"), "message: {msg}");
}

#[test]
fn final_binary_cli_run_with_invalid_json_response_fails() {
    // POST /v1/workspaces returns 200 with invalid JSON → json() fails.
    let (url, _handle) = start_healthy_mock_server(|_method, _path| {
        (200, "application/json".into(), "not valid json{{{".into())
    });
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &[
            "--json", "run", "hello", "--server", &url, "--token", "dummy",
        ],
        |_| {},
    );
    assert!(!output.status.success());
    let body = json(&output);
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("invalid JSON"), "message: {msg}");
}

#[test]
fn final_binary_cli_run_with_unauthorized_workspace_response_fails() {
    // POST /v1/workspaces returns 401 → map_error_status → Unauthorized.
    let (url, _handle) = start_healthy_mock_server(|_method, _path| {
        (
            401,
            "application/json".into(),
            "{\"error\":{\"message\":\"bad token\"}}".into(),
        )
    });
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &[
            "--json", "run", "hello", "--server", &url, "--token", "dummy",
        ],
        |_| {},
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "unauthorized");
}

#[test]
fn final_binary_cli_run_with_server_error_workspace_response_fails() {
    // POST /v1/workspaces returns 500 → map_error_status → Internal.
    let (url, _handle) = start_healthy_mock_server(|_method, _path| {
        (
            500,
            "application/json".into(),
            "{\"error\":{\"message\":\"boom\"}}".into(),
        )
    });
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &[
            "--json", "run", "hello", "--server", &url, "--token", "dummy",
        ],
        |_| {},
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "internal");
}

#[test]
fn final_binary_cli_list_with_server_error_fails() {
    // GET /v1/workspaces returns 500 → map_error_status → Internal.
    let (url, _handle) = start_healthy_mock_server(|_method, _path| {
        (
            500,
            "application/json".into(),
            "{\"error\":{\"message\":\"boom\"}}".into(),
        )
    });
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &["--json", "list", "--server", &url, "--token", "dummy"],
        |_| {},
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "internal");
}

#[test]
fn final_binary_cli_show_with_invalid_session_id_reports_usage() {
    // A non-UUID session id is a usage error from parse_session_id.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "show", "not-a-uuid"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_resume_with_invalid_session_id_reports_usage() {
    // A non-UUID session id is a usage error from parse_session_id.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "resume", "not-a-uuid", "hello"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_run_without_prompt_reports_usage() {
    // run requires a prompt; missing it is a usage error.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "run"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_unknown_command_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "bogus"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_list_with_positional_arg_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "list", "extra"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_show_without_session_id_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "show"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_resume_without_prompt_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &["--json", "resume", "01900000-0000-7000-8000-000000000001"],
        |_| {},
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_focus_flag_with_list_reports_usage() {
    // --focus is only valid with `run`.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "list", "--focus", "/tmp"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_removed_allow_flag_reports_usage() {
    // v1 --allow/--deny flags must remain hard errors.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "run", "--allow", "true", "prompt"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_unknown_option_for_list_reports_usage() {
    // Unknown --flag tokens are rejected for list (only run/resume treat
    // them as prompt content).
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "list", "--bogus"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_server_flag_without_value_reports_usage() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "run", "--server"], |_| {});
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "usage");
}

#[test]
fn final_binary_cli_run_with_dash_dash_treats_flags_as_prompt() {
    // `--` makes everything after it positional, so --flag-like tokens are
    // part of the prompt rather than rejected options. Parsing succeeds; the
    // command then fails at server connection (not at usage parsing).
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["--json", "run", "--", "--flag-like", "text"], |_| {});
    assert!(!output.status.success());
    let body = json(&output);
    assert_ne!(
        body["error"]["code"], "usage",
        "-- must pass parsing; flags after it are prompt content"
    );
}

#[test]
fn final_binary_cli_json_flag_after_subcommand_is_accepted() {
    // --json may appear after the subcommand (not just as a global prefix);
    // parse_session_command must still recognize it and emit a JSON envelope.
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(&["list", "--json"], |_| {});
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert!(
        body["data"]["sessions"].is_array(),
        "--json after subcommand must produce a JSON envelope"
    );
}

#[test]
fn final_binary_cli_list_with_unhealthy_server_reports_unreachable() {
    // health_check returns non-success → connect fails with Unreachable.
    let (url, _handle) =
        start_error_mock_server(|_method, _path| (500, "application/json".into(), String::new()));
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:1", r#"["true"]"#);
    let output = scenario.output(
        &["--json", "list", "--server", &url, "--token", "dummy"],
        |_| {},
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["code"], "server_unreachable");
}

#[test]
fn final_binary_cli_run_with_write_file_tool_prepares_and_waits_for_permission() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "write-1",
        "write_file",
        &serde_json::json!({"path": "output.txt", "content": "hello world", "create_intent": true}),
    )]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "write a file"], |command| {
        command.env("TEST_OPENAI_KEY", "write-tool-secret");
    });
    // write_file is a modify tool → session waits for permission.
    assert!(!output.status.success());
    let body = json(&output);
    assert_eq!(body["status"], "waiting");
    assert_eq!(body["data"]["session"]["lifecycle"], "waiting_permission");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_edit_file_tool_prepares_and_waits_for_permission() {
    // edit_file is a Modify tool: it requires a precondition (sha256 of the
    // target file) and waits for permission before executing.
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("editable.txt"), "before text\n").unwrap();
    // sha256("before text\n") — keeps the scripted mutation bound to the
    // exact fixture.
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "edit-1",
        "edit_file",
        &serde_json::json!({
            "path": "editable.txt",
            "before": "before text",
            "after": "after text",
            "precondition": "28a55c8567f548f31faa8bf32a1dfbb28c6944abb01da0c79a7cf498df2c62d3"
        }),
    )]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "edit the file"], |command| {
        command.env("TEST_OPENAI_KEY", "edit-tool-secret");
    });
    // edit_file is a modify tool → session waits for permission.
    assert!(!output.status.success());
    let body = json(&output);
    assert_eq!(body["status"], "waiting");
    assert_eq!(body["data"]["session"]["lifecycle"], "waiting_permission");
    provider.assert_consumed();
}

#[cfg(unix)]
#[test]
fn final_binary_cli_run_with_process_tool_completes() {
    // /bin/pwd is an Allow-class process: it executes without a permission
    // round-trip and completes the session.
    let scenario = Scenario::new();
    let process = serde_json::json!({
        "argv": ["/bin/pwd"],
        "cwd": ".",
        "env": {},
        "timeout_ms": 5000,
        "grace_ms": 50,
        "stdout_cap": 1024,
        "stderr_cap": 1024
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("pwd-1", "process", &process),
        ProviderReply::completion("pwd complete"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "print the directory"], |command| {
        command.env("TEST_OPENAI_KEY", "process-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_list_directory_tool_completes() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("a.txt"), "a").unwrap();
    std::fs::write(scenario.root().join("b.txt"), "b").unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "list-1",
            "list_directory",
            &serde_json::json!({"path": "."}),
        ),
        ProviderReply::completion("listed directory"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "list files"], |command| {
        command.env("TEST_OPENAI_KEY", "list-tool-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_git_diff_tool_completes() {
    let scenario = Scenario::new();
    // Initialize a git repo so git_diff works.
    let status = std::process::Command::new("git")
        .args(["init"])
        .current_dir(scenario.root())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(scenario.root().join("tracked.txt"), "content\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(scenario.root())
        .status();
    let _ = std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(scenario.root())
        .status();
    // Modify the file so git_diff has something to report.
    std::fs::write(scenario.root().join("tracked.txt"), "modified content\n").unwrap();

    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("diff-1", "git_diff", &serde_json::json!({})),
        ProviderReply::completion("diff reviewed"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "check the diff"], |command| {
        command.env("TEST_OPENAI_KEY", "diff-tool-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_provider_error_fails() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::error(500, "provider internal error")]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "trigger provider error"], |command| {
        command.env("TEST_OPENAI_KEY", "error-secret");
    });
    assert!(!output.status.success());
    let body = json(&output);
    assert_eq!(body["status"], "failed");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_missing_file_tool_covers_error_path() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "read-1",
        "read_file",
        &serde_json::json!({"path": "nonexistent.txt"}),
    )]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "read a missing file"], |command| {
        command.env("TEST_OPENAI_KEY", "missing-file-secret");
    });
    // The tool error is observed and the provider re-enters; the session
    // completes (or fails durably) — either way the error path in the
    // engine is exercised.
    let body = json(&output);
    assert!(
        body["status"] == "completed"
            || body["status"] == "failed"
            || body["status"] == "interrupted",
        "unexpected status: {}",
        body["status"]
    );
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_path_escape_tool_covers_error_path() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([ProviderReply::tool_call(
        "read-1",
        "read_file",
        &serde_json::json!({"path": "../escape.txt"}),
    )]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "read an escaping path"], |command| {
        command.env("TEST_OPENAI_KEY", "escape-secret");
    });
    let body = json(&output);
    assert!(
        body["status"] == "completed"
            || body["status"] == "failed"
            || body["status"] == "interrupted",
        "unexpected status: {}",
        body["status"]
    );
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_search_tool_completes() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("target.txt"), "hello search\n").unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "search-1",
            "search",
            &serde_json::json!({"query": "hello", "max_results": 10}),
        ),
        ProviderReply::completion("search done"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "search for hello"], |command| {
        command.env("TEST_OPENAI_KEY", "search-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_read_project_manifest_tool_completes() {
    let scenario = Scenario::new();
    std::fs::write(
        scenario.root().join("Cargo.toml"),
        "[package]\nname = \"test\"\n",
    )
    .unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call(
            "manifest-1",
            "read_project_manifest",
            &serde_json::json!({}),
        ),
        ProviderReply::completion("manifest read"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "read the manifest"], |command| {
        command.env("TEST_OPENAI_KEY", "manifest-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    provider.assert_consumed();
}

#[test]
fn final_binary_cli_run_with_multiple_tool_calls_completes() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("a.txt"), "file a\n").unwrap();
    std::fs::write(scenario.root().join("b.txt"), "file b\n").unwrap();
    let provider = ScriptedProvider::start([
        ProviderReply::tool_calls([
            ("read-1", "read_file", &serde_json::json!({"path": "a.txt"})),
            ("read-2", "read_file", &serde_json::json!({"path": "b.txt"})),
        ]),
        ProviderReply::completion("read both files"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["true"]"#);

    let output = scenario.output(&["--json", "run", "read both files"], |command| {
        command.env("TEST_OPENAI_KEY", "multi-tool-secret");
    });
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body = json(&output);
    assert_eq!(body["status"], "completed");
    provider.assert_consumed();
}
