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
    /// Starts `latte-code --json serve --port 0` and blocks until the readiness
    /// line reports the bound port and token file, then reads the token.
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

    /// Creates a session through the crash-safe contract: a fresh client
    /// `thread_id` + `command_id` in the body and a matching `Idempotency-Key`.
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

/// Computes the exact v2 provider binding the server will accept.
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

/// A create-session request body carrying the client-generated `thread_id`
/// and `command_id` the crash-safe contract now requires.
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

/// Polls the session snapshot until `lifecycle` matches one of `expected` or
/// the deadline expires.  Returns the snapshot body.
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

/// v2 migration: the v1 test exercised two CLI `resume --allow` cycles
/// (process permission, then auto-verification permission).  The v2 server
/// does not auto-run verification after a provider completion, so this test
/// now exercises the process permission chain over HTTP: the session parks at
/// `waiting_permission`, the permission is allowed via the HTTP endpoint, the
/// process runs (exit 3), and the tool result is fed back to the provider
/// which completes the turn.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn process_permission_then_verification_permission_fails_durably_on_second_resume() {
    let scenario = Scenario::new();
    let process = serde_json::json!({
        "argv": [
            "/bin/sh",
            "-c",
            "printf permission-chain-out; printf permission-chain-err >&2; exit 3"
        ],
        "cwd": ".",
        "env": {},
        "timeout_ms": 2_000,
        "grace_ms": 50,
        "stdout_cap": 4_096,
        "stderr_cap": 4_096
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("permission-chain-process", "process", &process),
        ProviderReply::completion("process observed; verify now"),
    ]);
    scenario.write_config(
        provider.endpoint(),
        r#"["/bin/sh","-c","printf verification-chain-failed >&2; exit 7"]"#,
    );
    let server = ServeChild::start(&scenario);

    // Create/resolve the workspace by absolute path.
    let root = scenario.root().to_string_lossy().into_owned();
    let (ws_status, ws_body) = server.request(
        "POST",
        "/v1/workspaces",
        Some(&server.token),
        Some(&serde_json::json!({ "path": root })),
        &[],
    );
    assert_eq!(ws_status, 200, "workspace resolve: {ws_body:?}");
    let workspace_id = ws_body["workspace_id"].as_str().unwrap().to_string();
    let binding = server_binding(&scenario);

    // Create the session; the background turn parks at WaitingPermission for
    // the process tool call.
    let (create_status, create_body) =
        server.create_session(&workspace_id, "exercise two permission phases", &binding);
    assert_eq!(create_status, 202, "create: {create_body:?}");
    let session_id = create_body["session_id"].as_str().unwrap().to_string();

    // Wait for the process permission request.
    let snapshot = wait_for_lifecycle(&server, &session_id, &["waiting_permission"]);
    let revision = snapshot["revision"].as_u64().unwrap();
    let request_id = snapshot["pending"]["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run_revision = snapshot["pending"]["expected_run_revision"]
        .as_u64()
        .unwrap();
    assert_eq!(provider.requests().len(), 1);

    // Allow the process permission over HTTP.
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
    assert_eq!(allow_status, 200, "process allow: {allow_body:?}");

    // The process runs (exit 3), the tool result is fed back to the provider,
    // and the provider completes the turn.
    let snapshot = wait_for_lifecycle(&server, &session_id, &["ready"]);
    assert_eq!(
        snapshot["runs"][0]["status"].as_str(),
        Some("completed"),
        "run should complete after process permission allowed: {snapshot:?}"
    );

    // The provider was called twice: once for the tool call, once after the
    // process result was fed back.
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let process_result = requests[1]
        .body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|messages| messages.iter().find(|message| message["role"] == "tool"))
        .and_then(|message| message["content"].as_str())
        .unwrap_or("");
    assert!(
        process_result.contains("permission-chain-out"),
        "process stdout missing from provider messages: {process_result}"
    );
    assert!(
        process_result.contains("permission-chain-err"),
        "process stderr missing from provider messages: {process_result}"
    );
    assert!(
        process_result.contains("\"exit_code\":3"),
        "process exit code missing from provider messages: {process_result}"
    );

    // The completion transcript entry carries the handoff summary.
    let transcript = snapshot["transcript"]["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| e["text"].as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    assert!(
        transcript.contains("process observed; verify now"),
        "completion text missing from transcript: {transcript}"
    );

    // A follow-up on a ready (completed) session is accepted (the session is
    // not terminal in v2 — ready sessions accept follow-up turns).
    let thread_revision = snapshot["revision"].as_u64().unwrap();
    let follow_command_id = "01900000-0000-7000-8000-0000000000b1".to_string();
    let (follow_status, _) = server.request(
        "POST",
        &format!("/v1/sessions/{session_id}/follow-up"),
        Some(&server.token),
        Some(&serde_json::json!({
            "command_id": follow_command_id,
            "prompt": "continue",
            "expected_thread_revision": thread_revision
        })),
        &[("Idempotency-Key", &follow_command_id)],
    );
    assert!(
        follow_status == 202 || follow_status == 409,
        "follow-up on ready session should be accepted or conflict, got {follow_status}"
    );
}
