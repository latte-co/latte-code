use crate::{
    EngineHandle, Lease, ThreadEffectDescriptor, ThreadEffectExecutionError,
    ThreadEffectObservedValue,
};
use latte_core::RunId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::process::Stdio;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use thiserror::Error;
use tokio::sync::Notify;
#[cfg(unix)]
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::{Duration, sleep, timeout},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessDecision {
    Allow,
    Ask,
    Deny,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(unix)]
pub(crate) enum GroupProbe {
    Absent,
    Present,
    Uncertain,
}
#[cfg(not(unix))]
pub(crate) type GroupProbe = ();

#[derive(Clone, Debug)]
pub struct ProcessInvocation<'a> {
    pub argv: &'a [String],
    pub shell: Option<&'a str>,
    pub cwd: &'a str,
    pub env: &'a BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub grace_ms: u64,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
    pub run_revision: u64,
    pub effect_id: &'a str,
    pub attempt: u64,
    pub approval_digest: Option<&'a str>,
    pub lease_owner: &'a str,
    pub lease_token: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub termination: ProcessTermination,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessTermination {
    Exited,
    TimedOut,
    Cancelled,
}
impl ProcessOutput {
    #[must_use]
    pub fn command_succeeded(&self) -> bool {
        self.termination == ProcessTermination::Exited && self.exit_code == Some(0)
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("invalid process request: {0}")]
    Invalid(String),
    #[error("process operation denied")]
    Denied,
    #[error("process permission required")]
    PermissionRequired { digest: String },
    #[error("approval is stale, mismatched, consumed, or lease-invalid")]
    InvalidApproval,
    #[error("process supervision unsupported on this platform")]
    Unsupported,
    #[error("process supervision failed: {0}")]
    Supervision(String),
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationInner>,
}
#[derive(Debug, Default)]
struct CancellationInner {
    cancelled: AtomicBool,
    notify: Notify,
}
impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }
    pub async fn cancelled(&self) {
        if self.inner.cancelled.load(Ordering::Acquire) {
            return;
        }
        self.inner.notify.notified().await;
    }
}

#[must_use]
pub fn classify(invocation: &ProcessInvocation<'_>) -> ProcessDecision {
    if let Some(shell) = invocation.shell {
        let lower = shell.to_ascii_lowercase();
        if lower.contains("rm -rf /") || lower.contains(":(){") || lower.contains("mkfs") {
            ProcessDecision::Deny
        } else {
            ProcessDecision::Ask
        }
    } else {
        let Some(command) = invocation.argv.first() else {
            return ProcessDecision::Deny;
        };
        let safe_grep = command == "/usr/bin/grep"
            && invocation.argv.len() == 4
            && invocation.argv[1] == "-q"
            && !invocation.argv[2].is_empty()
            && !std::path::Path::new(&invocation.argv[3]).is_absolute()
            && !std::path::Path::new(&invocation.argv[3])
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if invocation.env.is_empty()
            && ((command == "/bin/pwd" && invocation.argv.len() == 1) || safe_grep)
        {
            ProcessDecision::Allow
        } else {
            ProcessDecision::Ask
        }
    }
}

#[derive(Serialize)]
struct Binding<'a> {
    argv: &'a [String],
    shell: Option<&'a str>,
    cwd: &'a str,
    env: &'a BTreeMap<String, String>,
    timeout_ms: u64,
    grace_ms: u64,
    stdout_cap: usize,
    stderr_cap: usize,
    run_revision: u64,
    effect_id: &'a str,
    attempt: u64,
    lease_owner: &'a str,
    lease_token: u64,
    version: u32,
}
pub(crate) fn digest(i: &ProcessInvocation<'_>) -> String {
    let b = Binding {
        argv: i.argv,
        shell: i.shell,
        cwd: i.cwd,
        env: i.env,
        timeout_ms: i.timeout_ms,
        grace_ms: i.grace_ms,
        stdout_cap: i.stdout_cap,
        stderr_cap: i.stderr_cap,
        run_revision: i.run_revision,
        effect_id: i.effect_id,
        attempt: i.attempt,
        lease_owner: i.lease_owner,
        lease_token: i.lease_token,
        version: 1,
    };
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&b).expect("binding serializes"))
    )
}

/// Owned process fields reconstructed from a redacted durable v2 descriptor.
/// They are converted to the borrowed legacy supervisor invocation only after
/// the Started transaction has committed.
#[derive(Clone, Debug)]
pub(crate) struct ThreadProcessSpec {
    pub argv: Vec<String>,
    pub shell: Option<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub grace_ms: u64,
    pub stdout_cap: usize,
    pub stderr_cap: usize,
}

impl ThreadProcessSpec {
    pub(crate) fn from_input(input: &Value) -> Result<Self, ProcessError> {
        let object = input
            .as_object()
            .ok_or_else(|| ProcessError::Invalid("process input must be an object".into()))?;
        let argv = object
            .get("argv")
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| ProcessError::Invalid("argv must be an array".into()))?
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            ProcessError::Invalid("argv entries must be strings".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let shell = object
            .get("shell")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ProcessError::Invalid("shell must be a string".into()))
            })
            .transpose()?;
        let cwd = object
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_owned();
        let env = object
            .get("env")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|error| ProcessError::Invalid(format!("invalid env: {error}")))?
            .unwrap_or_default();
        let number = |key: &str, default: u64, maximum: u64| -> Result<u64, ProcessError> {
            let value = object.get(key).map_or(Ok(default), |value| {
                value.as_u64().ok_or_else(|| {
                    ProcessError::Invalid(format!("{key} must be an unsigned integer"))
                })
            })?;
            if value == 0 || value > maximum {
                return Err(ProcessError::Invalid(format!("{key} is out of range")));
            }
            Ok(value)
        };
        let timeout_ms = number("timeout_ms", 30_000, 600_000)?;
        let grace_ms = object.get("grace_ms").map_or(Ok(250), |value| {
            value
                .as_u64()
                .ok_or_else(|| ProcessError::Invalid("grace_ms must be an unsigned integer".into()))
        })?;
        if grace_ms > 30_000 {
            return Err(ProcessError::Invalid("grace_ms is out of range".into()));
        }
        let stdout_cap = usize::try_from(number("stdout_cap", 64 * 1024, 1_048_576)?)
            .map_err(|_| ProcessError::Invalid("stdout_cap overflows usize".into()))?;
        let stderr_cap = usize::try_from(number("stderr_cap", 64 * 1024, 1_048_576)?)
            .map_err(|_| ProcessError::Invalid("stderr_cap overflows usize".into()))?;
        if argv.is_empty() == shell.is_none() {
            return Err(ProcessError::Invalid(
                "provide exactly one of argv or shell".into(),
            ));
        }
        Ok(Self {
            argv,
            shell,
            cwd,
            env,
            timeout_ms,
            grace_ms,
            stdout_cap,
            stderr_cap,
        })
    }
    pub(crate) fn invocation<'a>(
        &'a self,
        run_revision: u64,
        descriptor: &'a ThreadEffectDescriptor,
        lease: &'a Lease,
    ) -> ProcessInvocation<'a> {
        ProcessInvocation {
            argv: &self.argv,
            shell: self.shell.as_deref(),
            cwd: &self.cwd,
            env: &self.env,
            timeout_ms: self.timeout_ms,
            grace_ms: self.grace_ms,
            stdout_cap: self.stdout_cap,
            stderr_cap: self.stderr_cap,
            run_revision,
            effect_id: &descriptor.effect_id,
            attempt: descriptor.attempt,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        }
    }
}

impl EngineHandle {
    pub(crate) async fn execute_started_thread_process(
        &self,
        descriptor: &ThreadEffectDescriptor,
        run_revision: u64,
        lease: &Lease,
        cancellation: &CancellationToken,
    ) -> Result<ThreadEffectObservedValue, ThreadEffectExecutionError> {
        if !self.process_supervision_supported {
            return Err(ThreadEffectExecutionError::Certified(
                ProcessError::Unsupported.to_string(),
            ));
        }
        let spec = ThreadProcessSpec::from_input(&descriptor.input)
            .map_err(|error| ThreadEffectExecutionError::Uncertain(error.to_string()))?;
        let invocation = spec.invocation(run_revision, descriptor, lease);
        let cwd = self
            .tools
            .resolve_cwd(&spec.cwd)
            .map_err(|error| ThreadEffectExecutionError::Uncertain(error.to_string()))?;
        let _operation = std::sync::Arc::clone(&self.operation_gate)
            .acquire_owned()
            .await
            .map_err(|_| ThreadEffectExecutionError::Uncertain("operation gate closed".into()))?;
        match supervise(
            &invocation,
            &cwd,
            cancellation,
            self.process_group_probe_override,
        )
        .await
        {
            Ok(output) if output.termination == ProcessTermination::Cancelled => {
                Err(ThreadEffectExecutionError::Uncertain(
                    "process cancelled after its external outcome may have happened".into(),
                ))
            }
            Ok(output) => Ok(ThreadEffectObservedValue {
                result: serde_json::to_string(&output).unwrap_or_else(|_| "{}".into()),
                payload: Some(latte_core::redact_thread_value(serde_json::json!({
                    "tool_call_id":descriptor.tool_call_id,
                    "name":descriptor.name,
                    "output":output,
                }))),
                success: true,
            }),
            Err(error) => Err(ThreadEffectExecutionError::Uncertain(error.to_string())),
        }
    }
}

impl EngineHandle {
    /// Executes and durably records an actual verification process outcome.
    pub async fn execute_verification(
        &self,
        run_id: RunId,
        expected_revision: u64,
        lease: &Lease,
        now_ms: u64,
        invocation: &ProcessInvocation<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        self.reject_linked_run(run_id)
            .map_err(|error| ProcessError::Invalid(error.to_string()))?;
        let _operation = Arc::clone(&self.operation_gate)
            .acquire_owned()
            .await
            .map_err(|_| ProcessError::Supervision("operation gate closed".into()))?;
        if invocation.run_revision != expected_revision {
            return Err(ProcessError::Invalid(
                "verification revision mismatch".into(),
            ));
        }
        let output = self
            .execute_process_inner(run_id, lease, now_ms, invocation, cancellation)
            .await?;
        let effect_epoch = self.storage.effect_epoch(run_id).map_err(storage)?;
        let workspace_manifest_digest = self
            .manifest_digest()
            .map_err(|error| ProcessError::Supervision(error.to_string()))?;
        let record = crate::storage::VerificationRecord {
            revision: expected_revision,
            effect_epoch,
            effect_id: invocation.effect_id.into(),
            passed: output.command_succeeded(),
            workspace_manifest_digest,
            summary: serde_json::to_string(&output)
                .map_err(|e| ProcessError::Supervision(e.to_string()))?,
        };
        let metadata =
            serde_json::to_string(&record).map_err(|e| ProcessError::Supervision(e.to_string()))?;
        self.storage
            .record_verification_evidence(
                run_id,
                expected_revision,
                lease,
                &crate::VerificationEvidence {
                    id: invocation.effect_id,
                    metadata_json: &metadata,
                    blob_ref: None,
                },
                now_ms,
            )
            .map_err(storage)?;
        Ok(output)
    }
    /// Rebinds an expired process approval atomically without executing it.
    pub fn reissue_process_permission(
        &self,
        old_effect_id: &str,
        run_id: RunId,
        lease: &Lease,
        now_ms: u64,
        invocation: &ProcessInvocation<'_>,
    ) -> Result<String, ProcessError> {
        self.reject_linked_run(run_id)
            .map_err(|error| ProcessError::Invalid(error.to_string()))?;
        if classify(invocation) != ProcessDecision::Ask {
            return Err(ProcessError::Invalid(
                "only ask operations can be reissued".into(),
            ));
        }
        let exact = digest(invocation);
        let descriptor = serde_json::to_string(&Binding {
            argv: invocation.argv,
            shell: invocation.shell,
            cwd: invocation.cwd,
            env: invocation.env,
            timeout_ms: invocation.timeout_ms,
            grace_ms: invocation.grace_ms,
            stdout_cap: invocation.stdout_cap,
            stderr_cap: invocation.stderr_cap,
            run_revision: invocation.run_revision,
            effect_id: invocation.effect_id,
            attempt: invocation.attempt,
            lease_owner: invocation.lease_owner,
            lease_token: invocation.lease_token,
            version: 1,
        })
        .map_err(|e| ProcessError::Invalid(e.to_string()))?;
        self.storage
            .replace_pending_effect(
                old_effect_id,
                invocation.effect_id,
                run_id,
                invocation.run_revision,
                invocation.attempt,
                &descriptor,
                &exact,
                lease,
                now_ms,
            )
            .map_err(storage)?;
        Ok(exact)
    }
    #[allow(clippy::too_many_lines)]
    pub async fn execute_process(
        &self,
        run_id: RunId,
        lease: &Lease,
        now_ms: u64,
        invocation: &ProcessInvocation<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        self.reject_linked_run(run_id)
            .map_err(|error| ProcessError::Invalid(error.to_string()))?;
        let _operation = Arc::clone(&self.operation_gate)
            .acquire_owned()
            .await
            .map_err(|_| ProcessError::Supervision("operation gate closed".into()))?;
        self.execute_process_inner(run_id, lease, now_ms, invocation, cancellation)
            .await
    }
    #[allow(clippy::too_many_lines)]
    async fn execute_process_inner(
        &self,
        run_id: RunId,
        lease: &Lease,
        now_ms: u64,
        invocation: &ProcessInvocation<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        if !self.process_supervision_supported {
            return Err(ProcessError::Unsupported);
        }
        if invocation.argv.is_empty() == invocation.shell.is_none() {
            return Err(ProcessError::Invalid(
                "provide exactly one of argv or shell".into(),
            ));
        }
        if invocation.stdout_cap == 0 || invocation.stderr_cap == 0 || invocation.timeout_ms == 0 {
            return Err(ProcessError::Invalid(
                "caps and timeout must be nonzero".into(),
            ));
        }
        if lease.owner != invocation.lease_owner || lease.fencing_token != invocation.lease_token {
            return Err(ProcessError::InvalidApproval);
        }
        let cwd = self
            .tools
            .resolve_cwd(invocation.cwd)
            .map_err(|e| ProcessError::Invalid(e.to_string()))?;
        let decision = classify(invocation);
        if decision == ProcessDecision::Deny {
            return Err(ProcessError::Denied);
        }
        let exact = digest(invocation);
        if decision == ProcessDecision::Ask && invocation.approval_digest.is_none() {
            let descriptor = serde_json::to_string(&Binding {
                argv: invocation.argv,
                shell: invocation.shell,
                cwd: invocation.cwd,
                env: invocation.env,
                timeout_ms: invocation.timeout_ms,
                grace_ms: invocation.grace_ms,
                stdout_cap: invocation.stdout_cap,
                stderr_cap: invocation.stderr_cap,
                run_revision: invocation.run_revision,
                effect_id: invocation.effect_id,
                attempt: invocation.attempt,
                lease_owner: invocation.lease_owner,
                lease_token: invocation.lease_token,
                version: 1,
            })
            .map_err(|e| ProcessError::Invalid(e.to_string()))?;
            self.storage
                .create_prepared_permission(
                    invocation.effect_id,
                    run_id,
                    invocation.run_revision.saturating_sub(2),
                    invocation.run_revision,
                    invocation.attempt,
                    &descriptor,
                    &exact,
                    lease,
                    now_ms,
                )
                .map_err(storage)?;
            return Err(ProcessError::PermissionRequired { digest: exact });
        }
        let authority = if decision == ProcessDecision::Allow {
            self.storage
                .create_prepared_permission(
                    invocation.effect_id,
                    run_id,
                    invocation.run_revision,
                    invocation.run_revision,
                    invocation.attempt,
                    "{}",
                    &exact,
                    lease,
                    now_ms,
                )
                .map_err(storage)?;
            self.storage
                .consume_permission_and_start(
                    invocation.effect_id,
                    run_id,
                    invocation.run_revision,
                    lease,
                    &exact,
                    now_ms,
                )
                .map_err(storage)?
        } else {
            if invocation.approval_digest != Some(exact.as_str()) {
                return Err(ProcessError::InvalidApproval);
            }
            self.storage
                .consume_permission_and_start(
                    invocation.effect_id,
                    run_id,
                    invocation.run_revision,
                    lease,
                    &exact,
                    now_ms,
                )
                .map_err(|_| ProcessError::InvalidApproval)?
        };
        let terminal_now = || {
            if now_ms < 1_000_000_000_000 {
                now_ms
            } else {
                crate::wall_now_ms()
            }
        };
        match supervise(
            invocation,
            &cwd,
            cancellation,
            self.process_group_probe_override,
        )
        .await
        {
            Ok(output) => {
                if output.termination == ProcessTermination::Cancelled {
                    self.storage
                        .mark_effect_unknown(&authority, terminal_now())
                        .map_err(storage)?;
                    return Err(ProcessError::Supervision(
                        "process cancelled before its external outcome could be certified".into(),
                    ));
                }
                self.storage
                    .finish_effect(
                        &authority,
                        true,
                        &serde_json::to_string(&output)
                            .map_err(|e| ProcessError::Supervision(e.to_string()))?,
                        terminal_now(),
                    )
                    .map_err(storage)?;
                Ok(output)
            }
            Err(error) => {
                self.storage
                    .mark_effect_unknown(&authority, terminal_now())
                    .map_err(storage)?;
                Err(error)
            }
        }
    }
}
#[allow(clippy::needless_pass_by_value)]
fn storage(error: crate::StorageError) -> ProcessError {
    ProcessError::Supervision(format!("durable sequencing: {error}"))
}

pub(crate) fn supervise_git_diff(
    cwd: &std::path::Path,
    cap: usize,
) -> Result<(String, bool), ProcessError> {
    supervise_git(cwd, cap, GitQuery::DiffStat)
}
pub(crate) fn supervise_git_changed_files(
    cwd: &std::path::Path,
    cap: usize,
) -> Result<(String, bool), ProcessError> {
    let (tracked, tracked_truncated) = supervise_git(cwd, cap, GitQuery::TrackedChanges)?;
    if tracked_truncated {
        return Ok((tracked, true));
    }
    let remaining = cap
        .saturating_sub(tracked.len())
        .saturating_sub(usize::from(!tracked.is_empty()));
    let (untracked, untracked_truncated) = supervise_git(cwd, remaining, GitQuery::Untracked)?;
    let paths = tracked
        .lines()
        .chain(untracked.lines())
        .filter(|path| !path.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    Ok((
        paths.into_iter().collect::<Vec<_>>().join("\n"),
        untracked_truncated,
    ))
}

#[derive(Clone, Copy)]
enum GitQuery {
    DiffStat,
    TrackedChanges,
    Untracked,
}

fn supervise_git(
    cwd: &std::path::Path,
    cap: usize,
    query: GitQuery,
) -> Result<(String, bool), ProcessError> {
    if !cfg!(unix) {
        return Err(ProcessError::Unsupported);
    }
    let cwd = cwd.to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ProcessError::Supervision(e.to_string()))?;
        runtime.block_on(async move {
            let mut argv = vec![
                "/usr/bin/git".into(),
                "--no-pager".into(),
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "-c".into(),
                "diff.external=".into(),
                "-c".into(),
                "core.attributesFile=/dev/null".into(),
            ];
            match query {
                GitQuery::DiffStat => argv.extend([
                    "diff".into(),
                    "--stat".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--".into(),
                ]),
                GitQuery::TrackedChanges => argv.extend([
                    "diff".into(),
                    "--name-only".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "HEAD".into(),
                    "--".into(),
                    ".".into(),
                    ":(exclude).latte/**".into(),
                    ":(exclude)target/**".into(),
                ]),
                GitQuery::Untracked => argv.extend([
                    "ls-files".into(),
                    "--others".into(),
                    "--exclude-standard".into(),
                    "--".into(),
                    ".".into(),
                    ":(exclude).latte/**".into(),
                    ":(exclude)target/**".into(),
                ]),
            }
            let env = BTreeMap::from([
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ]);
            let invocation = ProcessInvocation {
                argv: &argv,
                shell: None,
                cwd: ".",
                env: &env,
                timeout_ms: 5_000,
                grace_ms: 100,
                stdout_cap: cap,
                stderr_cap: 1024,
                run_revision: 0,
                effect_id: "internal-git-diff",
                attempt: 1,
                approval_digest: None,
                lease_owner: "internal",
                lease_token: 0,
            };
            let output = supervise(&invocation, &cwd, &CancellationToken::new(), None).await?;
            if output.exit_code != Some(0) {
                return Err(ProcessError::Supervision(output.stderr));
            }
            Ok((output.stdout, output.stdout_truncated))
        })
    })
    .join()
    .map_err(|_| ProcessError::Supervision("git supervisor thread panicked".into()))?
}

#[cfg(unix)]
async fn drain(
    mut reader: impl AsyncRead + Unpin,
    cap: usize,
) -> Result<(String, bool), std::io::Error> {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let remaining = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..n.min(remaining)]);
        truncated |= n > remaining;
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}

#[cfg(unix)]
async fn supervise(
    i: &ProcessInvocation<'_>,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
    probe_override: Option<GroupProbe>,
) -> Result<ProcessOutput, ProcessError> {
    use std::os::unix::process::CommandExt;
    if cancel.is_cancelled() {
        return Err(ProcessError::Supervision(
            "process cancelled before it could be started".into(),
        ));
    }
    let mut command = if let Some(shell) = i.shell {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", shell]);
        c
    } else {
        let mut c = Command::new(&i.argv[0]);
        c.args(&i.argv[1..]);
        c
    };
    command
        .current_dir(cwd)
        .env_clear()
        .envs(i.env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.as_std_mut().process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| ProcessError::Supervision(e.to_string()))?;
    let pid = child
        .id()
        .ok_or_else(|| ProcessError::Supervision("missing child pid".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessError::Supervision("stdout pipe missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessError::Supervision("stderr pipe missing".into()))?;
    let out_task = tokio::spawn(drain(stdout, i.stdout_cap));
    let err_task = tokio::spawn(drain(stderr, i.stderr_cap));
    let termination;
    let deadline = sleep(Duration::from_millis(i.timeout_ms));
    tokio::pin!(deadline);
    let status = tokio::select! {biased;
        result=child.wait()=>{termination=ProcessTermination::Exited;let status=result.map_err(|e|ProcessError::Supervision(e.to_string()))?;match group_probe(pid,probe_override)?{GroupProbe::Present=>shutdown_group(pid,i.grace_ms,probe_override).await?,GroupProbe::Absent=>{},GroupProbe::Uncertain=>return Err(ProcessError::Supervision("process group existence is uncertain".into()))}status},
        ()=cancel.cancelled()=>{termination=ProcessTermination::Cancelled;terminate_and_reap(pid,&mut child,i.grace_ms,probe_override).await?},
        ()=&mut deadline=>{termination=ProcessTermination::TimedOut;terminate_and_reap(pid,&mut child,i.grace_ms,probe_override).await?}
    };
    let join_bound = Duration::from_millis(i.grace_ms.saturating_add(500));
    let (stdout, stdout_truncated) = timeout(join_bound, out_task)
        .await
        .map_err(|_| ProcessError::Supervision("stdout drain did not reach EOF".into()))?
        .map_err(|e| ProcessError::Supervision(e.to_string()))?
        .map_err(|e| ProcessError::Supervision(e.to_string()))?;
    let (stderr, stderr_truncated) = timeout(join_bound, err_task)
        .await
        .map_err(|_| ProcessError::Supervision("stderr drain did not reach EOF".into()))?
        .map_err(|e| ProcessError::Supervision(e.to_string()))?
        .map_err(|e| ProcessError::Supervision(e.to_string()))?;
    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        termination,
    })
}
#[cfg(unix)]
fn group_pid(pid: u32) -> Result<nix::unistd::Pid, ProcessError> {
    Ok(nix::unistd::Pid::from_raw(i32::try_from(pid).map_err(
        |_| ProcessError::Supervision("pid overflow".into()),
    )?))
}
#[cfg(unix)]
fn group_probe(pid: u32, override_value: Option<GroupProbe>) -> Result<GroupProbe, ProcessError> {
    use nix::sys::signal::killpg;
    if let Some(value) = override_value {
        return Ok(value);
    }
    match killpg(group_pid(pid)?, None) {
        Ok(()) => Ok(GroupProbe::Present),
        Err(nix::errno::Errno::ESRCH) => Ok(GroupProbe::Absent),
        Err(nix::errno::Errno::EPERM) => Ok(GroupProbe::Uncertain),
        Err(e) => Err(ProcessError::Supervision(e.to_string())),
    }
}
#[cfg(unix)]
async fn shutdown_group(
    pid: u32,
    grace: u64,
    probe_override: Option<GroupProbe>,
) -> Result<(), ProcessError> {
    use nix::sys::signal::{Signal, killpg};
    let group = group_pid(pid)?;
    let _ = killpg(group, Signal::SIGTERM);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(grace);
    while matches!(group_probe(pid, probe_override)?, GroupProbe::Present)
        && tokio::time::Instant::now() < deadline
    {
        sleep(Duration::from_millis(10)).await;
    }
    if matches!(group_probe(pid, probe_override)?, GroupProbe::Uncertain) {
        return Err(ProcessError::Supervision(
            "process group existence is uncertain".into(),
        ));
    }
    if matches!(group_probe(pid, probe_override)?, GroupProbe::Present) {
        let _ = killpg(group, Signal::SIGKILL);
        if !await_group_absent(pid, grace, probe_override).await? {
            return Err(ProcessError::Supervision(
                "process group survived SIGKILL".into(),
            ));
        }
    }
    Ok(())
}
#[cfg(unix)]
async fn terminate_and_reap(
    pid: u32,
    child: &mut tokio::process::Child,
    grace: u64,
    probe_override: Option<GroupProbe>,
) -> Result<std::process::ExitStatus, ProcessError> {
    use nix::sys::signal::{Signal, killpg};
    let group = group_pid(pid)?;
    let _ = killpg(group, Signal::SIGTERM);
    let status = if let Ok(result) = timeout(Duration::from_millis(grace), child.wait()).await {
        result.map_err(|e| ProcessError::Supervision(e.to_string()))?
    } else {
        let _ = killpg(group, Signal::SIGKILL);
        child
            .wait()
            .await
            .map_err(|e| ProcessError::Supervision(e.to_string()))?
    };
    if matches!(group_probe(pid, probe_override)?, GroupProbe::Uncertain) {
        return Err(ProcessError::Supervision(
            "process group existence is uncertain".into(),
        ));
    }
    if matches!(group_probe(pid, probe_override)?, GroupProbe::Present) {
        let _ = killpg(group, Signal::SIGKILL);
        if !await_group_absent(pid, grace, probe_override).await? {
            return Err(ProcessError::Supervision(
                "descendant process group survived SIGKILL".into(),
            ));
        }
    }
    Ok(status)
}

/// Certify that a process group has disappeared after SIGKILL.
///
/// A killed orphan can remain visible to `killpg(..., 0)` as a zombie until its
/// new parent reaps it. That delay is outside the supervisor's control and can
/// become noticeable when tests or the host are busy. We therefore keep the
/// safety check, but give the kernel/init a bounded settling window instead of
/// treating a short scheduling delay as proof that a live descendant survived.
#[cfg(unix)]
async fn await_group_absent(
    pid: u32,
    grace: u64,
    probe_override: Option<GroupProbe>,
) -> Result<bool, ProcessError> {
    const REAP_SETTLE_BOUND_MS: u64 = 10_000;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(grace.max(REAP_SETTLE_BOUND_MS));
    loop {
        match group_probe(pid, probe_override)? {
            GroupProbe::Absent => return Ok(true),
            GroupProbe::Present | GroupProbe::Uncertain
                if tokio::time::Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(10)).await;
            }
            GroupProbe::Uncertain => {
                return Err(ProcessError::Supervision(
                    "process group existence remained uncertain".into(),
                ));
            }
            GroupProbe::Present => return Ok(false),
        }
    }
}
#[cfg(not(unix))]
fn supervise(
    _i: &ProcessInvocation<'_>,
    _cwd: &std::path::Path,
    _cancel: &CancellationToken,
    _probe_override: Option<GroupProbe>,
) -> std::future::Ready<Result<ProcessOutput, ProcessError>> {
    std::future::ready(Err(ProcessError::Unsupported))
}

#[cfg(all(test, not(unix)))]
mod non_unix_tests {
    use super::*;
    use crate::EngineBuilder;
    use latte_core::{IdSource, SystemIdSource};

    #[tokio::test]
    async fn process_capability_is_absent_and_execution_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        assert!(
            engine
                .tool_descriptors()
                .iter()
                .all(|tool| tool.name != "process")
        );
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let argv = vec!["echo".into(), "x".into()];
        let env = BTreeMap::new();
        let request = ProcessInvocation {
            argv: &argv,
            shell: None,
            cwd: ".",
            env: &env,
            timeout_ms: 1_000,
            grace_ms: 10,
            stdout_cap: 1_024,
            stderr_cap: 1_024,
            run_revision: 0,
            effect_id: "unsupported-process",
            attempt: 1,
            approval_digest: None,
            lease_owner: lease.owner(),
            lease_token: lease.fencing_token(),
        };
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &request, &CancellationToken::new())
                .await,
            Err(ProcessError::Unsupported)
        ));
        assert!(engine.effect_status("unsupported-process").is_err());
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::manual_let_else)]
mod tests {
    use super::*;
    use crate::{EffectStatus, EngineBuilder};
    use latte_core::{IdSource, SystemIdSource};
    use std::collections::BTreeMap;
    fn empty_env() -> &'static BTreeMap<String, String> {
        static ENV: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
        ENV.get_or_init(BTreeMap::new)
    }

    async fn assert_group_gone_bounded(pgid: u32) {
        assert!(
            timeout(Duration::from_secs(12), async {
                loop {
                    if matches!(group_probe(pgid, None).unwrap(), GroupProbe::Absent) {
                        break;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok(),
            "process group {pgid} was not reaped within the certification bound"
        );
    }

    fn invocation<'a>(
        shell: Option<&'a str>,
        argv: &'a [String],
        effect: &'a str,
        lease: &'a Lease,
        approval: Option<&'a str>,
    ) -> ProcessInvocation<'a> {
        ProcessInvocation {
            argv,
            shell,
            cwd: ".",
            env: empty_env(),
            timeout_ms: 2_000,
            grace_ms: 50,
            stdout_cap: 128,
            stderr_cap: 128,
            run_revision: 0,
            effect_id: effect,
            attempt: 1,
            approval_digest: approval,
            lease_owner: &lease.owner,
            lease_token: lease.fencing_token,
        }
    }

    #[test]
    fn classifies_argv_shell_and_high_risk() {
        let lease = Lease {
            scope: "runtime".into(),
            owner: "o".into(),
            fencing_token: 1,
            expires_at_ms: 1,
        };
        let empty = Vec::new();
        let safe = vec!["/bin/pwd".into()];
        assert_eq!(
            classify(&invocation(None, &safe, "a", &lease, None)),
            ProcessDecision::Allow
        );
        assert_eq!(
            classify(&invocation(Some("echo x | cat"), &empty, "b", &lease, None)),
            ProcessDecision::Ask
        );
        assert_eq!(
            classify(&invocation(Some("rm -rf /"), &empty, "c", &lease, None)),
            ProcessDecision::Deny
        );
        assert_eq!(
            classify(&invocation(None, &empty, "empty", &lease, None)),
            ProcessDecision::Deny
        );
        for dangerous in [":(){ :|:& };:", "mkfs.ext4 /dev/test"] {
            assert_eq!(
                classify(&invocation(
                    Some(dangerous),
                    &empty,
                    dangerous,
                    &lease,
                    None
                )),
                ProcessDecision::Deny
            );
        }
        let safe_grep = vec![
            "/usr/bin/grep".into(),
            "-q".into(),
            "needle".into(),
            "relative.txt".into(),
        ];
        assert_eq!(
            classify(&invocation(None, &safe_grep, "grep", &lease, None)),
            ProcessDecision::Allow
        );
        for unsafe_path in ["/absolute.txt", "../escape.txt"] {
            let unsafe_grep = vec![
                "/usr/bin/grep".into(),
                "-q".into(),
                "needle".into(),
                unsafe_path.into(),
            ];
            assert_eq!(
                classify(&invocation(None, &unsafe_grep, unsafe_path, &lease, None)),
                ProcessDecision::Ask
            );
        }
        let git_hook = vec!["git".into(), "diff".into(), "--ext-diff".into()];
        let hooked_env = BTreeMap::from([("GIT_EXTERNAL_DIFF".into(), "evil".into())]);
        let hooked = ProcessInvocation {
            env: &hooked_env,
            ..invocation(None, &git_hook, "hook", &lease, None)
        };
        assert_eq!(classify(&hooked), ProcessDecision::Ask);
        let pwd = vec!["/bin/pwd".into()];
        let loader = BTreeMap::from([("LD_PRELOAD".into(), "evil.so".into())]);
        let hooked = ProcessInvocation {
            env: &loader,
            ..invocation(None, &pwd, "loader", &lease, None)
        };
        assert_eq!(classify(&hooked), ProcessDecision::Ask);

        let successful = ProcessOutput {
            exit_code: Some(0),
            stdout: "ok".into(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            termination: ProcessTermination::Exited,
        };
        assert!(successful.command_succeeded());
        assert!(
            !ProcessOutput {
                termination: ProcessTermination::Cancelled,
                ..successful.clone()
            }
            .command_succeeded()
        );
        assert!(
            !ProcessOutput {
                exit_code: Some(1),
                ..successful
            }
            .command_succeeded()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn thread_process_spec_parses_defaults_exactly_and_rejects_typed_boundaries() {
        let defaults = ThreadProcessSpec::from_input(&serde_json::json!({
            "argv": ["/bin/pwd"]
        }))
        .unwrap();
        assert_eq!(defaults.argv, ["/bin/pwd"]);
        assert_eq!(defaults.shell, None);
        assert_eq!(defaults.cwd, ".");
        assert!(defaults.env.is_empty());
        assert_eq!(defaults.timeout_ms, 30_000);
        assert_eq!(defaults.grace_ms, 250);
        assert_eq!(defaults.stdout_cap, 64 * 1024);
        assert_eq!(defaults.stderr_cap, 64 * 1024);

        let configured = ThreadProcessSpec::from_input(&serde_json::json!({
            "shell": "printf ok",
            "cwd": "subdir",
            "env": {"LANG": "C"},
            "timeout_ms": 600_000,
            "grace_ms": 0,
            "stdout_cap": 1_048_576,
            "stderr_cap": 1
        }))
        .unwrap();
        assert_eq!(configured.shell.as_deref(), Some("printf ok"));
        assert_eq!(configured.cwd, "subdir");
        assert_eq!(configured.env["LANG"], "C");
        assert_eq!(configured.timeout_ms, 600_000);
        assert_eq!(configured.grace_ms, 0);
        assert_eq!(configured.stdout_cap, 1_048_576);
        assert_eq!(configured.stderr_cap, 1);

        let lease = Lease {
            scope: "runtime".into(),
            owner: "owner".into(),
            fencing_token: 7,
            expires_at_ms: 99,
        };
        let descriptor = ThreadEffectDescriptor {
            effect_id: "effect".into(),
            tool_call_id: "call_1".into(),
            name: "process".into(),
            input: serde_json::json!({}),
            attempt: 3,
        };
        let invocation = configured.invocation(11, &descriptor, &lease);
        assert_eq!(invocation.run_revision, 11);
        assert_eq!(invocation.effect_id, "effect");
        assert_eq!(invocation.attempt, 3);
        assert_eq!(invocation.lease_owner, "owner");
        assert_eq!(invocation.lease_token, 7);

        let invalid = [
            (serde_json::json!(null), "process input must be an object"),
            (serde_json::json!({"argv":"pwd"}), "argv must be an array"),
            (
                serde_json::json!({"argv":[1]}),
                "argv entries must be strings",
            ),
            (serde_json::json!({"shell":1}), "shell must be a string"),
            (
                serde_json::json!({"argv":["/bin/pwd"],"env":[]}),
                "invalid env:",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"timeout_ms":-1}),
                "timeout_ms must be an unsigned integer",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"timeout_ms":0}),
                "timeout_ms is out of range",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"timeout_ms":600_001}),
                "timeout_ms is out of range",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"grace_ms":"soon"}),
                "grace_ms must be an unsigned integer",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"grace_ms":30_001}),
                "grace_ms is out of range",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"stdout_cap":0}),
                "stdout_cap is out of range",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"stderr_cap":1_048_577}),
                "stderr_cap is out of range",
            ),
            (
                serde_json::json!({}),
                "provide exactly one of argv or shell",
            ),
            (
                serde_json::json!({"argv":["/bin/pwd"],"shell":"pwd"}),
                "provide exactly one of argv or shell",
            ),
        ];
        for (input, expected) in invalid {
            let error = ThreadProcessSpec::from_input(&input).unwrap_err();
            assert!(
                matches!(error, ProcessError::Invalid(message) if message.contains(expected)),
                "input {input} did not produce {expected}"
            );
        }
    }

    #[tokio::test]
    async fn cancellation_token_wakes_waiters_and_remains_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        let waiter = token.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::task::yield_now().await;
        token.cancel();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(token.is_cancelled());
        timeout(Duration::from_secs(1), token.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn execute_process_rejects_invalid_requests_before_creating_effect_authority() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("not-a-directory"), "x").unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let empty = Vec::new();
        let pwd = vec!["/bin/pwd".into()];

        let both = invocation(Some("pwd"), &pwd, "both", &lease, None);
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &both, &CancellationToken::new())
                .await,
            Err(ProcessError::Invalid(message)) if message.contains("exactly one")
        ));
        let neither = invocation(None, &empty, "neither", &lease, None);
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &neither, &CancellationToken::new())
                .await,
            Err(ProcessError::Invalid(message)) if message.contains("exactly one")
        ));

        let zero_cap = ProcessInvocation {
            stdout_cap: 0,
            ..invocation(None, &pwd, "zero-cap", &lease, None)
        };
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &zero_cap, &CancellationToken::new())
                .await,
            Err(ProcessError::Invalid(message)) if message.contains("nonzero")
        ));

        let wrong_owner = ProcessInvocation {
            lease_owner: "other",
            ..invocation(None, &pwd, "wrong-owner", &lease, None)
        };
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &wrong_owner, &CancellationToken::new())
                .await,
            Err(ProcessError::InvalidApproval)
        ));

        let file_cwd = ProcessInvocation {
            cwd: "not-a-directory",
            ..invocation(None, &pwd, "file-cwd", &lease, None)
        };
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &file_cwd, &CancellationToken::new())
                .await,
            Err(ProcessError::Invalid(message)) if message.contains("must be a directory")
        ));

        let dangerous = invocation(Some("mkfs /dev/example"), &empty, "denied", &lease, None);
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &dangerous, &CancellationToken::new())
                .await,
            Err(ProcessError::Denied)
        ));
        let ask_with_unissued_digest =
            invocation(Some("printf ok"), &empty, "unissued", &lease, Some("wrong"));
        assert!(matches!(
            engine
                .execute_process(
                    run,
                    &lease,
                    3,
                    &ask_with_unissued_digest,
                    &CancellationToken::new()
                )
                .await,
            Err(ProcessError::InvalidApproval)
        ));

        for effect in [
            "both",
            "neither",
            "zero-cap",
            "wrong-owner",
            "file-cwd",
            "denied",
            "unissued",
        ] {
            assert!(
                engine.effect_status(effect).is_err(),
                "{effect} created a ledger row"
            );
        }

        let descriptor = ThreadEffectDescriptor {
            effect_id: "thread-process".into(),
            tool_call_id: "call".into(),
            name: "process".into(),
            input: serde_json::json!({"argv":["/bin/echo","ok"]}),
            attempt: 1,
        };
        let mut unsupported = engine.clone();
        unsupported.process_supervision_supported = false;
        assert!(matches!(
            unsupported
                .execute_started_thread_process(&descriptor, 0, &lease, &CancellationToken::new())
                .await,
            Err(ThreadEffectExecutionError::Certified(_))
        ));
        let gated = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        gated.operation_gate.close();
        assert!(matches!(
            gated
                .execute_started_thread_process(&descriptor, 0, &lease, &CancellationToken::new())
                .await,
            Err(ThreadEffectExecutionError::Uncertain(message)) if message.contains("gate closed")
        ));
        assert!(
            storage(crate::StorageError::LeaseLost)
                .to_string()
                .contains("durable sequencing")
        );
    }

    #[tokio::test]
    async fn permission_dual_stream_bounds_and_timeout_are_durable() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 10_000).unwrap();
        let empty = Vec::new();
        let command =
            "i=0; while [ $i -lt 1000 ]; do echo 123456789; echo abcdefghi >&2; i=$((i+1)); done";
        let ask = invocation(Some(command), &empty, "dual", &lease, None);
        let digest = match engine
            .execute_process(run, &lease, 3, &ask, &CancellationToken::new())
            .await
            .unwrap_err()
        {
            ProcessError::PermissionRequired { digest } => digest,
            _ => unreachable!(),
        };
        let approved = ProcessInvocation {
            approval_digest: Some(&digest),
            ..ask
        };
        let output = engine
            .execute_process(run, &lease, 4, &approved, &CancellationToken::new())
            .await
            .unwrap();
        assert!(output.stdout_truncated && output.stderr_truncated);
        assert_eq!(
            engine.effect_status("dual").unwrap(),
            EffectStatus::ObservedSuccess
        );
        let mut slow = invocation(Some("sleep 5"), &empty, "timeout", &lease, None);
        slow.timeout_ms = 30;
        let digest = match engine
            .execute_process(run, &lease, 5, &slow, &CancellationToken::new())
            .await
            .unwrap_err()
        {
            ProcessError::PermissionRequired { digest } => digest,
            _ => unreachable!(),
        };
        let slow = ProcessInvocation {
            approval_digest: Some(&digest),
            ..slow
        };
        let output = engine
            .execute_process(run, &lease, 6, &slow, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.termination, ProcessTermination::TimedOut);
        assert!(!output.command_succeeded());
        assert_eq!(
            engine.effect_status("timeout").unwrap(),
            EffectStatus::ObservedSuccess
        );
    }

    #[tokio::test]
    async fn cancellation_terminates_process_group_once() {
        let dir = tempfile::tempdir().unwrap();
        let pgid_file = dir.path().join("cancelled-process-group");
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 10_000).unwrap();
        let empty = Vec::new();
        let shell = format!("echo $$ > {}; sleep 10 & wait", pgid_file.to_string_lossy());
        let ask = invocation(Some(&shell), &empty, "cancel", &lease, None);
        let digest = match engine
            .execute_process(run, &lease, 3, &ask, &CancellationToken::new())
            .await
            .unwrap_err()
        {
            ProcessError::PermissionRequired { digest } => digest,
            _ => unreachable!(),
        };
        let approved = ProcessInvocation {
            approval_digest: Some(&digest),
            ..ask
        };
        let token = CancellationToken::new();
        let trigger = token.clone();
        let trigger_file = pgid_file.clone();
        tokio::spawn(async move {
            while std::fs::read_to_string(&trigger_file)
                .ok()
                .is_none_or(|value| value.trim().parse::<u32>().is_err())
            {
                sleep(Duration::from_millis(5)).await;
            }
            trigger.cancel();
        });
        let error = engine
            .execute_process(run, &lease, 4, &approved, &token)
            .await
            .unwrap_err();
        assert!(matches!(error, ProcessError::Supervision(_)));
        let pgid = std::fs::read_to_string(&pgid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert_group_gone_bounded(pgid).await;
        assert_eq!(
            engine.effect_status("cancel").unwrap(),
            EffectStatus::Unknown
        );
    }

    #[tokio::test]
    async fn leader_exit_still_kills_term_ignoring_pipe_holder_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 10_000).unwrap();
        let empty = Vec::new();
        let mut ask = invocation(
            Some("(trap '' TERM; sleep 10) & echo $$ $!; exit 0"),
            &empty,
            "leader-exit",
            &lease,
            None,
        );
        ask.grace_ms = 30;
        let digest = match engine
            .execute_process(run, &lease, 3, &ask, &CancellationToken::new())
            .await
            .unwrap_err()
        {
            ProcessError::PermissionRequired { digest } => digest,
            _ => unreachable!(),
        };
        let approved = ProcessInvocation {
            approval_digest: Some(&digest),
            ..ask
        };
        let started = tokio::time::Instant::now();
        let output = engine
            .execute_process(run, &lease, 4, &approved, &CancellationToken::new())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(12));
        assert_eq!(output.termination, ProcessTermination::Exited);
        let pids = output
            .stdout
            .split_whitespace()
            .map(|value| value.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 2);
        assert_group_gone_bounded(pids[0]).await;
    }

    #[tokio::test]
    async fn unsupported_preflight_creates_no_effect() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let unsupported = EngineHandle {
            process_supervision_supported: false,
            ..engine.clone()
        };
        let argv = vec!["echo".into(), "x".into()];
        let request = invocation(None, &argv, "unsupported-process", &lease, None);
        assert!(matches!(
            unsupported
                .execute_process(run, &lease, 3, &request, &CancellationToken::new())
                .await,
            Err(ProcessError::Unsupported)
        ));
        assert!(engine.effect_status("unsupported-process").is_err());
    }

    #[tokio::test]
    async fn path_and_git_hooks_cannot_turn_public_request_into_allow() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let fake = dir.path().join("git");
        std::fs::write(&fake, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let argv = vec!["git".into(), "diff".into(), "--ext-diff".into()];
        let env = BTreeMap::from([
            ("PATH".into(), dir.path().display().to_string()),
            ("GIT_EXTERNAL_DIFF".into(), fake.display().to_string()),
            ("LD_PRELOAD".into(), "evil.so".into()),
        ]);
        let request = ProcessInvocation {
            env: &env,
            ..invocation(None, &argv, "hooked-public", &lease, None)
        };
        assert!(matches!(
            engine
                .execute_process(run, &lease, 3, &request, &CancellationToken::new())
                .await,
            Err(ProcessError::PermissionRequired { .. })
        ));
        assert!(!marker.exists());
        assert_eq!(
            engine.effect_status("hooked-public").unwrap(),
            EffectStatus::Prepared
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn uncertain_group_probe_makes_started_effect_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let run = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let engine = EngineBuilder::new()
            .workspace_root(dir.path())
            .build()
            .unwrap();
        engine.create_run(run, 1).unwrap();
        let lease = engine.acquire_lease("owner", 2, 100).unwrap();
        let uncertain = EngineHandle {
            process_group_probe_override: Some(GroupProbe::Uncertain),
            ..engine.clone()
        };
        let argv = vec!["/bin/pwd".into()];
        let request = invocation(None, &argv, "uncertain-probe", &lease, None);
        assert!(matches!(
            uncertain
                .execute_process(run, &lease, 3, &request, &CancellationToken::new())
                .await,
            Err(ProcessError::Supervision(_))
        ));
        assert_eq!(
            engine.effect_status("uncertain-probe").unwrap(),
            EffectStatus::Unknown
        );

        assert!(group_pid(u32::MAX).is_err());
        for probe in [GroupProbe::Absent, GroupProbe::Uncertain] {
            assert_eq!(group_probe(0, Some(probe)).unwrap(), probe);
        }
        assert!(
            await_group_absent(0, 0, Some(GroupProbe::Absent))
                .await
                .unwrap()
        );
    }

    // -- Pure helper coverage ------------------------------------------------

    #[test]
    fn digest_is_stable_and_hex() {
        let argv = vec!["/bin/echo".into(), "hello".into()];
        let env = BTreeMap::from([("KEY".into(), "value".into())]);
        let invocation = ProcessInvocation {
            argv: &argv,
            shell: None,
            cwd: ".",
            env: &env,
            timeout_ms: 5000,
            grace_ms: 100,
            stdout_cap: 65536,
            stderr_cap: 1024,
            run_revision: 1,
            effect_id: "effect-1",
            attempt: 1,
            approval_digest: None,
            lease_owner: "owner",
            lease_token: 42,
        };
        let d1 = digest(&invocation);
        let d2 = digest(&invocation);
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
        assert!(d1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn group_pid_converts_u32_to_pid() {
        let pid = group_pid(12345).unwrap();
        assert_eq!(pid, nix::unistd::Pid::from_raw(12345));
    }

    #[test]
    fn storage_maps_error_to_supervision() {
        let error = crate::StorageError::InvalidData("test".into());
        let mapped = storage(error);
        match mapped {
            ProcessError::Supervision(msg) => assert!(msg.contains("test")),
            other => panic!("expected Supervision, got {other:?}"),
        }
    }

    #[test]
    fn supervise_git_diff_works_in_temp_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // Initialize a git repo with a committed file.
        std::fs::write(repo.join("file.txt"), "content\n").unwrap();
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(repo)
            .output()
            .unwrap();
        // Modify the file to produce a diff.
        std::fs::write(repo.join("file.txt"), "modified content\n").unwrap();
        let (output, truncated) = supervise_git_diff(repo, 65536).unwrap();
        assert!(!output.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn supervise_git_changed_files_lists_modified_and_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(repo.join("tracked.txt"), "content\n").unwrap();
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(repo)
            .output()
            .unwrap();
        // Modify tracked + add untracked.
        std::fs::write(repo.join("tracked.txt"), "modified\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "new\n").unwrap();
        let (output, _truncated) = supervise_git_changed_files(repo, 65536).unwrap();
        assert!(output.contains("tracked.txt"));
        assert!(output.contains("untracked.txt"));
    }

    #[tokio::test]
    async fn drain_reads_until_eof_and_truncates() {
        let data = b"hello world";
        let (output, truncated) = drain(&data[..], 100).await.unwrap();
        assert_eq!(output, "hello world");
        assert!(!truncated);
        // Truncation when cap is smaller than data.
        let (output, truncated) = drain(&data[..], 5).await.unwrap();
        assert_eq!(output, "hello");
        assert!(truncated);
        // Empty input.
        let (output, truncated) = drain(&b""[..], 100).await.unwrap();
        assert_eq!(output, "");
        assert!(!truncated);
    }

    #[test]
    fn group_probe_returns_override() {
        assert_eq!(
            group_probe(99999, Some(GroupProbe::Present)).unwrap(),
            GroupProbe::Present
        );
        assert_eq!(
            group_probe(99999, Some(GroupProbe::Absent)).unwrap(),
            GroupProbe::Absent
        );
        assert_eq!(
            group_probe(99999, Some(GroupProbe::Uncertain)).unwrap(),
            GroupProbe::Uncertain
        );
    }

    #[tokio::test]
    async fn shutdown_group_succeeds_when_already_absent() {
        // probe_override=Absent means the group is already gone → immediate success.
        shutdown_group(99999, 10, Some(GroupProbe::Absent))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_group_fails_when_uncertain() {
        let result = shutdown_group(99999, 10, Some(GroupProbe::Uncertain)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn await_group_absent_returns_true_when_absent() {
        let result = await_group_absent(99999, 10, Some(GroupProbe::Absent))
            .await
            .unwrap();
        assert!(result);
    }
}
