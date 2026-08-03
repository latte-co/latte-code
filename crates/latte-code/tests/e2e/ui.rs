use super::support::{PtySession, Scenario};
use std::time::Duration;

const TUI_READY: &[u8] = b"\x1b[>3u";
const CTRL_P: &[u8] = b"\x1b[112;5u";
const ESCAPE: &[u8] = b"\x1b[27u";
const F10: &[u8] = b"\x1b[21~";

#[cfg(unix)]
#[test]
fn final_tui_slash_suggestions_filter_navigate_and_execute_builtins() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    pty.write(b"/");
    assert!(
        pty.wait_for_output(b"/sessions", Duration::from_secs(5)),
        "slash suggestions were not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"\x1b[B\x1b[13u");
    assert!(
        pty.wait_for_output(b"No saved sessions", Duration::from_secs(5)),
        "Down and Enter did not execute /sessions: {}",
        String::from_utf8_lossy(&pty.output())
    );

    pty.write(ESCAPE);
    pty.write(b"/h");
    assert!(
        pty.wait_for_output(b"Show keyboard shortcuts", Duration::from_secs(5)),
        "slash prefix filtering was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"\r");
    assert!(pty.wait_for_output(b"Single-session transcript", Duration::from_secs(5)));

    pty.write(CTRL_P);
    pty.write(b"jjjjjj\r");
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    assert!(
        output
            .windows(b"Show keyboard shortcuts".len())
            .any(|value| value == b"Show keyboard shortcuts")
    );
}

#[cfg(unix)]
#[test]
fn final_tui_palette_navigation_and_bracketed_paste_are_operable() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    pty.write(CTRL_P);
    assert!(
        pty.wait_for_output(b"Show keyboard shortcuts", Duration::from_secs(5)),
        "command palette was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"jjj\r");
    assert!(pty.wait_for_output(b"Single-session transcript", Duration::from_secs(5)));

    // Enter Navigation through the palette, which also closes Help. Exercise
    // every navigation family even though the initial transcript has no
    // expandable actions, then return to the composer with `i`.
    pty.write(CTRL_P);
    pty.write(b"jjjj\r");
    pty.write(b"?jk\x1b[6~\x1b[5~\x1b[H\x1b[C\x1b[D ?i");

    // Crossterm recognizes the terminal's bracketed-paste protocol as one
    // Paste event, including its embedded newline. Backspace then removes the
    // final emoji as a complete grapheme before Shift+Enter appends a newline.
    pty.write(b"\x1b[200~pasted first\npasted emoji \xf0\x9f\x99\x82\x1b[201~");
    assert!(pty.wait_for_output(b"pasted emoji", Duration::from_secs(5)));
    pty.write(b"\x7f\x1b[13;2u");
    pty.write(b"\x1b[200~after newline\x1b[201~");
    assert!(
        pty.wait_for_output(b"after newline", Duration::from_secs(5)),
        "paste/backspace/newline was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );

    let before_refresh = pty.output().len();
    pty.write(CTRL_P);
    pty.write(b"jjjjj\r");
    assert!(
        pty.wait_for_growth(before_refresh, Duration::from_secs(5)),
        "Refresh did not redraw the final TUI"
    );

    pty.write(CTRL_P);
    pty.write(b"jjjjjj\r");
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    assert!(
        output
            .windows(b"pasted first".len())
            .any(|value| value == b"pasted first")
    );
    assert!(
        output
            .windows(b"after newline".len())
            .any(|value| value == b"after newline")
    );
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|value| value == b"\x1b[?1049l")
    );
}

#[cfg(unix)]
#[test]
fn final_tui_resizes_across_viewport_tiers_and_sanitizes_rich_paste() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    let mut pty = PtySession::spawn_with_size(scenario.command(&["tui"]), 46, 180);
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));
    assert!(pty.wait_for_visible_text("permissions:", Duration::from_secs(5)));

    for (rows, columns) in [(30, 112), (22, 84), (16, 62), (10, 44), (6, 28)] {
        let before = pty.output().len();
        pty.resize(rows, columns);
        assert!(
            pty.wait_for_growth(before, Duration::from_secs(5)),
            "viewport {rows}x{columns} did not redraw"
        );
        assert!(pty.is_running(), "viewport {rows}x{columns} exited the TUI");
    }

    pty.resize(20, 80);
    assert!(
        pty.wait_for_output(b"\x1b[18;7H", Duration::from_secs(5)),
        "20x80 viewport did not finish its redraw: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(CTRL_P);
    assert!(
        pty.wait_for_visible_text("Show keyboard shortcuts", Duration::from_secs(5)),
        "resized viewport did not open the command palette: {}",
        String::from_utf8_lossy(&pty.output())
    );
    let before_close = pty.output().len();
    pty.write(ESCAPE);
    assert!(pty.wait_for_growth(before_close, Duration::from_secs(5)));
    pty.write(b"\x1b[200~wide \xe7\x95\x8c\ttabbed \x1b[31mred text\x1b[0m\nsecond row\x1b[201~");
    assert!(
        pty.wait_for_visible_text("wide", Duration::from_secs(5)),
        "sanitized rich paste was not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    assert!(pty.wait_for_visible_text("tabbed", Duration::from_secs(5)));
    assert!(pty.wait_for_visible_text("red text", Duration::from_secs(5)));
    assert!(pty.wait_for_visible_text("second row", Duration::from_secs(5)));

    pty.write(F10);
    let (status, output) = pty.finish(Duration::from_secs(5));
    assert!(status.success());
    assert!(
        output
            .windows(b"\x1b[?1049l".len())
            .any(|value| value == b"\x1b[?1049l")
    );
}

#[cfg(unix)]
#[test]
fn final_tui_cold_starts_and_restores_every_viewport_composition() {
    for (rows, columns) in [(60, 200), (40, 120), (24, 80), (18, 59), (10, 39), (4, 20)] {
        let scenario = Scenario::new();
        scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
        let mut pty = PtySession::spawn_with_size(scenario.command(&["tui"]), rows, columns);
        assert!(
            pty.wait_for_output(TUI_READY, Duration::from_secs(5)),
            "viewport {rows}x{columns} did not initialize: {}",
            String::from_utf8_lossy(&pty.output())
        );
        assert!(pty.is_running(), "viewport {rows}x{columns} exited early");
        pty.write(F10);
        let (status, output) = pty.finish(Duration::from_secs(5));
        assert!(status.success(), "viewport {rows}x{columns} failed");
        assert!(
            output
                .windows(b"\x1b[?1049l".len())
                .any(|value| value == b"\x1b[?1049l"),
            "viewport {rows}x{columns} did not restore the terminal"
        );
    }
}

#[cfg(unix)]
#[test]
fn final_tui_exercises_escape_reverse_navigation_and_empty_picker_boundaries() {
    let scenario = Scenario::new();
    std::fs::create_dir_all(scenario.root().join(".latte")).unwrap();
    std::fs::write(
        scenario.root().join(".latte/latte-code.jsonc"),
        r#"{
            version: 1,
            default_model: "primary/model-a",
            providers: {
                primary: {
                    type: "openai-chat", models: ["model-a", "model-b"],
                    endpoint: "http://127.0.0.1:1",
                    api_key: { source: "env", name: "TEST_OPENAI_KEY" }
                },
                secondary: {
                    type: "openai-chat", models: { "model-c": { name: "Friendly C" } },
                    endpoint: "http://127.0.0.1:1",
                    api_key: { source: "env", name: "TEST_OPENAI_KEY" }
                }
            },
            database: { path: ".latte/latte-code.db" },
            verification: { argv: ["/bin/pwd"] }
        }"#,
    )
    .unwrap();
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    // Slash popup reverse navigation and explicit dismissal.
    pty.write(b"/");
    assert!(pty.wait_for_visible_text("/sessions", Duration::from_secs(5)));
    pty.write(b"\x1b[A");
    pty.write(ESCAPE);
    pty.write(b"\x7f");

    // Palette supports reverse aliases and both close gestures.
    pty.write(CTRL_P);
    assert!(pty.wait_for_visible_text("Show keyboard shortcuts", Duration::from_secs(5)));
    pty.write(b"\x1b[Ak");
    pty.write(ESCAPE);
    pty.write(CTRL_P);
    pty.write(CTRL_P);

    // Model picker covers filtering to zero results, grapheme backspace,
    // reverse movement, dismissal, and a draft selection.
    pty.write(b"/model\r");
    assert!(pty.wait_for_visible_text("model-b", Duration::from_secs(5)));
    pty.write(b"no-such-model");
    assert!(pty.wait_for_visible_text("No matching provider models", Duration::from_secs(5)));
    for _ in 0..13 {
        pty.write(b"\x7f");
    }
    pty.write(b"\x1b[B\x1b[A");
    pty.write(ESCAPE);
    pty.write(b"/model\r");
    assert!(pty.wait_for_visible_text("model-b", Duration::from_secs(5)));
    let after_selection = pty.output().len();
    pty.write(b"\x1b[B\r");
    assert!(pty.wait_for_growth(after_selection, Duration::from_secs(5)));

    // A durable provider-configuration failure creates a catalog entry
    // without network access. Then exercise every session-picker direction.
    pty.write(b"catalog boundary\r");
    assert!(pty.wait_for_visible_text(
        "selected model could not be started",
        Duration::from_secs(5)
    ));
    pty.write(b"/new\r");
    let before_sessions = pty.output().len();
    pty.write(b"/sessions\r");
    assert!(pty.wait_for_growth(before_sessions, Duration::from_secs(5)));
    pty.write(b"\x1b[Bj\x1b[Ak");
    pty.write(ESCAPE);
    pty.write(b"/sessions\r\r");
    assert!(pty.wait_for_visible_text("catalog boundary", Duration::from_secs(5)));

    pty.write(ESCAPE);
    pty.write(b"?q");
    assert!(pty.finish(Duration::from_secs(5)).0.success());
}

#[cfg(unix)]
#[test]
fn final_tui_startup_configuration_and_storage_failures_return_stable_exit_codes() {
    let invalid = Scenario::new();
    std::fs::create_dir_all(invalid.root().join(".latte")).unwrap();
    std::fs::write(invalid.root().join(".latte/latte-code.jsonc"), "{").unwrap();
    let (status, output) =
        PtySession::spawn(invalid.command(&["tui"])).finish(Duration::from_secs(5));
    assert_eq!(status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output).contains("configuration"));

    let invalid_policy = Scenario::new();
    std::fs::create_dir_all(invalid_policy.root().join(".latte")).unwrap();
    std::fs::write(
        invalid_policy.root().join(".latte/latte-code.jsonc"),
        r#"{
            version:1, default_model:"main/mock",
            providers:{main:{type:"openai-chat",models:["mock"],endpoint:"http://127.0.0.1:1",
                api_key:{source:"env",name:"TEST_OPENAI_KEY"}}},
            database:{path:".latte/state.db"},
            thread:{max_request_bytes:1,max_input_bytes:1,reserved_output_bytes:1,context_cap_bytes:0}
        }"#,
    )
    .unwrap();
    let (status, output) =
        PtySession::spawn(invalid_policy.command(&["tui"])).finish(Duration::from_secs(5));
    assert_eq!(status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output).contains("configuration"));

    let blocked_parent = Scenario::new();
    std::fs::create_dir_all(blocked_parent.root().join(".latte")).unwrap();
    std::fs::write(blocked_parent.root().join("blocked"), "not a directory").unwrap();
    std::fs::write(
        blocked_parent.root().join(".latte/latte-code.jsonc"),
        r#"{
            version:1, default_model:"main/mock",
            providers:{main:{type:"openai-chat",models:["mock"],endpoint:"http://127.0.0.1:1",
                api_key:{source:"env",name:"TEST_OPENAI_KEY"}}},
            database:{path:"blocked/state.db"}, verification:{argv:["/bin/pwd"]}
        }"#,
    )
    .unwrap();
    let (status, output) =
        PtySession::spawn(blocked_parent.command(&["tui"])).finish(Duration::from_secs(5));
    assert_eq!(status.code(), Some(70));
    assert!(String::from_utf8_lossy(&output).contains("cannot create"));

    let directory_database = Scenario::new();
    std::fs::create_dir_all(directory_database.root().join(".latte/database-dir")).unwrap();
    std::fs::write(
        directory_database.root().join(".latte/latte-code.jsonc"),
        r#"{
            version:1, default_model:"main/mock",
            providers:{main:{type:"openai-chat",models:["mock"],endpoint:"http://127.0.0.1:1",
                api_key:{source:"env",name:"TEST_OPENAI_KEY"}}},
            database:{path:".latte/database-dir"}, verification:{argv:["/bin/pwd"]}
        }"#,
    )
    .unwrap();
    let (status, output) =
        PtySession::spawn(directory_database.command(&["tui"])).finish(Duration::from_secs(5));
    assert_eq!(status.code(), Some(70));
    assert!(!output.is_empty());
}
