//! PTY probe used to inject SIGINT at each transactional terminal-entry boundary.
use latte_tui::{TerminalGuard, TuiError};
fn main() {
    let requested = std::env::args().nth(1).expect("stage");
    if let Some(cleanup) = requested.strip_prefix("Cleanup") {
        let mut guard = TerminalGuard::enter().expect("enter");
        guard.restore_with_hook(|stage| {
            if format!("{stage:?}").eq_ignore_ascii_case(cleanup) {
                signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise");
            }
        });
        let interrupted = guard.interrupted();
        drop(guard);
        if interrupted {
            std::process::exit(130)
        }
        return;
    }
    let result = TerminalGuard::enter_with_hook(|stage| {
        if format!("{stage:?}").eq_ignore_ascii_case(&requested) {
            signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise");
        }
    });
    match result {
        Err(TuiError::Interrupted) => std::process::exit(130),
        Ok(guard) => drop(guard),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1)
        }
    }
}
