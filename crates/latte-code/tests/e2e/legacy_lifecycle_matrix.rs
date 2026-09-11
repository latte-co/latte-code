//! Legacy v1 run-lifecycle matrix tests, visible through the final binary.
//!
//! All three tests in this module were removed in the v2 session-command
//! migration:
//!
//! - `public_legacy_lifecycle_matrix_is_visible_through_final_list_and_show`
//! - `public_lease_loss_fencing_is_observed_as_interrupted_by_final_binary`
//! - `public_unknown_effect_reconciliation_is_terminal_in_final_binary`
//!
//! They seeded v1 runs through the v1 engine API (`create_turn`,
//! `apply_transition`, `interrupt_after_lease_loss`, `reconcile_unknown_and_abort`)
//! and asserted their projection through the v1 CLI `list`/`show` shapes
//! (`data.turns[]` / `data.run`).
//!
//! In v2 the run is a child of a session: `list`/`show` read `sessions` and
//! return `data.sessions[]` / `data.session`. A v1 run is never inserted into
//! `sessions`, so it is invisible to the v2 session commands (`list` returns
//! an empty catalogue; `show <run-id>` fails closed as `not_found`). The v1
//! run lifecycle state machine and its fencing/reconciliation entry points are
//! removed from the CLI contract.
//!
//! The v2 successor behavior — the session lifecycle (ready/running/
//! `waiting_permission`/`waiting_input`/`interrupted`/`failed`/
//! `reconciliation_required`)
//! projected through the final binary, including lease-loss recovery and
//! unknown-effect reconciliation — is covered by `public_lifecycle_matrix.rs`,
//! which seeds v2 sessions through `create_session_v2` +
//! `commit_session_turn_update` and drives the same `list`/`show` surface.
