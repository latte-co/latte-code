//! Legacy v1 run-lifecycle matrix tests, visible through the final binary.
//!
//! All three tests in this module were removed in the v2 session-command
//! migration:
//!
//! - `public_legacy_lifecycle_matrix_is_visible_through_final_list_and_show`
//! - `public_lease_loss_fencing_is_observed_as_interrupted_by_final_binary`
//! - `public_unknown_effect_reconciliation_is_terminal_in_final_binary`
//!
//! They seeded v1 runs through the v1 engine API (`create_run`,
//! `apply_transition`, `interrupt_after_lease_loss`, `reconcile_unknown_and_abort`)
//! and asserted their projection through the v1 CLI `list`/`show` shapes
//! (`data.runs[]` / `data.run`).
//!
//! In v2 the run is a child of a thread: `list`/`show` read `threads_v2` and
//! return `data.sessions[]` / `data.session`. A v1 run is never inserted into
//! `threads_v2`, so it is invisible to the v2 session commands (`list` returns
//! an empty catalogue; `show <run-id>` fails closed as `not_found`). The v1
//! run lifecycle state machine and its fencing/reconciliation entry points are
//! removed from the CLI contract.
//!
//! The v2 successor behavior — the thread lifecycle (ready/running/
//! `waiting_permission`/`waiting_input`/`interrupted`/`failed`/
//! `reconciliation_required`)
//! projected through the final binary, including lease-loss recovery and
//! unknown-effect reconciliation — is covered by `public_lifecycle_matrix.rs`,
//! which seeds v2 threads through `create_thread_v2` +
//! `commit_thread_run_update` and drives the same `list`/`show` surface.
