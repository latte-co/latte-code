use super::support::{ProviderReply, PtySession, Scenario, ScriptedProvider, json, wait_until};
use std::time::Duration;

const TUI_READY: &[u8] = b"\x1b[>3u";
const F10: &[u8] = b"\x1b[21~";
const CTRL_A: &[u8] = b"\x1b[97;5u";
const CTRL_C: &[u8] = b"\x1b[99;5u";

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn chunked_stream_resize_follow_up_and_input_complete_one_durable_thread() {
    let scenario = Scenario::new();
    let first_event = "data: {\"choices\":[{\"delta\":{\"content\":\"live delta sentinel\"}}]}\n\n";
    let final_events = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\" then complete\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let stream = format!("{first_event}{final_events}");
    let provider = ScriptedProvider::start([
        ProviderReply::raw(200, "text/event-stream", stream.into_bytes())
            .chunked(first_event.len(), Duration::from_millis(250)),
        ProviderReply::input_request("matrix-input", "Provide the matrix value", false),
        ProviderReply::completion("input follow-up completed"),
    ]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",streaming:true,compatibility_input_request:true",
    );
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "interactive-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"start the streamed matrix\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(
        pty.wait_for_output(b"live delta sentinel", Duration::from_secs(5)),
        "stream delta was not rendered before completion: {}",
        String::from_utf8_lossy(&pty.output())
    );

    let before_resize = pty.output().len();
    pty.resize(18, 68);
    assert!(pty.wait_for_growth(before_resize, Duration::from_secs(5)));
    assert!(
        pty.wait_for_output(b"Completed", Duration::from_secs(5)),
        "stream completion was not rendered after resize: {}",
        String::from_utf8_lossy(&pty.output())
    );

    pty.resize(36, 108);
    pty.write(b"ask for one follow-up value\r");
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Input required", Duration::from_secs(5)));
    pty.write(b"\x1b[200~matrix-value\nsecond-line\x1b[201~");
    pty.write(b"\x7f\x1b[13;2utail\r");
    assert!(provider.wait_for_calls(3, Duration::from_secs(5)));
    assert!(wait_until(Duration::from_secs(5), || {
        let listed = scenario.output(&["--json", "list"], |_| {});
        listed.status.success()
            && json(&listed)["data"]["runs"]
                .as_array()
                .is_some_and(|runs| {
                    runs.iter()
                        .filter(|run| run["status"] == "completed")
                        .count()
                        == 2
                })
    }));
    provider.assert_consumed();

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body["stream"], true);
    assert_eq!(requests[1].body["stream"], true);
    let users = requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        users,
        [
            "start the streamed matrix",
            "ask for one follow-up value",
            "matrix-value\nsecond-lin\ntail"
        ]
    );

    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_provider_wait_interrupts_cleanly_and_restart_never_reenters() {
    let scenario = Scenario::new();
    let waiting = ": provider body is waiting\n\n";
    let discarded = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"must be discarded\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let provider = ScriptedProvider::start([ProviderReply::raw(
        200,
        "text/event-stream",
        format!("{waiting}{discarded}").into_bytes(),
    )
    .chunked(waiting.len(), Duration::from_millis(1_500))]);
    scenario.write_config_with_provider_fields(
        provider.endpoint(),
        r#"["/bin/pwd"]"#,
        ".latte/latte-code.db",
        ",streaming:true",
    );
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "cancel-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"cancel while provider is waiting\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Running", Duration::from_secs(5)));
    pty.write(CTRL_C);
    assert!(
        pty.wait_for_output(b"Interrupted", Duration::from_secs(5)),
        "clean provider cancellation was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert!(pty.is_running());
    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());

    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert!(
        json(&listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["status"] == "interrupted")
    );

    let mut restart_command = scenario.command(&["tui"]);
    restart_command.env("TEST_OPENAI_KEY", "cancel-secret");
    let mut restart = PtySession::spawn(restart_command);
    assert!(restart.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(restart.wait_for_output(b"Interrupted", Duration::from_secs(5)));
    assert_eq!(provider.requests().len(), 1);
    restart.write(F10);
    assert!(restart.finish(Duration::from_secs(5)).0.success());
}

#[cfg(unix)]
#[test]
fn edit_observed_failure_reaches_provider_then_verifies_without_mutation() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("duplicate.txt"), "same\nsame\n").unwrap();
    let edit = serde_json::json!({
        "path": "duplicate.txt",
        "before": "same",
        "after": "changed",
        "precondition": "562db9b7dbd05bedf8f05dba56c17da47886d5eb878a939704463ccc105c1fe8"
    });
    let provider = ScriptedProvider::start([
        ProviderReply::tool_call("ambiguous-edit", "edit_file", &edit),
        ProviderReply::completion("observed edit failure handled"),
    ]);
    scenario.write_config(provider.endpoint(), r#"["/bin/pwd"]"#);
    let mut command = scenario.command(&["tui"]);
    command.env("TEST_OPENAI_KEY", "edit-failure-secret");
    let mut pty = PtySession::spawn(command);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    pty.write(b"attempt the ambiguous edit\r");
    assert!(provider.wait_for_calls(1, Duration::from_secs(5)));
    assert!(pty.wait_for_output(b"Permission required", Duration::from_secs(5)));
    pty.write(CTRL_A);
    assert!(provider.wait_for_calls(2, Duration::from_secs(5)));
    assert!(
        pty.wait_for_output(b"Completed", Duration::from_secs(5)),
        "observed failure recovery was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    provider.assert_consumed();

    assert_eq!(
        std::fs::read_to_string(scenario.root().join("duplicate.txt")).unwrap(),
        "same\nsame\n"
    );
    let requests = provider.requests();
    let tool_result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(tool_result["tool_call_id"], "ambiguous-edit");
    assert!(tool_result["content"].as_str().unwrap().contains("match"));

    pty.write(F10);
    assert!(pty.finish(Duration::from_secs(5)).0.success());
    let listed = scenario.output(&["--json", "list"], |_| {});
    assert!(listed.status.success());
    assert!(
        json(&listed)["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["status"] == "completed")
    );
}
