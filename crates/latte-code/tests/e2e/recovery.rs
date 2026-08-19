use super::{
    headless::{assert_tool_result_reached_provider, write_file_reply},
    support::{ProviderReply, Scenario, ScriptedProvider, json},
};

/// A supervised `latte-code serve` child bound to an ephemeral loopback port.
/// v2 permission decisions go over HTTP (`POST
/// /v1/sessions/{id}/permissions/{req_id}`), not the removed `resume
/// --allow/--deny` CLI, so tests that need to resolve a permission drive this
/// standalone server and speak the HTTP contract directly. Dropping it
/// terminates the process group so no server survives the test.
pub(super) struct ServerChild {
    child: std::process::Child,
    port: u16,
    token: String,
    #[cfg(unix)]
    process_group: i32,
}

impl ServerChild {
    pub(super) fn start(scenario: &Scenario) -> Self {
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

    pub(super) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub(super) fn token(&self) -> &str {
        &self.token
    }

    /// Issues one framed HTTP/1.1 request over loopback and returns the
    /// parsed (status, JSON body) pair. The Bearer token is attached
    /// automatically.
    pub(super) fn request(
        &self,
        method: &str,
        path: &str,
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
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAuthorization: Bearer {}\r\n",
            self.token
        );
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

    /// Resolves (creating if needed) the workspace id for the scenario root.
    pub(super) fn create_workspace(&self, scenario: &Scenario) -> String {
        let root = scenario.root().to_string_lossy().into_owned();
        let (status, body) = self.request(
            "POST",
            "/v1/workspaces",
            Some(&serde_json::json!({ "path": root })),
            &[],
        );
        assert_eq!(status, 200, "create workspace: {body:?}");
        body["workspace_id"].as_str().unwrap().to_string()
    }

    /// Creates a session through the crash-safe contract: a fresh client
    /// `thread_id` + `command_id` in the body and a matching `Idempotency-Key`.
    pub(super) fn create_session(
        &self,
        workspace_id: &str,
        prompt: &str,
        binding: &serde_json::Value,
    ) -> String {
        let thread_id = latte_core::ThreadId::from_uuid(uuid::Uuid::now_v7()).to_string();
        let command_id = latte_core::ThreadCommandId::from_uuid(uuid::Uuid::now_v7()).to_string();
        let body = serde_json::json!({
            "thread_id": thread_id,
            "command_id": command_id,
            "prompt": prompt,
            "binding": binding,
        });
        let (status, resp) = self.request(
            "POST",
            &format!("/v1/workspaces/{workspace_id}/sessions"),
            Some(&body),
            &[("Idempotency-Key", &command_id)],
        );
        assert_eq!(status, 202, "create session: {resp:?}");
        resp["session_id"].as_str().unwrap().to_string()
    }

    /// Fetches the authoritative session snapshot.
    pub(super) fn snapshot(&self, session_id: &str) -> serde_json::Value {
        let (status, body) = self.request("GET", &format!("/v1/sessions/{session_id}"), None, &[]);
        assert_eq!(status, 200, "snapshot: {body:?}");
        body["snapshot"].clone()
    }

    /// Polls until the session parks at `WaitingPermission`, returning the
    /// (thread revision, request id, run revision) needed for the decision.
    pub(super) fn wait_for_permission(&self, session_id: &str) -> (u64, String, u64) {
        for _ in 0..200 {
            let snapshot = self.snapshot(session_id);
            if snapshot["lifecycle"].as_str() == Some("waiting_permission") {
                return (
                    snapshot["revision"].as_u64().unwrap(),
                    snapshot["pending"]["request_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                    snapshot["pending"]["expected_run_revision"]
                        .as_u64()
                        .unwrap(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("session {session_id} never reached WaitingPermission");
    }

    /// Resolves a pending permission request over HTTP.
    pub(super) fn resolve_permission(
        &self,
        session_id: &str,
        request_id: &str,
        revision: u64,
        run_revision: u64,
        allow: bool,
    ) -> (u16, serde_json::Value) {
        self.request(
            "POST",
            &format!("/v1/sessions/{session_id}/permissions/{request_id}"),
            Some(&serde_json::json!({
                "allow": allow,
                "expected_thread_revision": revision,
                "expected_run_revision": run_revision,
            })),
            &[],
        )
    }

    /// Polls until the session reaches a terminal lifecycle, returning the
    /// snapshot.
    pub(super) fn wait_for_terminal(&self, session_id: &str) -> serde_json::Value {
        for _ in 0..300 {
            let snapshot = self.snapshot(session_id);
            let lifecycle = snapshot["lifecycle"].as_str().unwrap_or("");
            if !matches!(
                lifecycle,
                "running" | "waiting_permission" | "waiting_input"
            ) {
                return snapshot;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("session {session_id} never reached a terminal state");
    }
}

impl Drop for ServerChild {
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

/// Computes the exact v2 provider binding the server will accept, mirroring
/// what a co-located client does: load the same layered config and derive the
/// binding against the workspace engine's tool descriptors.
pub(super) fn server_binding(scenario: &Scenario) -> serde_json::Value {
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

/// Opens a fresh engine against the shared durable store so a test can assert
/// engine-visible state (effect status, checkpoints) after the server exits.
fn engine_for(scenario: &Scenario) -> latte_engine::EngineHandle {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

/// CLI `show`/`list` against a standalone server, exercising the v2 CLI
/// contract (`data.session` / `data.sessions[]`) end to end.
fn cli_show(scenario: &Scenario, server: &ServerChild, session_id: &str) -> std::process::Output {
    scenario.output(
        &[
            "--json",
            "show",
            session_id,
            "--server",
            &server.url(),
            "--token",
            server.token(),
        ],
        |_| {},
    )
}

fn cli_list(scenario: &Scenario, server: &ServerChild) -> std::process::Output {
    scenario.output(
        &[
            "--json",
            "list",
            "--server",
            &server.url(),
            "--token",
            server.token(),
        ],
        |_| {},
    )
}

/// Finds the completion transcript card's handoff evidence, if any.
fn completion_evidence(
    snapshot: &serde_json::Value,
) -> Option<&Vec<serde_json::Value>> {
    snapshot["transcript"]["entries"]
        .as_array()?
        .iter()
        .find(|entry| entry["kind"] == "completion")
        .and_then(|entry| entry["payload"]["handoff"]["evidence"].as_array())
}

#[test]
#[allow(clippy::too_many_lines)]
fn run_waiting_resume_allow_and_deny_are_durable_across_processes() {
    // Allow journey: a write_file permission is resolved over HTTP, the effect
    // runs, verification auto-allows, and the session completes durably.
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        write_file_reply("allow-write"),
        ProviderReply::completion("done"),
    ]);
    scenario.write_config_with_database(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "state/nested/custom.db",
    );
    let server = ServerChild::start(&scenario);
    let workspace = server.create_workspace(&scenario);
    let binding = server_binding(&scenario);
    let session_id = server.create_session(&workspace, "finish safely", &binding);
    let (revision, request_id, run_revision) = server.wait_for_permission(&session_id);
    let (allow_status, allow_body) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, true);
    assert_eq!(allow_status, 200, "allow: {allow_body:?}");
    let terminal = server.wait_for_terminal(&session_id);
    assert_eq!(terminal["lifecycle"], "ready");
    assert_eq!(terminal["runs"][0]["status"], "completed");

    // The completed session is visible through the v2 CLI show/list contract.
    let shown = cli_show(&scenario, &server, &session_id);
    assert!(
        shown.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&shown.stdout),
        String::from_utf8_lossy(&shown.stderr)
    );
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "completed"
    );
    let listed = cli_list(&scenario, &server);
    assert!(listed.status.success());
    assert!(
        json(&listed)["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["thread_id"] == session_id)
    );
    assert!(!scenario.root().join("state/nested/custom.db").exists());
    assert!(scenario.database_path().exists());
    provider.assert_consumed();

    // Deny journey: a denied write_file permission fails the run durably with
    // permission_denied, the effect is observed-failed, and no checkpoint
    // survives that could resume the child.
    let denied_scenario = Scenario::new();
    let denied_provider = ScriptedProvider::start([write_file_reply("deny-write")]);
    denied_scenario.write_config_with_database(
        denied_provider.endpoint(),
        r#"["/bin/pwd"]"#,
        "state/deny.db",
    );
    let denied_server = ServerChild::start(&denied_scenario);
    let denied_workspace = denied_server.create_workspace(&denied_scenario);
    let denied_binding = server_binding(&denied_scenario);
    let denied_session =
        denied_server.create_session(&denied_workspace, "deny safely", &denied_binding);
    let (d_revision, d_request, d_run_rev) = denied_server.wait_for_permission(&denied_session);
    let waiting = denied_server.snapshot(&denied_session);
    let effect_id = waiting["pending"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (deny_status, deny_body) =
        denied_server.resolve_permission(&denied_session, &d_request, d_revision, d_run_rev, false);
    assert_eq!(deny_status, 200, "deny: {deny_body:?}");
    let denied_terminal = denied_server.wait_for_terminal(&denied_session);
    assert_eq!(denied_terminal["lifecycle"], "ready");
    assert_eq!(denied_terminal["runs"][0]["status"], "failed");
    assert_eq!(
        denied_terminal["runs"][0]["failure_code"],
        "permission_denied"
    );
    // The denied run is terminal and its pending permission is consumed; the
    // session returns to `ready` for a follow-up but the failed child cannot
    // be resumed (v2 thread-linked runs carry no runtime checkpoint).
    assert!(denied_terminal["pending"].is_null());
    denied_provider.assert_consumed();
    drop(denied_server);
    let engine = engine_for(&denied_scenario);
    assert_eq!(
        engine.effect_status(&effect_id).unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
}

#[test]
fn write_file_deny_never_mutates_and_never_reenters_the_provider() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([write_file_reply("deny-write")]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);

    let server = ServerChild::start(&scenario);
    let workspace = server.create_workspace(&scenario);
    let binding = server_binding(&scenario);
    let session_id = server.create_session(&workspace, "create new.txt", &binding);
    let (revision, request_id, run_revision) = server.wait_for_permission(&session_id);
    let waiting = server.snapshot(&session_id);
    let effect_id = waiting["pending"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!scenario.root().join("new.txt").exists());

    let (deny_status, deny_body) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, false);
    assert_eq!(deny_status, 200, "deny: {deny_body:?}");
    let terminal = server.wait_for_terminal(&session_id);
    assert_eq!(terminal["runs"][0]["status"], "failed");
    assert_eq!(terminal["runs"][0]["failure_code"], "permission_denied");
    assert!(!scenario.root().join("new.txt").exists());
    provider.assert_consumed();
    assert_eq!(provider.requests().len(), 1);
    drop(server);
    let engine = engine_for(&scenario);
    assert_eq!(
        engine.effect_status(&effect_id).unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
}

#[test]
fn write_file_allow_resumes_in_a_new_process_verifies_and_completes_once() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        write_file_reply("allow-write"),
        ProviderReply::completion("done"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);

    let server = ServerChild::start(&scenario);
    let workspace = server.create_workspace(&scenario);
    let binding = server_binding(&scenario);
    let session_id = server.create_session(&workspace, "create new.txt", &binding);
    let (revision, request_id, run_revision) = server.wait_for_permission(&session_id);
    let (allow_status, allow_body) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, true);
    assert_eq!(allow_status, 200, "allow: {allow_body:?}");
    let terminal = server.wait_for_terminal(&session_id);
    assert_eq!(terminal["lifecycle"], "ready");
    assert_eq!(terminal["runs"][0]["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    let evidence = completion_evidence(&terminal).expect("completion card carries evidence");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["status"], "passed");
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_tool_result_reached_provider(&requests[1].body);

    // A repeated permission decision on the consumed request is rejected and
    // never re-executes the effect or re-enters the provider.
    let (repeat_status, _) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, true);
    assert!(
        repeat_status == 404 || repeat_status == 409,
        "repeat permission returned {repeat_status}"
    );
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    assert_eq!(provider.requests().len(), 2);
}

#[test]
fn failed_verification_is_durable_and_never_claims_completion() {
    let scenario = Scenario::new();
    let provider = ScriptedProvider::start([
        write_file_reply("failing-verification-write"),
        ProviderReply::completion("done"),
    ]);
    scenario.write_config(
        provider.endpoint(),
        r#"["/usr/bin/grep","-q","not-present","new.txt"]"#,
    );

    let server = ServerChild::start(&scenario);
    let workspace = server.create_workspace(&scenario);
    let binding = server_binding(&scenario);
    let session_id = server.create_session(&workspace, "create new.txt", &binding);
    let (revision, request_id, run_revision) = server.wait_for_permission(&session_id);
    let (allow_status, allow_body) =
        server.resolve_permission(&session_id, &request_id, revision, run_revision, true);
    assert_eq!(allow_status, 200, "allow: {allow_body:?}");
    let terminal = server.wait_for_terminal(&session_id);
    assert_eq!(terminal["lifecycle"], "failed");
    assert_eq!(terminal["runs"][0]["status"], "failed");
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("new.txt")).unwrap(),
        "created by e2e\n"
    );
    // The failure is durable in the transcript and never claims completion.
    assert!(
        terminal["transcript"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "failure"
                && entry["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("verification failed"))
    );
    let shown = cli_show(&scenario, &server, &session_id);
    assert!(shown.status.success());
    assert_eq!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "failed"
    );
    assert_ne!(
        json(&shown)["data"]["session"]["runs"][0]["status"],
        "completed"
    );
    provider.assert_consumed();
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_tool_result_reached_provider(&requests[1].body);
}
