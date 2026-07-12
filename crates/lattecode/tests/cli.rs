use serde_json::Value;
use std::process::{Command, Output, Stdio};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

fn invoke(args: &[&str], configure: impl FnOnce(&mut Command)) -> Output {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lattecode"));
    command
        .args(args)
        .current_dir(root.path())
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

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn help_list_show_and_usage_have_stable_envelopes_and_exits() {
    let help = invoke(&["--help"], |_| {});
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("lattecode [--json] run"));

    let json_help = invoke(&["--json", "--help"], |_| {});
    assert!(json_help.status.success());
    assert_eq!(json(&json_help)["status"], "completed");
    assert_eq!(json(&json_help)["version"], 1);

    let list = invoke(&["--json", "list"], |_| {});
    assert!(list.status.success());
    assert_eq!(json(&list)["data"]["runs"], serde_json::json!([]));

    let missing = invoke(
        &["--json", "show", "01900000-0000-7000-8000-000000000001"],
        |_| {},
    );
    assert_eq!(missing.status.code(), Some(4));
    assert_eq!(json(&missing)["error"]["code"], "run_not_found");
    let missing_text = invoke(&["show", "01900000-0000-7000-8000-000000000002"], |_| {});
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
fn configuration_and_provider_failures_are_typed() {
    let missing = invoke(&["--json", "run", "do work"], |_| {});
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(json(&missing)["error"]["code"], "configuration");

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

    let invalid_verification = invoke(&["--json", "run", "do work"], |command| {
        command
            .env("LATTE_OPENAI_ENDPOINT", "http://127.0.0.1:1")
            .env("LATTE_OPENAI_MODEL", "test")
            .env("LATTE_OPENAI_API_KEY", "secret")
            .env("LATTE_VERIFY_ARGV", "not-json");
    });
    assert_eq!(invalid_verification.status.code(), Some(2));
    assert!(
        json(&invalid_verification)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid LATTE_VERIFY_ARGV")
    );

    let empty_verification = invoke(&["--json", "run", "do work"], |command| {
        command
            .env("LATTE_OPENAI_ENDPOINT", "http://127.0.0.1:1")
            .env("LATTE_OPENAI_MODEL", "test")
            .env("LATTE_OPENAI_API_KEY", "secret")
            .env("LATTE_VERIFY_ARGV", "[]");
    });
    assert_eq!(empty_verification.status.code(), Some(2));
    assert!(
        json(&empty_verification)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must not be empty")
    );

    let transport = invoke(&["--json", "run", "do work"], |command| {
        command
            .env("LATTE_OPENAI_ENDPOINT", "http://127.0.0.1:1")
            .env("LATTE_OPENAI_MODEL", "test")
            .env("LATTE_OPENAI_API_KEY", "secret")
            .env("LATTE_VERIFY_ARGV", r#"["/usr/bin/true"]"#);
    });
    assert_eq!(transport.status.code(), Some(1));
    assert_eq!(json(&transport)["status"], "failed");
    assert_eq!(json(&transport)["data"]["run"]["status"], "failed");

    for (missing_name, configure_partial) in
        [("LATTE_OPENAI_MODEL", 1_u8), ("LATTE_OPENAI_API_KEY", 2_u8)]
    {
        let output = invoke(&["--json", "run", "do work"], |command| {
            command.env("LATTE_OPENAI_ENDPOINT", "http://127.0.0.1:1");
            if configure_partial >= 2 {
                command.env("LATTE_OPENAI_MODEL", "test");
            }
            command.env("LATTE_VERIFY_ARGV", r#"["/usr/bin/true"]"#);
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
    command
        .env("LATTE_OPENAI_ENDPOINT", endpoint)
        .env("LATTE_OPENAI_MODEL", "mock")
        .env("LATTE_OPENAI_API_KEY", "secret")
        .env("LATTE_VERIFY_ARGV", verification);
}

#[test]
#[allow(clippy::too_many_lines)]
fn run_waiting_resume_allow_and_deny_are_durable_across_processes() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_lattecode");
    let execute = |args: &[&str], endpoint: &str| {
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(root.path())
            .stdin(Stdio::null());
        configured(&mut command, endpoint, r#"["/usr/bin/true"]"#);
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

    let root2 = tempfile::tempdir().unwrap();
    let endpoint2 = completion_server();
    let execute2 = |args: &[&str]| {
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(root2.path())
            .stdin(Stdio::null());
        configured(&mut command, &endpoint2, r#"["/usr/bin/true"]"#);
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
        .database_path(root2.path().join(".latte/lattecode.db"))
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
    let binary = env!("CARGO_BIN_EXE_lattecode");
    let mut command = Command::new("/usr/bin/script");
    #[cfg(target_os = "macos")]
    command.args(["-q", "/dev/null", binary, "tui"]);
    #[cfg(target_os = "linux")]
    command.args(["-qec", &format!("{binary} tui"), "/dev/null"]);
    command
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    preserve_coverage_profiles(&mut command);
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"?\tq").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let terminal = String::from_utf8_lossy(&output.stdout);
    assert!(terminal.contains("\u{1b}[?1049h"));
    assert!(terminal.contains("\u{1b}[?1049l"));
}

#[cfg(unix)]
#[test]
fn tui_dispatches_prompt_and_consumes_runtime_feedback() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_lattecode");
    let endpoint = completion_server();
    let mut command = Command::new("/usr/bin/script");
    #[cfg(target_os = "macos")]
    command.args(["-q", "/dev/null", binary, "tui"]);
    #[cfg(target_os = "linux")]
    command.args(["-qec", &format!("{binary} tui"), "/dev/null"]);
    command
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    preserve_coverage_profiles(&mut command);
    configured(&mut command, &endpoint, r#"["/usr/bin/true"]"#);
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    // Runs -> Timeline -> Details -> Prompt, then submit a real runtime command.
    input.write_all(b"\t\t\tcheck\r").unwrap();
    thread::sleep(std::time::Duration::from_millis(700));
    input.write_all(b"q").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.path().join(".latte/lattecode.db").exists());
}
