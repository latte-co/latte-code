//! Verification plan shared by the v1 compatibility surface and the v2 thread
//! runtime. The v1 `AgentRuntime` was removed when the CLI migrated to the
//! embedded HTTP+SSE server; `VerificationPlan` is retained because the v2
//! `ThreadRuntimeService` still consumes it.

/// Describes how a completed file change is verified before the run is
/// declared successful.
#[derive(Clone, Debug)]
pub struct VerificationPlan {
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_ms: u64,
    pub grace_ms: u64,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
}
