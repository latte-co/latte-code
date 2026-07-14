use serde_json::Value;
use std::process::{Command, Output, Stdio};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

fn invoke(args: &[&str], configure: impl FnOnce(&mut Command)) -> Output {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_latte-code"));
    command
        .args(args)
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env_remove("LATTE_OPENAI_ENDPOINT")
        .env_remove("LATTE_OPENAI_MODEL")
        .env_remove("LATTE_OPENAI_API_KEY")
        .env_remove("LATTE_VERIFY_ARGV")
        .stdin(Stdio::null());
    preserve_coverage_profiles(&mut command);
    configure(&mut command);
    command.output().unwrap()
}

fn preserve_coverage_profiles(command: &mut Command) {
    if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
        command.env(
            "LLVM_PROFILE_FILE",
            profile.replace(".profraw", "-%p.profraw"),
        );
    }
}

fn write_config(command: &Command, endpoint: &str, verification: &str) {
    write_config_with_database(command, endpoint, verification, ".latte/latte-code.db");
}

fn write_config_with_database(
    command: &Command,
    endpoint: &str,
    verification: &str,
    database_path: &str,
) {
    let root = command.get_current_dir().unwrap();
    std::fs::create_dir_all(root.join(".latte")).unwrap();
    std::fs::write(root.join(".latte/latte-code.jsonc"), format!(r#"{{version:1,default_provider:"main",providers:{{main:{{type:"openai-chat",model:"mock",endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},credential_ref_id:"env:TEST_OPENAI_KEY",data_scope_id:"workspace",credential_generation:1}}}},database:{{path:{database_path:?}}},verification:{{argv:{verification}}}}}"#)).unwrap();
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn help_list_show_and_usage_have_stable_envelopes_and_exits() {
    let help = invoke(&["--help"], |_| {});
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("latte-code [--json] run"));

    let json_help = invoke(&["--json", "--help"], |_| {});
    assert!(json_help.status.success());
    assert_eq!(json(&json_help)["status"], "completed");
    assert_eq!(json(&json_help)["version"], 1);

    let list = invoke(&["--json", "list"], |_| {});
    assert!(list.status.success());
    assert_eq!(json(&list)["data"]["runs"], serde_json::json!([]));

    let missing = invoke(
        &["--json", "show", "01900000-0000-7000-8000-000000000001"],
        |command| write_config(command, "http://127.0.0.1:1", r#"["/usr/bin/true"]"#),
    );
    assert_eq!(missing.status.code(), Some(4));
    assert_eq!(json(&missing)["error"]["code"], "run_not_found");
    let missing_text = invoke(
        &["show", "01900000-0000-7000-8000-000000000002"],
        |command| {
            write_config(command, "http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
        },
    );
    assert_eq!(missing_text.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&missing_text.stderr).contains("was not found"));

    let invalid = invoke(&["--json", "show", "not-a-run"], |_| {});
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(json(&invalid)["error"]["code"], "usage");

    let unknown = invoke(&["wat"], |_| {});
    assert_eq!(unknown.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("expected:"));
}

#[test]
fn nested_build_directory_discovers_workspace_and_uses_defaults() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("target/debug");
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_latte-code"))
        .args(["--json", "list"])
        .current_dir(&nested)
        .env("HOME", root.path().join("home"))
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join(".latte/latte-code.db").exists());
    assert!(!nested.join(".latte/latte-code.db").exists());
}

#[test]
fn configuration_and_provider_failures_are_typed() {
    let missing = invoke(&["--json", "run", "do work"], |_| {});
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(json(&missing)["error"]["code"], "configuration");
    assert!(
        json(&missing)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OPENAI_API_KEY")
    );

    let missing_deny = invoke(
        &[
            "--json",
            "resume",
            "01900000-0000-7000-8000-000000000001",
            "--deny",
        ],
        |_| {},
    );
    assert_eq!(missing_deny.status.code(), Some(4));
    assert_eq!(json(&missing_deny)["error"]["code"], "run_not_found");

    let empty_database = invoke(&["--json", "list"], |command| {
        write_config_with_database(command, "http://127.0.0.1:1", r#"["/usr/bin/true"]"#, "  ");
    });
    assert_eq!(empty_database.status.code(), Some(2));
    assert!(
        json(&empty_database)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("database.path must not be empty")
    );

    let invalid_verification = invoke(&["--json", "run", "do work"], |command| {
        write_config(command, "http://127.0.0.1:1", "not-json");
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(invalid_verification.status.code(), Some(2));
    assert!(
        json(&invalid_verification)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid JSONC")
    );

    let empty_verification = invoke(&["--json", "run", "do work"], |command| {
        write_config(command, "http://127.0.0.1:1", "[]");
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(empty_verification.status.code(), Some(2));
    assert!(
        json(&empty_verification)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must not be empty")
    );

    let transport = invoke(&["--json", "run", "do work"], |command| {
        write_config(command, "http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
        command.env("TEST_OPENAI_KEY", "secret");
    });
    assert_eq!(transport.status.code(), Some(1));
    assert_eq!(json(&transport)["status"], "failed");
    assert_eq!(json(&transport)["data"]["run"]["status"], "failed");

    {
        let missing_name = "TEST_OPENAI_KEY";
        let output = invoke(&["--json", "run", "do work"], |command| {
            write_config(command, "http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
        });
        assert_eq!(output.status.code(), Some(2));
        assert!(
            json(&output)["error"]["message"]
                .as_str()
                .unwrap()
                .contains(missing_name)
        );
    }
}

#[test]
fn explicit_tui_fails_cleanly_without_a_terminal() {
    let output = invoke(&["tui"], |_| {});
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a TTY"));
}

#[test]
fn headless_parser_and_placeholder_cover_every_public_command_shape() {
    use latte_headless::{HeadlessCommand, parse, render_placeholder};
    let root = tempfile::tempdir().unwrap();
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(root.path())
        .build()
        .unwrap();
    let run_id = latte_core::RunId::from_uuid(
        uuid::Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap(),
    );
    let commands = [
        HeadlessCommand::List,
        HeadlessCommand::Run {
            prompt: "work".into(),
            focus: None,
        },
        HeadlessCommand::Resume {
            run_id,
            allow: true,
        },
        HeadlessCommand::Show { run_id },
    ];
    for command in &commands {
        assert!(!render_placeholder(command, &engine).is_empty());
    }
    assert!(parse(&["run".into()]).is_err());
    assert!(parse(&["run".into(), "--focus".into()]).is_err());
    assert!(parse(&["run".into(), "x".into(), "--focus".into()]).is_err());
}

fn completion_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/chat/completions", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = br#"{"choices":[{"message":{"content":"done","tool_calls":[]}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    endpoint
}

fn configured(command: &mut Command, endpoint: &str, verification: &str) {
    write_config(command, endpoint, verification);
    command.env("TEST_OPENAI_KEY", "secret");
}

#[cfg(unix)]
fn capture_stream(
    mut stream: impl Read + Send + 'static,
) -> (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&capture);
    let handle = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(&buffer[..read]),
            }
        }
    });
    (capture, handle)
}

#[cfg(unix)]
fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[cfg(unix)]
fn capture_contains(capture: &Arc<Mutex<Vec<u8>>>, needle: &[u8]) -> bool {
    capture
        .lock()
        .unwrap()
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
#[allow(clippy::too_many_lines)]
fn run_waiting_resume_allow_and_deny_are_durable_across_processes() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_latte-code");
    let execute = |args: &[&str], endpoint: &str| {
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(root.path())
            .env("HOME", root.path().join("home"))
            .stdin(Stdio::null());
        write_config_with_database(
            &command,
            endpoint,
            r#"["/usr/bin/true"]"#,
            "state/nested/custom.db",
        );
        command.env("TEST_OPENAI_KEY", "secret");
        preserve_coverage_profiles(&mut command);
        command.output().unwrap()
    };

    let endpoint = completion_server();
    let waiting = execute(&["--json", "run", "finish safely"], &endpoint);
    assert_eq!(waiting.status.code(), Some(10));
    let waiting_json = json(&waiting);
    assert_eq!(waiting_json["status"], "waiting");
    let run_id = waiting_json["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned();

    let completed = execute(&["--json", "resume", &run_id, "--allow"], &endpoint);
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(json(&completed)["status"], "completed");
    let shown = execute(&["--json", "show", &run_id], &endpoint);
    assert!(shown.status.success());
    assert_eq!(json(&shown)["data"]["run"]["status"], "completed");
    let listed = execute(&["list"], &endpoint);
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&run_id));
    assert!(root.path().join("state/nested/custom.db").exists());
    assert!(!root.path().join(".latte/latte-code.db").exists());

    let root2 = tempfile::tempdir().unwrap();
    let endpoint2 = completion_server();
    let execute2 = |args: &[&str]| {
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(root2.path())
            .env("HOME", root2.path().join("home"))
            .stdin(Stdio::null());
        write_config_with_database(
            &command,
            &endpoint2,
            r#"["/usr/bin/true"]"#,
            "state/deny.db",
        );
        command.env("TEST_OPENAI_KEY", "secret");
        preserve_coverage_profiles(&mut command);
        command.output().unwrap()
    };
    let waiting2 = execute2(&["--json", "run", "deny safely"]);
    assert_eq!(
        waiting2.status.code(),
        Some(10),
        "{}",
        String::from_utf8_lossy(&waiting2.stdout)
    );
    let run_id2 = json(&waiting2)["error"]["message"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned();
    let waiting_show = execute2(&["--json", "show", &run_id2]);
    let effect_id = json(&waiting_show)["data"]["run"]["pending_permission"]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut deny_command = Command::new(binary);
    deny_command
        .args(["--json", "resume", &run_id2, "--deny"])
        .current_dir(root2.path())
        .env("HOME", root2.path().join("home"))
        .env_remove("LATTE_OPENAI_ENDPOINT")
        .env_remove("LATTE_OPENAI_MODEL")
        .env_remove("LATTE_OPENAI_API_KEY")
        .env_remove("LATTE_VERIFY_ARGV")
        .stdin(Stdio::null());
    preserve_coverage_profiles(&mut deny_command);
    let denied = deny_command.output().unwrap();
    assert_eq!(
        denied.status.code(),
        Some(11),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert_eq!(json(&denied)["status"], "denied");
    assert_eq!(json(&denied)["data"]["run"]["status"], "failed");
    assert_eq!(
        json(&denied)["data"]["run"]["failure"]["code"],
        "permission_denied"
    );
    let shown_denied = execute2(&["--json", "show", &run_id2]);
    assert_eq!(json(&shown_denied)["data"]["run"]["status"], "failed");
    assert!(json(&shown_denied)["data"]["run"]["pending_permission"].is_null());
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(root2.path())
        .database_path(root2.path().join("state/deny.db"))
        .build()
        .unwrap();
    assert_eq!(
        engine.effect_status(&effect_id).unwrap(),
        latte_engine::EffectStatus::ObservedFailed
    );
    assert!(
        engine
            .runtime_checkpoint(latte_core::RunId::from_uuid(
                uuid::Uuid::parse_str(&run_id2).unwrap(),
            ))
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn tui_runs_inside_a_real_pty_and_restores_terminal_modes() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_latte-code");
    let endpoint = completion_server();
    let pty = nix::pty::openpty(
        Some(&nix::pty::Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let master = std::fs::File::from(pty.master);
    let mut input = master.try_clone().unwrap();
    let slave_stdout = pty.slave.try_clone().unwrap();
    let slave_stderr = pty.slave.try_clone().unwrap();
    let mut command = Command::new(binary);
    command
        .arg("tui")
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .stdin(Stdio::from(pty.slave))
        .stdout(Stdio::from(slave_stdout))
        .stderr(Stdio::from(slave_stderr));
    configured(&mut command, &endpoint, r#"["/usr/bin/true"]"#);
    preserve_coverage_profiles(&mut command);
    let mut child = command.spawn().unwrap();
    drop(command);
    let (output, output_reader) = capture_stream(master);

    // The keyboard-protocol push is the final-binary readiness boundary: the
    // engine and projection already exist, raw mode is active, and CSI-u input
    // can now distinguish Shift+Enter from Enter.
    assert!(
        wait_until(Duration::from_secs(5), || capture_contains(
            &output,
            b"\x1b[>3u"
        )),
        "TUI never enabled keyboard disambiguation: {}",
        String::from_utf8_lossy(&output.lock().unwrap())
    );

    input.write_all(b"first").unwrap();
    input.write_all(b"\x1b[13;2u").unwrap();
    input.write_all(b"second").unwrap();
    input.write_all(b"\r").unwrap();
    let durable_reader = latte_engine::EngineBuilder::new()
        .workspace_root(root.path())
        .database_path(root.path().join(".latte/latte-code.db"))
        .build()
        .unwrap();

    // This is the earliest durable authoritative submission boundary. It
    // proves the final binary decoded CSI-u Shift+Enter as a newline and the
    // following plain Enter as exactly one submit, independent of provider
    // completion timing.
    let submitted = wait_until(Duration::from_secs(5), || {
        let Ok(threads) = durable_reader.list_threads_v2() else {
            return false;
        };
        let user_entries = threads
            .iter()
            .flat_map(|thread| thread.transcript.entries.iter())
            .filter(|entry| entry.kind == latte_core::TranscriptKind::User)
            .collect::<Vec<_>>();
        user_entries.len() == 1 && user_entries[0].text == "first\nsecond"
    });
    let observed_prompts = durable_reader.list_threads_v2().map(|threads| {
        threads
            .into_iter()
            .flat_map(|thread| thread.transcript.entries)
            .filter(|entry| entry.kind == latte_core::TranscriptKind::User)
            .map(|entry| entry.text)
            .collect::<Vec<_>>()
    });
    assert!(
        submitted,
        "final TUI did not durably submit exactly one multiline prompt; observed={observed_prompts:?}; terminal={}",
        String::from_utf8_lossy(&output.lock().unwrap())
    );

    // F10 keeps this restoration assertion separate from signal delivery.
    // Double Ctrl+C remains covered by the reducer and signal-edge tests.
    input.write_all(b"\x1b[21~").unwrap();
    drop(input);
    let status = child.wait().unwrap();
    output_reader.join().unwrap();
    assert!(status.success());
    let output = output.lock().unwrap();
    let terminal = String::from_utf8_lossy(&output);
    assert!(terminal.contains("first"));
    assert!(terminal.contains("second"));
    assert!(terminal.contains("\u{1b}[>3u"));
    assert!(terminal.contains("\u{1b}[?1049h"));
    assert!(terminal.contains("\u{1b}[?1049l"));
    assert!(terminal.contains("\u{1b}[<1u"));
}

#[cfg(unix)]
#[test]
fn tui_commits_prompt_before_a_provider_configuration_failure() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_latte-code");
    let pty = nix::pty::openpty(
        Some(&nix::pty::Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None::<&nix::sys::termios::Termios>,
    )
    .unwrap();
    let master = std::fs::File::from(pty.master);
    let mut input = master.try_clone().unwrap();
    let slave_stdout = pty.slave.try_clone().unwrap();
    let slave_stderr = pty.slave.try_clone().unwrap();
    let mut command = Command::new(binary);
    command
        .arg("tui")
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .env_remove("TEST_OPENAI_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::from(pty.slave))
        .stdout(Stdio::from(slave_stdout))
        .stderr(Stdio::from(slave_stderr));
    write_config(&command, "http://127.0.0.1:1", r#"["/usr/bin/true"]"#);
    preserve_coverage_profiles(&mut command);
    let mut child = command.spawn().unwrap();
    drop(command);
    let (output, output_reader) = capture_stream(master);
    assert!(wait_until(Duration::from_secs(5), || capture_contains(
        &output,
        b"\x1b[>3u"
    )));

    let sentinel = b"failed-start-visible-sentinel";
    input.write_all(sentinel).unwrap();
    input.write_all(b"\r").unwrap();

    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(root.path())
        .database_path(root.path().join(".latte/latte-code.db"))
        .build()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            engine.list_threads_v2().is_ok_and(|threads| {
                threads.len() == 1
                    && threads[0].lifecycle == latte_core::ThreadLifecycle::Failed
                    && threads[0].transcript.entries.iter().any(|entry| {
                        entry.kind == latte_core::TranscriptKind::User
                            && entry.text.as_bytes() == sentinel
                    })
                    && threads[0]
                        .transcript
                        .entries
                        .iter()
                        .any(|entry| entry.kind == latte_core::TranscriptKind::Failure)
            })
        }),
        "provider configuration failure was not durably projected: {}",
        String::from_utf8_lossy(&output.lock().unwrap())
    );
    assert!(wait_until(Duration::from_secs(5), || capture_contains(
        &output,
        b"selected model could not be started"
    )));

    let threads = engine.list_threads_v2().unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].lifecycle, latte_core::ThreadLifecycle::Failed);
    assert_eq!(threads[0].runs.len(), 1);
    assert_eq!(
        threads[0].runs[0].status,
        latte_core::ThreadRunStatus::Failed
    );
    assert!(child.try_wait().unwrap().is_none());
    let terminal = output.lock().unwrap();
    assert!(
        !terminal
            .windows(b"prompt has been restored".len())
            .any(|value| value == b"prompt has been restored")
    );
    assert!(
        !terminal
            .windows(b"Unable to submit".len())
            .any(|value| value == b"Unable to submit")
    );
    assert!(
        !terminal
            .windows(b"TEST_OPENAI_KEY".len())
            .any(|value| value == b"TEST_OPENAI_KEY")
    );
    drop(terminal);

    input.write_all(b"\x1b[21~").unwrap();
    drop(input);
    assert!(child.wait().unwrap().success());
    output_reader.join().unwrap();
}

#[cfg(unix)]
#[test]
fn tui_dispatches_prompt_and_consumes_runtime_feedback() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_latte-code");
    let endpoint = completion_server();
    let mut command = Command::new("/usr/bin/script");
    #[cfg(target_os = "macos")]
    command.args(["-q", "/dev/null", binary, "tui"]);
    #[cfg(target_os = "linux")]
    command.args(["-qec", &format!("{binary} tui"), "/dev/null"]);
    command
        .current_dir(root.path())
        .env("HOME", root.path().join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    preserve_coverage_profiles(&mut command);
    configured(&mut command, &endpoint, r#"["/usr/bin/true"]"#);
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    // Composer starts focused. F5 remains a compatibility submit key and F10
    // exits after feedback is consumed. The sibling direct-PTY test exercises
    // final-binary Shift+Enter/Enter semantics and durable transcript evidence.
    thread::sleep(std::time::Duration::from_millis(500));
    input.write_all(b"check\x1b[15~").unwrap();
    thread::sleep(std::time::Duration::from_millis(1_500));
    input.write_all(b"\x1b[21~").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join(".latte/latte-code.db").exists());
}
