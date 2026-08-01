use super::support::{PtySession, Scenario};
use std::time::Duration;

const TUI_READY: &[u8] = b"\x1b[>3u";
const CTRL_P: &[u8] = b"\x1b[112;5u";
const ESCAPE: &[u8] = b"\x1b[27u";

#[cfg(unix)]
#[test]
fn final_tui_slash_suggestions_filter_navigate_and_execute_builtins() {
    let scenario = Scenario::new();
    scenario.write_config("http://127.0.0.1:9", r#"["/bin/pwd"]"#);
    let mut pty = PtySession::spawn(scenario.command(&["tui"]));
    assert!(pty.wait_for_output(TUI_READY, Duration::from_secs(5)));

    pty.write(b"/");
    assert!(
        pty.wait_for_output(b"Find and resume a saved session", Duration::from_secs(5)),
        "slash suggestions were not rendered: {}",
        String::from_utf8_lossy(&pty.output())
    );
    pty.write(b"\x1b[B\r");
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
