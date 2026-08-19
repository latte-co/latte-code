//! Legacy v1 `resume <run-id> --allow|--deny` boundary tests.
//!
//! All three tests in this module were removed in the v2 session-command
//! migration. They exclusively drove the v1 permission-reissue workflow
//! through the CLI:
//!
//! - `final_cli_reissues_expired_process_and_tool_permissions_before_exactly_once_execution`
//! - `final_cli_resumes_public_interrupted_checkpoint_at_verification_without_provider_reentry`
//! - `interrupted_final_message_reissues_verification_permission_before_completion`
//!
//! The v1 `resume <run-id> --allow|--deny` command is gone: permission
//! decisions now go through the HTTP API
//! (`POST /v1/sessions/{id}/permissions/{req_id}`) or the TUI, and `resume
//! <session-id> <prompt>` is a thread follow-up, not a permission decision.
//! The v1 lease-token permission reissue and resume-from-checkpoint paths
//! lived in the v1 `AgentRuntime` and are unreachable from the v2 contract.
//!
//! The permission-decision behavior these tests guarded is covered over HTTP
//! in `portable.rs`:
//! - `final_binary_server_resolves_a_permission_request_through_http`
//!   (allow -> the effect executes exactly once)
//! - `final_binary_server_denies_a_permission_request_through_http`
//!   (deny -> the effect never executes)
//! - `final_binary_server_rejects_stale_run_revision_on_permission`
//!   (revision-fenced permission resolution, the v2 successor to the v1
//!   lease-token reissue fencing)
