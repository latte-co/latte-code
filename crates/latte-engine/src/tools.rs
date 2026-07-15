use crate::{
    policy::{
        self, EffectClass, OperationBinding, PendingApproval, PermissionPolicy, PolicyDecision,
    },
    workspace::{FileIdentity, PathError, WorkspacePath},
};
use ignore::WalkBuilder;
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::Mutex,
};
use thiserror::Error;

const DEFAULT_CAP: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub version: u32,
    pub effect: String,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub truncated: bool,
}
#[derive(Clone, Debug)]
pub struct ToolInvocation<'a> {
    pub name: &'a str,
    pub input: &'a Value,
    pub run_revision: u64,
    pub effect_id: &'a str,
    pub attempt: u64,
    pub precondition: Option<&'a str>,
    pub timeout_ms: u64,
    pub output_cap: usize,
    pub approval_digest: Option<&'a str>,
    pub lease_owner: &'a str,
    pub lease_token: u64,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown or disabled tool: {0}")]
    Unknown(String),
    #[error("invalid tool input: {0}")]
    Input(String),
    #[error("workspace path rejected: {0}")]
    Path(String),
    #[error("policy denied target {0}")]
    Denied(String),
    #[error("permission required for target {target}")]
    PermissionRequired { target: String, digest: String },
    #[error("approval is stale, mismatched, or already consumed")]
    InvalidApproval,
    #[error("fresh read snapshot required for {0}")]
    ReadRequired(String),
    #[error("file changed since read: {0}")]
    Stale(String),
    #[error("edit match count must be exactly one, found {0}")]
    MatchCount(usize),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid regex: {0}")]
    Regex(#[from] regex::Error),
    #[error("git diff failed: {0}")]
    Git(String),
    #[error("workspace remained unstable while sampling: {0}")]
    WorkspaceUnstable(String),
    #[error("workspace contains an unsafe symbolic link: {0}")]
    WorkspaceUnsafe(String),
}
impl From<PathError> for ToolError {
    fn from(value: PathError) -> Self {
        Self::Path(value.to_string())
    }
}

trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    fn prepare(&self, input: &Value, context: &Context<'_>) -> Result<Prepared, ToolError>;
}
struct Context<'a> {
    workspace: &'a WorkspacePath,
}
pub(crate) struct Prepared {
    target: String,
    effect: EffectClass,
    action: Action,
    expected_hash: Option<String>,
}
enum Action {
    Read {
        path: std::path::PathBuf,
        max: usize,
    },
    List {
        path: std::path::PathBuf,
        max: usize,
    },
    Search {
        query: String,
        regex: bool,
        max_results: usize,
        max_output: usize,
    },
    Manifest {
        max: usize,
    },
    Edit {
        path: std::path::PathBuf,
        before: String,
        after: String,
    },
    Write {
        path: std::path::PathBuf,
        content: String,
        create: bool,
    },
    GitDiff {
        max: usize,
    },
}

struct Builtin {
    name: &'static str,
    effect: EffectClass,
}
impl Tool for Builtin {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.into(),
            description: format!("Engine-owned {} operation", self.name.replace('_', " ")),
            input_schema: tool_schema(self.name),
            version: 1,
            effect: format!("{:?}", self.effect).to_lowercase(),
        }
    }
    fn prepare(&self, input: &Value, context: &Context<'_>) -> Result<Prepared, ToolError> {
        let path = || string(input, "path");
        let max = || usize_field(input, "max_output", DEFAULT_CAP, 1, DEFAULT_CAP);
        let (target, action) = match self.name {
            "read_file" => {
                let p = path()?;
                (
                    context.workspace.display(p)?,
                    Action::Read {
                        path: context.workspace.read(p)?,
                        max: max()?,
                    },
                )
            }
            "list_directory" => {
                let p = path()?;
                (
                    context.workspace.display(p)?,
                    Action::List {
                        path: context.workspace.read(p)?,
                        max: usize_field(input, "max_entries", 1000, 1, 10_000)?,
                    },
                )
            }
            "search" => (
                ".".into(),
                Action::Search {
                    query: string(input, "query")?.into(),
                    regex: bool_field(input, "regex", false)?,
                    max_results: usize_field(input, "max_results", 100, 1, 10_000)?,
                    max_output: max()?,
                },
            ),
            "read_project_manifest" => (".".into(), Action::Manifest { max: max()? }),
            "edit_file" => {
                let p = path()?;
                let before = input
                    .get("before")
                    .or_else(|| input.get("anchor"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ToolError::Input("before or anchor must be a non-empty string".into())
                    })?;
                (
                    context.workspace.display(p)?,
                    Action::Edit {
                        path: context.workspace.mutation(p, false)?,
                        before: before.into(),
                        after: string(input, "after")?.into(),
                    },
                )
            }
            "write_file" => {
                let p = path()?;
                let create = bool_field(input, "create_intent", false)?;
                (
                    context.workspace.display(p)?,
                    Action::Write {
                        path: context.workspace.mutation(p, create)?,
                        content: string(input, "content")?.into(),
                        create,
                    },
                )
            }
            "git_diff" => (".".into(), Action::GitDiff { max: max()? }),
            _ => return Err(ToolError::Unknown(self.name.into())),
        };
        let effect = if matches!(&action, Action::Write { create: true, .. }) {
            EffectClass::Create
        } else {
            self.effect
        };
        Ok(Prepared {
            target,
            effect,
            action,
            expected_hash: None,
        })
    }
}

pub(crate) fn tool_schema(name: &str) -> Value {
    let path = || json!({"type":"string","minLength":1,"maxLength":4096});
    let cap = || json!({"type":"integer","minimum":1,"maximum":65536});
    let digest = || json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    match name {
        "read_file" => {
            json!({"type":"object","required":["path"],"properties":{"path":path(),"max_output":cap()},"additionalProperties":false})
        }
        "list_directory" => {
            json!({"type":"object","required":["path"],"properties":{"path":path(),"max_entries":{"type":"integer","minimum":1,"maximum":10000}},"additionalProperties":false})
        }
        "search" => {
            json!({"type":"object","required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":4096},"regex":{"type":"boolean"},"max_results":{"type":"integer","minimum":1,"maximum":10000},"max_output":cap()},"additionalProperties":false})
        }
        "read_project_manifest" | "git_diff" => {
            json!({"type":"object","required":[],"properties":{"max_output":cap()},"additionalProperties":false})
        }
        "edit_file" => {
            json!({"type":"object","required":["path","after","precondition"],"properties":{"path":path(),"before":{"type":"string","minLength":1},"anchor":{"type":"string","minLength":1},"after":{"type":"string"},"precondition":digest()},"anyOf":[{"required":["before"]},{"required":["anchor"]}],"additionalProperties":false})
        }
        "write_file" => {
            json!({"type":"object","required":["path","content","create_intent"],"properties":{"path":path(),"content":{"type":"string"},"create_intent":{"type":"boolean"},"precondition":digest()},"additionalProperties":false})
        }
        "process" => {
            json!({"type":"object","required":[],"properties":{"argv":{"type":"array","minItems":1,"maxItems":256,"items":{"type":"string","minLength":1,"maxLength":4096}},"shell":{"type":"string","minLength":1,"maxLength":16384},"cwd":path(),"env":{"type":"object","maxProperties":128,"additionalProperties":{"type":"string","maxLength":16384}},"timeout_ms":{"type":"integer","minimum":1,"maximum":600_000},"grace_ms":{"type":"integer","minimum":0,"maximum":30_000},"stdout_cap":{"type":"integer","minimum":1,"maximum":1_048_576},"stderr_cap":{"type":"integer","minimum":1,"maximum":1_048_576}},"oneOf":[{"required":["argv"]},{"required":["shell"]}],"additionalProperties":false})
        }
        _ => json!({"type":"object","required":[],"properties":{},"additionalProperties":false}),
    }
}

pub(crate) struct ToolRegistry {
    workspace: WorkspacePath,
    database_path: Option<std::path::PathBuf>,
    policy: PermissionPolicy,
    tools: BTreeMap<String, Box<dyn Tool>>,
    snapshots: Mutex<BTreeMap<String, FileIdentity>>,
    approvals: Mutex<BTreeMap<String, PendingApproval>>,
}
impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("root", &self.workspace.root())
            .field("tools", &self.tools.keys())
            .finish_non_exhaustive()
    }
}
impl ToolRegistry {
    pub(crate) fn workspace_manifest(&self) -> Result<BTreeMap<String, String>, ToolError> {
        let mut result = BTreeMap::new();
        for entry in WalkBuilder::new(self.workspace.root())
            .hidden(false)
            .filter_entry(|entry| {
                !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".latte" | "target")
                )
            })
            .build()
        {
            let entry = entry.map_err(|e| ToolError::Io(std::io::Error::other(e)))?;
            if self.is_database_artifact(entry.path()) {
                continue;
            }
            let relative = self.utf8_relative(entry.path())?;
            let file_type = entry.file_type();
            if file_type.is_some_and(|kind| kind.is_symlink()) {
                let before = fs::symlink_metadata(entry.path())?;
                let raw_target = fs::read_link(entry.path())?;
                let raw_target_components = Self::raw_target_components(&raw_target)?;
                let normalized = self.safe_symlink_target(entry.path(), &raw_target)?;
                let canonical = fs::canonicalize(entry.path())
                    .map_err(|error| ToolError::WorkspaceUnsafe(format!("{relative}: {error}")))?;
                if !canonical.starts_with(self.workspace.root()) {
                    return Err(ToolError::WorkspaceUnsafe(format!(
                        "{relative}: target escapes workspace"
                    )));
                }
                let target_identity = if canonical.is_file() {
                    Some(WorkspacePath::identity(&canonical)?.hash)
                } else {
                    None
                };
                let after_target = fs::read_link(entry.path())?;
                let after = fs::symlink_metadata(entry.path())?;
                if raw_target != after_target
                    || before.len() != after.len()
                    || before.modified()? != after.modified()?
                {
                    return Err(ToolError::WorkspaceUnstable(relative));
                }
                let record = serde_json::json!({
                    "path": relative,
                    "type": "symlink",
                    "raw_target": { "absolute": false, "components": raw_target_components },
                    "normalized_target": normalized,
                    "target_identity": target_identity,
                });
                if result
                    .insert(
                        relative,
                        format!(
                            "{:x}",
                            Sha256::digest(
                                serde_json::to_vec(&record)
                                    .map_err(|error| ToolError::Input(error.to_string()))?
                            )
                        ),
                    )
                    .is_some()
                {
                    return Err(ToolError::WorkspaceUnsafe("duplicate manifest path".into()));
                }
                continue;
            }
            if !file_type.is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let before = fs::metadata(entry.path())?;
            let before_modified = before.modified()?;
            let bytes = fs::read(entry.path())?;
            let after = fs::metadata(entry.path())?;
            if before.len() != after.len() || before_modified != after.modified()? {
                return Err(ToolError::WorkspaceUnstable(relative));
            }
            if result
                .insert(relative, format!("{:x}", Sha256::digest(bytes)))
                .is_some()
            {
                return Err(ToolError::WorkspaceUnsafe("duplicate manifest path".into()));
            }
        }
        Ok(result)
    }
    fn safe_symlink_target(&self, link: &Path, target: &Path) -> Result<Vec<String>, ToolError> {
        if target.is_absolute() {
            return Err(ToolError::WorkspaceUnsafe(format!(
                "{}: absolute target",
                link.display()
            )));
        }
        let parent = link
            .parent()
            .ok_or_else(|| ToolError::WorkspaceUnsafe(link.display().to_string()))?;
        let joined = parent.join(target);
        let mut normalized = std::path::PathBuf::new();
        for component in joined.components() {
            match component {
                std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                    normalized.push(component);
                }
                std::path::Component::CurDir => {}
                std::path::Component::Normal(value) => normalized.push(value),
                std::path::Component::ParentDir => {
                    if !normalized.pop() || !normalized.starts_with(self.workspace.root()) {
                        return Err(ToolError::WorkspaceUnsafe(format!(
                            "{}: parent escape",
                            link.display()
                        )));
                    }
                }
            }
        }
        if !normalized.starts_with(self.workspace.root()) {
            return Err(ToolError::WorkspaceUnsafe(format!(
                "{}: target escape",
                link.display()
            )));
        }
        let relative = normalized
            .strip_prefix(self.workspace.root())
            .map_err(|error| ToolError::WorkspaceUnsafe(error.to_string()))?;
        Self::normal_components(relative)
    }
    fn utf8_relative(&self, path: &Path) -> Result<String, ToolError> {
        let relative = path
            .strip_prefix(self.workspace.root())
            .map_err(|error| ToolError::Path(error.to_string()))?;
        serde_json::to_string(&Self::normal_components(relative)?)
            .map_err(|error| ToolError::Input(error.to_string()))
    }
    fn normal_components(path: &Path) -> Result<Vec<String>, ToolError> {
        let mut values = Vec::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(value) => values.push(
                    value
                        .to_str()
                        .ok_or_else(|| ToolError::WorkspaceUnsafe("NonUtf8Path".into()))?
                        .to_owned(),
                ),
                std::path::Component::CurDir => {}
                std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::ParentDir => {
                    return Err(ToolError::WorkspaceUnsafe(
                        "non-relative manifest path".into(),
                    ));
                }
            }
        }
        Ok(values)
    }
    fn raw_target_components(target: &Path) -> Result<Vec<serde_json::Value>, ToolError> {
        target
            .components()
            .map(|component| match component {
                std::path::Component::Normal(value) => Ok(serde_json::json!([
                    "normal",
                    value
                        .to_str()
                        .ok_or_else(|| ToolError::WorkspaceUnsafe("NonUtf8Path".into()))?
                ])),
                std::path::Component::CurDir => Ok(serde_json::json!(["current"])),
                std::path::Component::ParentDir => Ok(serde_json::json!(["parent"])),
                std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                    Err(ToolError::WorkspaceUnsafe("absolute symlink target".into()))
                }
            })
            .collect()
    }
    fn is_database_artifact(&self, path: &Path) -> bool {
        self.database_path.as_ref().is_some_and(|database| {
            if path == database {
                return true;
            }
            let Some(parent) = database.parent() else {
                return false;
            };
            if path.parent() != Some(parent) {
                return false;
            }
            let Some(name) = database.file_name() else {
                return false;
            };
            let Some(candidate) = path.file_name() else {
                return false;
            };
            ["-wal", "-shm", "-journal"].iter().any(|suffix| {
                let mut expected = name.to_os_string();
                expected.push(suffix);
                candidate == expected
            })
        })
    }
    pub(crate) fn changed_files(&self) -> Result<Vec<String>, ToolError> {
        let (text, truncated) =
            crate::process::supervise_git_changed_files(self.workspace.root(), 64 * 1024)
                .map_err(|e| ToolError::Git(e.to_string()))?;
        if truncated {
            return Err(ToolError::Git("changed file list exceeded cap".into()));
        }
        Ok(text
            .lines()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect())
    }
    pub(crate) fn resolve_cwd(&self, path: &str) -> Result<std::path::PathBuf, ToolError> {
        let resolved = self.workspace.read(path)?;
        if !resolved.is_dir() {
            return Err(ToolError::Input("process cwd must be a directory".into()));
        }
        Ok(resolved)
    }
    pub(crate) fn new(
        root: &Path,
        enabled: Option<&BTreeSet<String>>,
        denies: &[String],
        database_path: Option<&Path>,
    ) -> Result<Self, ToolError> {
        let workspace = WorkspacePath::new(root)?;
        let policy =
            PermissionPolicy::new(denies, 1).map_err(|e| ToolError::Input(e.to_string()))?;
        let specs = [
            ("read_file", EffectClass::Read),
            ("list_directory", EffectClass::Read),
            ("search", EffectClass::Read),
            ("read_project_manifest", EffectClass::Read),
            ("edit_file", EffectClass::Modify),
            ("write_file", EffectClass::Modify),
            ("git_diff", EffectClass::Read),
        ];
        let tools = specs
            .into_iter()
            .filter(|(n, _)| enabled.is_none_or(|set| set.contains(*n)))
            .map(|(n, e)| {
                (
                    n.into(),
                    Box::new(Builtin { name: n, effect: e }) as Box<dyn Tool>,
                )
            })
            .collect();
        Ok(Self {
            workspace,
            database_path: database_path.map(|path| {
                path.parent()
                    .and_then(|parent| std::fs::canonicalize(parent).ok())
                    .and_then(|parent| path.file_name().map(|name| parent.join(name)))
                    .unwrap_or_else(|| path.to_path_buf())
            }),
            policy,
            tools,
            snapshots: Mutex::new(BTreeMap::new()),
            approvals: Mutex::new(BTreeMap::new()),
        })
    }
    #[cfg(test)]
    pub(crate) fn new_with_safe_support(
        root: &Path,
        enabled: Option<&BTreeSet<String>>,
        denies: &[String],
        supported: bool,
    ) -> Result<Self, ToolError> {
        let mut registry = Self::new(root, enabled, denies, None)?;
        registry.workspace = WorkspacePath::new_with_safe_support(root, supported)?;
        Ok(registry)
    }
    pub(crate) fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools.values().map(|t| t.descriptor()).collect()
    }
    pub(crate) fn prepare_for_engine(
        &self,
        invocation: &ToolInvocation<'_>,
    ) -> Result<(Prepared, PolicyDecision, String), ToolError> {
        let tool = self
            .tools
            .get(invocation.name)
            .ok_or_else(|| ToolError::Unknown(invocation.name.into()))?;
        let mut prepared = tool.prepare(
            invocation.input,
            &Context {
                workspace: &self.workspace,
            },
        )?;
        if prepared.effect != EffectClass::Read {
            self.workspace.validate_mutation_support()?;
        }
        if prepared.effect == EffectClass::Modify && invocation.precondition.is_none() {
            return Err(ToolError::ReadRequired(prepared.target));
        }
        prepared.expected_hash = invocation.precondition.map(str::to_owned);
        let digest = policy::digest(&OperationBinding {
            descriptor_version: tool.descriptor().version,
            run_revision: invocation.run_revision,
            effect_id: invocation.effect_id,
            attempt: invocation.attempt,
            tool: invocation.name,
            target: &prepared.target,
            input: invocation.input,
            precondition: invocation.precondition,
            timeout_ms: invocation.timeout_ms,
            output_cap: invocation.output_cap,
            policy_version: self.policy.version,
            lease_owner: invocation.lease_owner,
            lease_token: invocation.lease_token,
        });
        let decision = self.policy.decide(prepared.effect, &prepared.target);
        Ok((prepared, decision, digest))
    }
    pub(crate) fn execute_prepared(&self, prepared: Prepared) -> Result<ToolOutput, ToolError> {
        self.execute(prepared)
    }
    #[allow(dead_code)]
    pub(crate) fn invoke(&self, invocation: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
        let tool = self
            .tools
            .get(invocation.name)
            .ok_or_else(|| ToolError::Unknown(invocation.name.into()))?;
        let prepared = tool.prepare(
            invocation.input,
            &Context {
                workspace: &self.workspace,
            },
        )?;
        let digest = policy::digest(&OperationBinding {
            descriptor_version: tool.descriptor().version,
            run_revision: invocation.run_revision,
            effect_id: invocation.effect_id,
            attempt: invocation.attempt,
            tool: invocation.name,
            target: &prepared.target,
            input: invocation.input,
            precondition: invocation.precondition,
            timeout_ms: invocation.timeout_ms,
            output_cap: invocation.output_cap,
            policy_version: self.policy.version,
            lease_owner: invocation.lease_owner,
            lease_token: invocation.lease_token,
        });
        match self.policy.decide(prepared.effect, &prepared.target) {
            PolicyDecision::Deny => return Err(ToolError::Denied(prepared.target)),
            PolicyDecision::Ask => {
                let mut approvals = self.approvals.lock().unwrap();
                if let Some(supplied) = invocation.approval_digest {
                    approvals
                        .get_mut(&digest)
                        .ok_or(ToolError::InvalidApproval)?
                        .consume(supplied)
                        .map_err(|_| ToolError::InvalidApproval)?;
                } else {
                    approvals.insert(digest.clone(), PendingApproval::new(digest.clone()));
                    return Err(ToolError::PermissionRequired {
                        target: prepared.target,
                        digest,
                    });
                }
            }
            PolicyDecision::Allow => {}
        }
        self.execute(prepared)
    }
    #[allow(clippy::too_many_lines)]
    fn execute(&self, prepared: Prepared) -> Result<ToolOutput, ToolError> {
        match prepared.action {
            Action::Read { path, max } => {
                let bytes = fs::read(&path)?;
                let id = WorkspacePath::identity(&path)?;
                self.snapshots
                    .lock()
                    .unwrap()
                    .insert(prepared.target, id.clone());
                let (text, truncated) = bounded(&String::from_utf8_lossy(&bytes), max);
                Ok(ToolOutput {
                    value: json!({"content":text,"sha256":id.hash,"size":id.size,"modified_ns":id.modified_ns}),
                    truncated,
                })
            }
            Action::List { path, max } => {
                let mut entries = fs::read_dir(path)?
                    .map(|e| e.map(|v| v.file_name().to_string_lossy().into_owned()))
                    .collect::<Result<Vec<_>, _>>()?;
                entries.sort();
                let truncated = entries.len() > max;
                entries.truncate(max);
                Ok(ToolOutput {
                    value: json!({"entries":entries}),
                    truncated,
                })
            }
            Action::Search {
                query,
                regex,
                max_results,
                max_output,
            } => {
                let matcher = regex.then(|| Regex::new(&query)).transpose()?;
                let mut results = Vec::new();
                let mut used = 0;
                let mut truncated = false;
                for entry in WalkBuilder::new(self.workspace.root())
                    .hidden(false)
                    .git_ignore(true)
                    .build()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                {
                    let Ok(text) = fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    for (i, line) in text.lines().enumerate() {
                        if matcher
                            .as_ref()
                            .map_or_else(|| line.contains(&query), |r| r.is_match(line))
                        {
                            let rel = entry
                                .path()
                                .strip_prefix(self.workspace.root())
                                .unwrap()
                                .to_string_lossy()
                                .replace('\\', "/");
                            let item = format!("{rel}:{}:{line}", i + 1);
                            if results.len() == max_results || used + item.len() > max_output {
                                truncated = true;
                                break;
                            }
                            used += item.len();
                            results.push(item);
                        }
                    }
                    if truncated {
                        break;
                    }
                }
                Ok(ToolOutput {
                    value: json!({"matches":results}),
                    truncated,
                })
            }
            Action::Manifest { max } => {
                let names = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
                let mut found = serde_json::Map::new();
                let mut truncated = false;
                for name in names {
                    let p = self.workspace.root().join(name);
                    if p.is_file() {
                        let data = fs::read_to_string(p)?;
                        let (v, t) = bounded(&data, max);
                        truncated |= t;
                        found.insert(name.into(), Value::String(v));
                    }
                }
                Ok(ToolOutput {
                    value: Value::Object(found),
                    truncated,
                })
            }
            Action::Edit {
                path,
                before,
                after,
            } => {
                let checked = self.workspace.mutation(&prepared.target, false)?;
                if checked != path {
                    return Err(ToolError::Path(
                        "target identity changed before mutation".into(),
                    ));
                }
                self.require_fresh(&prepared.target, &path, prepared.expected_hash.as_deref())?;
                let content = fs::read_to_string(&path)?;
                let count = content.match_indices(&before).count();
                if count != 1 {
                    return Err(ToolError::MatchCount(count));
                }
                let next = content.replacen(&before, &after, 1);
                self.workspace
                    .atomic_replace(&prepared.target, next.as_bytes(), false)?;
                let id = WorkspacePath::identity(&path)?;
                Ok(ToolOutput {
                    value: json!({"sha256":id.hash,"size":id.size}),
                    truncated: false,
                })
            }
            Action::Write {
                path,
                content,
                create,
            } => {
                let checked = self.workspace.mutation(&prepared.target, create)?;
                if checked != path {
                    return Err(ToolError::Path(
                        "target identity changed before mutation".into(),
                    ));
                }
                if create {
                    if path.exists() {
                        return Err(ToolError::Input(
                            "create_intent requires a missing target".into(),
                        ));
                    }
                } else {
                    self.require_fresh(&prepared.target, &path, prepared.expected_hash.as_deref())?;
                }
                self.workspace
                    .atomic_replace(&prepared.target, content.as_bytes(), create)?;
                let id = WorkspacePath::identity(&path)?;
                Ok(ToolOutput {
                    value: json!({"sha256":id.hash,"size":id.size}),
                    truncated: false,
                })
            }
            Action::GitDiff { max } => {
                let (text, truncated) =
                    crate::process::supervise_git_diff(self.workspace.root(), max)
                        .map_err(|e| ToolError::Git(e.to_string()))?;
                Ok(ToolOutput {
                    value: json!({"summary":text}),
                    truncated,
                })
            }
        }
    }
    fn require_fresh(
        &self,
        target: &str,
        path: &Path,
        expected_hash: Option<&str>,
    ) -> Result<(), ToolError> {
        let actual = WorkspacePath::identity(path)?;
        if let Some(hash) = expected_hash {
            if actual.hash != hash {
                return Err(ToolError::Stale(target.into()));
            }
            return Ok(());
        }
        let expected = self
            .snapshots
            .lock()
            .unwrap()
            .get(target)
            .cloned()
            .ok_or_else(|| ToolError::ReadRequired(target.into()))?;
        if actual != expected {
            return Err(ToolError::Stale(target.into()));
        }
        Ok(())
    }
}

fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::Input(format!("{key} must be a non-empty string")))
}
fn bool_field(v: &Value, key: &str, default: bool) -> Result<bool, ToolError> {
    v.get(key).map_or(Ok(default), |x| {
        x.as_bool()
            .ok_or_else(|| ToolError::Input(format!("{key} must be boolean")))
    })
}
fn usize_field(
    v: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ToolError> {
    let n = v.get(key).map_or(Ok(default), |x| {
        x.as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| ToolError::Input(format!("{key} must be an integer")))
    })?;
    if !(min..=max).contains(&n) {
        return Err(ToolError::Input(format!("{key} is out of bounds")));
    }
    Ok(n)
}
fn bounded(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.into(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].into(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn setup() -> (TempDir, ToolRegistry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\none\n").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        let registry = ToolRegistry::new(dir.path(), None, &[], None).unwrap();
        (dir, registry)
    }
    fn call<'a>(name: &'a str, input: &'a Value, approval: Option<&'a str>) -> ToolInvocation<'a> {
        ToolInvocation {
            name,
            input,
            run_revision: 4,
            effect_id: "effect-1",
            attempt: 1,
            precondition: None,
            timeout_ms: 1_000,
            output_cap: DEFAULT_CAP,
            approval_digest: approval,
            lease_owner: "owner",
            lease_token: 1,
        }
    }
    fn approve(
        registry: &ToolRegistry,
        name: &str,
        input: &Value,
    ) -> Result<ToolOutput, ToolError> {
        let digest = match registry.invoke(&call(name, input, None)).unwrap_err() {
            ToolError::PermissionRequired { digest, .. } => digest,
            other => panic!("unexpected {other}"),
        };
        registry.invoke(&call(name, input, Some(&digest)))
    }

    #[test]
    fn reads_lists_searches_manifests_and_bounds_output() {
        let (_dir, registry) = setup();
        let read = registry
            .invoke(&call(
                "read_file",
                &json!({"path":"a.txt","max_output":3}),
                None,
            ))
            .unwrap();
        assert!(read.truncated);
        assert_eq!(read.value["content"], "one");
        assert_eq!(read.value["sha256"].as_str().unwrap().len(), 64);
        assert!(
            registry
                .invoke(&call(
                    "list_directory",
                    &json!({"path":".","max_entries":1}),
                    None
                ))
                .unwrap()
                .truncated
        );
        let search = registry
            .invoke(&call(
                "search",
                &json!({"query":"t.o","regex":true,"max_results":1}),
                None,
            ))
            .unwrap();
        assert_eq!(search.value["matches"].as_array().unwrap().len(), 1);
        assert!(
            registry
                .invoke(&call("read_project_manifest", &json!({}), None))
                .unwrap()
                .value
                .get("Cargo.toml")
                .is_some()
        );
    }

    #[test]
    fn rejects_escape_denies_and_disabled_tools() {
        let (dir, _) = setup();
        let registry = ToolRegistry::new(
            dir.path(),
            Some(&BTreeSet::from(["read_file".into()])),
            &["a.txt".into()],
            None,
        )
        .unwrap();
        assert!(matches!(
            registry.invoke(&call("read_file", &json!({"path":"a.txt"}), None)),
            Err(ToolError::Denied(_))
        ));
        assert!(matches!(
            registry.invoke(&call("read_file", &json!({"path":"../a.txt"}), None)),
            Err(ToolError::Path(_))
        ));
        assert!(matches!(
            registry.invoke(&call("read_file", &json!({"path":"/etc/passwd"}), None)),
            Err(ToolError::Path(_))
        ));
        assert!(matches!(
            registry.invoke(&call("search", &json!({"query":"x"}), None)),
            Err(ToolError::Unknown(_))
        ));
        assert_eq!(registry.descriptors().len(), 1);
    }

    #[test]
    fn edit_and_write_require_reads_exact_matches_and_single_use_approval() {
        let (dir, registry) = setup();
        assert!(matches!(
            approve(
                &registry,
                "write_file",
                &json!({"path":"a.txt","content":"x"})
            ),
            Err(ToolError::ReadRequired(_))
        ));
        registry
            .invoke(&call("read_file", &json!({"path":"a.txt"}), None))
            .unwrap();
        assert!(matches!(
            approve(
                &registry,
                "edit_file",
                &json!({"path":"a.txt","before":"one","after":"x"})
            ),
            Err(ToolError::MatchCount(2))
        ));
        approve(
            &registry,
            "edit_file",
            &json!({"path":"a.txt","anchor":"two","after":"2"}),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n2\none\n"
        );
        registry
            .invoke(&call("read_file", &json!({"path":"a.txt"}), None))
            .unwrap();
        assert!(
            approve(
                &registry,
                "write_file",
                &json!({"path":"new.txt","content":"n","create_intent":true})
            )
            .is_ok()
        );
        assert!(matches!(
            approve(
                &registry,
                "write_file",
                &json!({"path":"new.txt","content":"n","create_intent":true})
            ),
            Err(ToolError::Input(_))
        ));
        let input = json!({"path":"a.txt","content":"z"});
        let ToolError::PermissionRequired { digest, .. } = registry
            .invoke(&call("write_file", &input, None))
            .unwrap_err()
        else {
            unreachable!()
        };
        assert!(
            registry
                .invoke(&call("write_file", &input, Some("wrong")))
                .is_err()
        );
        registry
            .invoke(&call("write_file", &input, Some(&digest)))
            .unwrap();
        assert!(matches!(
            registry.invoke(&call("write_file", &input, Some(&digest))),
            Err(ToolError::InvalidApproval)
        ));
    }

    #[test]
    fn detects_stale_snapshot() {
        let (dir, registry) = setup();
        registry
            .invoke(&call("read_file", &json!({"path":"a.txt"}), None))
            .unwrap();
        fs::write(dir.path().join("a.txt"), "changed").unwrap();
        assert!(matches!(
            approve(
                &registry,
                "write_file",
                &json!({"path":"a.txt","content":"new"})
            ),
            Err(ToolError::Stale(_))
        ));
    }

    #[test]
    fn git_diff_is_read_only_summary_and_bounded() {
        let (dir, registry) = setup();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let marker = dir.path().join("textconv-ran");
            let helper = dir.path().join("textconv.sh");
            fs::write(
                &helper,
                format!("#!/bin/sh\ntouch {}\ncat \"$1\"\n", marker.display()),
            )
            .unwrap();
            let mut mode = fs::metadata(&helper).unwrap().permissions();
            mode.set_mode(0o755);
            fs::set_permissions(&helper, mode).unwrap();
            fs::write(dir.path().join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        }
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["add", "."],
            vec!["commit", "-qm", "base"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        #[cfg(unix)]
        assert!(
            Command::new("git")
                .args([
                    "config",
                    "diff.evil.textconv",
                    dir.path().join("textconv.sh").to_str().unwrap()
                ])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join("a.txt"), "a substantially changed file\n").unwrap();
        let result = registry.invoke(&call("git_diff", &json!({"max_output":8}), None));
        #[cfg(unix)]
        {
            let output = result.unwrap();
            assert!(output.truncated);
            assert!(output.value["summary"].as_str().is_some());
            assert!(!dir.path().join("textconv-ran").exists());
        }
        #[cfg(not(unix))]
        assert!(matches!(
            result,
            Err(ToolError::Git(message)) if message.contains("unsupported on this platform")
        ));
    }

    #[cfg(unix)]
    #[test]
    #[rustfmt::skip]
    fn manifest_tracks_internal_symlink_topology_and_rejects_unsafe_links() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "same").unwrap();
        fs::write(dir.path().join("b"), "same").unwrap();
        symlink("a", dir.path().join("link")).unwrap();
        let registry = ToolRegistry::new(dir.path(), None, &[], None).unwrap();
        let link_key = r#"["link"]"#;
        let first = registry.workspace_manifest().unwrap();
        assert!(first.contains_key(link_key));
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("b", dir.path().join("link")).unwrap();
        let second = registry.workspace_manifest().unwrap();
        assert_ne!(first[link_key], second[link_key]);

        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("missing", dir.path().join("link")).unwrap();
        assert!(matches!(
            registry.workspace_manifest(),
            Err(ToolError::WorkspaceUnsafe(_))
        ));
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("/tmp", dir.path().join("link")).unwrap();
        assert!(matches!(
            registry.workspace_manifest(),
            Err(ToolError::WorkspaceUnsafe(_))
        ));
        fs::remove_file(dir.path().join("link")).unwrap(); symlink("/tmp", dir.path().join("inside")).unwrap(); symlink("inside", dir.path().join("link")).unwrap(); assert!(matches!(registry.workspace_manifest(), Err(ToolError::WorkspaceUnsafe(_)))); fs::remove_file(dir.path().join("link")).unwrap(); fs::remove_file(dir.path().join("inside")).unwrap();
        symlink("../outside", dir.path().join("link")).unwrap();
        assert!(matches!(
            registry.workspace_manifest(),
            Err(ToolError::WorkspaceUnsafe(_))
        ));
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("link", dir.path().join("link")).unwrap();
        assert!(matches!(
            registry.workspace_manifest(),
            Err(ToolError::WorkspaceUnsafe(_))
        ));

        fs::remove_file(dir.path().join("link")).unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        fs::write(dir.path().join("folder/file"), "content").unwrap();
        symlink("folder", dir.path().join("link")).unwrap();
        let directory = registry.workspace_manifest().unwrap();
        assert!(directory.contains_key(link_key));
        assert!(directory.contains_key(r#"["folder","file"]"#));
        assert!(!directory.contains_key(r#"["link","file"]"#));
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_non_utf8_paths_and_link_targets_without_lossy_collisions() {
        use std::{
            ffi::OsString,
            os::unix::{ffi::OsStringExt, fs::symlink},
        };

        for byte in [0x80, 0x81] {
            let dir = tempfile::tempdir().unwrap();
            let name = OsString::from_vec(vec![byte]);
            if fs::write(dir.path().join(&name), "same").is_err() {
                // Some Unix filesystems (notably default macOS volumes) reject
                // invalid byte sequences before the manifest layer sees them.
                return;
            }
            let registry = ToolRegistry::new(dir.path(), None, &[], None).unwrap();
            assert!(
                matches!(registry.workspace_manifest(), Err(ToolError::WorkspaceUnsafe(message)) if message.contains("NonUtf8Path"))
            );

            let links = tempfile::tempdir().unwrap();
            symlink(&name, links.path().join("link")).unwrap();
            let registry = ToolRegistry::new(links.path(), None, &[], None).unwrap();
            assert!(
                matches!(registry.workspace_manifest(), Err(ToolError::WorkspaceUnsafe(message)) if message.contains("NonUtf8Path"))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn manifest_component_encoding_distinguishes_backslash_from_directory_structure() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a\\b"), "same").unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/b"), "same").unwrap();
        symlink("a\\b", dir.path().join("link")).unwrap();
        let registry = ToolRegistry::new(dir.path(), None, &[], None).unwrap();
        let first = registry.workspace_manifest().unwrap();
        assert!(first.contains_key(r#"["a\\b"]"#));
        assert!(first.contains_key(r#"["a","b"]"#));

        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("a/b", dir.path().join("link")).unwrap();
        let second = registry.workspace_manifest().unwrap();
        assert_ne!(first[r#"["link"]"#], second[r#"["link"]"#]);
    }

    #[cfg(unix)]
    #[test]
    fn reads_safe_internal_symlink_but_mutations_reject_symlinks() {
        use std::os::unix::fs::symlink;
        let (dir, registry) = setup();
        symlink("a.txt", dir.path().join("link")).unwrap();
        assert!(
            registry
                .invoke(&call("read_file", &json!({"path":"link"}), None))
                .is_ok()
        );
        assert!(matches!(
            registry.invoke(&call(
                "write_file",
                &json!({"path":"link","content":"x"}),
                None
            )),
            Err(ToolError::Path(_))
        ));
        symlink("/tmp", dir.path().join("outside")).unwrap();
        assert!(matches!(
            registry.invoke(&call("read_file", &json!({"path":"outside/x"}), None)),
            Err(ToolError::Path(_))
        ));
    }

    #[test]
    #[rustfmt::skip]
    fn typed_input_helpers_and_builtin_preparation_enforce_schema_boundaries() {
        assert!(matches!(
            string(&json!({}), "path"),
            Err(ToolError::Input(_))
        ));
        assert!(matches!(
            string(&json!({"path":""}), "path"),
            Err(ToolError::Input(_))
        ));
        assert!(bool_field(&json!({}), "flag", true).unwrap());
        assert!(matches!(
            bool_field(&json!({"flag":"yes"}), "flag", false),
            Err(ToolError::Input(_))
        ));
        assert_eq!(usize_field(&json!({}), "cap", 3, 1, 4).unwrap(), 3);
        for value in [json!("3"), json!(-1), json!(5)] {
            assert!(matches!(
                usize_field(&json!({"cap":value}), "cap", 3, 1, 4),
                Err(ToolError::Input(_))
            ));
        }
        assert_eq!(bounded("short", 5), ("short".into(), false));
        assert_eq!(bounded("éx", 1), (String::new(), true));

        let (dir, registry) = setup();
        assert!(format!("{registry:?}").contains("ToolRegistry"));
        let link = registry.workspace.root().join("link");
        assert!(registry.safe_symlink_target(&link, Path::new("/absolute")).is_err());
        assert_eq!(registry.safe_symlink_target(&link, Path::new("./a.txt")).unwrap(), vec!["a.txt"]);
        assert!(registry.safe_symlink_target(&link, Path::new("../../outside")).is_err());
        assert!(registry.safe_symlink_target(Path::new("/tmp/outside-link"), Path::new("child")).is_err());
        assert_eq!(ToolRegistry::normal_components(Path::new("./a")).unwrap(), vec!["a"]);
        assert!(ToolRegistry::normal_components(Path::new("/absolute")).is_err());
        assert_eq!(ToolRegistry::raw_target_components(Path::new("./a/../b")).unwrap().len(), 4);
        assert!(ToolRegistry::raw_target_components(Path::new("/absolute")).is_err());
        let context = Context {
            workspace: &registry.workspace,
        };
        let unknown = Builtin {
            name: "not_registered",
            effect: EffectClass::Read,
        };
        assert!(matches!(
            unknown.prepare(&json!({}), &context),
            Err(ToolError::Unknown(name)) if name == "not_registered"
        ));
        let edit = Builtin {
            name: "edit_file",
            effect: EffectClass::Modify,
        };
        assert!(matches!(
            edit.prepare(
                &json!({"path":"a.txt","before":"","after":"next"}),
                &context
            ),
            Err(ToolError::Input(message)) if message.contains("before or anchor")
        ));
        let search = Builtin {
            name: "search",
            effect: EffectClass::Read,
        };
        assert!(matches!(
            search.prepare(&json!({"query":"x","regex":"yes"}), &context),
            Err(ToolError::Input(message)) if message.contains("regex must be boolean")
        ));
        drop(dir);

        let process_schema = tool_schema("process");
        assert_eq!(process_schema["oneOf"].as_array().unwrap().len(), 2);
        assert_eq!(tool_schema("not_registered")["additionalProperties"], false);
    }

    #[test]
    fn database_artifacts_and_process_cwd_stay_workspace_confined() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state.db");
        for name in [
            "state.db",
            "state.db-wal",
            "state.db-shm",
            "state.db-journal",
            "state.db-other",
        ] {
            fs::write(dir.path().join(name), name).unwrap();
        }
        let registry = ToolRegistry::new(dir.path(), None, &[], Some(&database)).unwrap();
        let manifest = registry.workspace_manifest().unwrap();
        for private in [
            r#"["state.db"]"#,
            r#"["state.db-wal"]"#,
            r#"["state.db-shm"]"#,
            r#"["state.db-journal"]"#,
        ] {
            assert!(!manifest.contains_key(private), "leaked {private}");
        }
        assert!(manifest.contains_key(r#"["state.db-other"]"#));

        assert_eq!(
            registry.resolve_cwd(".").unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
        assert!(matches!(
            registry.resolve_cwd("state.db-other"),
            Err(ToolError::Input(message)) if message.contains("must be a directory")
        ));
        assert!(matches!(
            registry.resolve_cwd(".."),
            Err(ToolError::Path(_))
        ));
    }

    #[test]
    fn engine_preparation_binds_policy_preconditions_and_safe_mutation_support() {
        let (dir, registry) = setup();
        let read_input = json!({"path":"a.txt"});
        let (read, decision, digest) = registry
            .prepare_for_engine(&call("read_file", &read_input, None))
            .unwrap();
        assert_eq!(decision, PolicyDecision::Allow);
        assert_eq!(digest.len(), 64);
        registry.execute_prepared(read).unwrap();

        let identity = WorkspacePath::identity(&dir.path().join("a.txt")).unwrap();
        let edit_input = json!({
            "path":"a.txt",
            "before":"two",
            "after":"second",
            "precondition":identity.hash
        });
        let edit_call = ToolInvocation {
            precondition: edit_input["precondition"].as_str(),
            ..call("edit_file", &edit_input, None)
        };
        let (prepared, decision, _) = registry.prepare_for_engine(&edit_call).unwrap();
        assert_eq!(decision, PolicyDecision::Ask);
        registry.execute_prepared(prepared).unwrap();
        assert!(
            fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .contains("second")
        );

        let missing_precondition = json!({"path":"a.txt","content":"next"});
        assert!(matches!(
            registry.prepare_for_engine(&call("write_file", &missing_precondition, None)),
            Err(ToolError::ReadRequired(target)) if target == "a.txt"
        ));

        let denied = ToolRegistry::new(dir.path(), None, &["a.txt".into()], None).unwrap();
        let (prepared, decision, _) = denied
            .prepare_for_engine(&ToolInvocation {
                precondition: Some(&identity.hash),
                ..call(
                    "write_file",
                    &json!({"path":"a.txt","content":"blocked"}),
                    None,
                )
            })
            .unwrap();
        assert_eq!(decision, PolicyDecision::Deny);
        drop(prepared);

        let unsupported =
            ToolRegistry::new_with_safe_support(dir.path(), None, &[], false).unwrap();
        assert!(matches!(
            unsupported.prepare_for_engine(&ToolInvocation {
                precondition: Some(&identity.hash),
                ..call(
                    "write_file",
                    &json!({"path":"a.txt","content":"blocked"}),
                    None
                )
            }),
            Err(ToolError::Path(_))
        ));
    }

    #[test]
    fn literal_search_skips_non_utf8_and_reports_regex_and_output_boundaries() {
        let (dir, registry) = setup();
        fs::write(dir.path().join("binary.bin"), [0xff, 0xfe, 0xfd]).unwrap();
        fs::write(dir.path().join("many.txt"), "needle one\nneedle two\n").unwrap();

        let literal = registry
            .invoke(&call(
                "search",
                &json!({"query":"needle","max_output":5}),
                None,
            ))
            .unwrap();
        assert!(literal.truncated);
        assert!(literal.value["matches"].as_array().unwrap().is_empty());
        assert!(matches!(
            registry.invoke(&call("search", &json!({"query":"[","regex":true}), None)),
            Err(ToolError::Regex(_))
        ));

        fs::write(dir.path().join("package.json"), "{\"name\":\"long\"}").unwrap();
        let manifests = registry
            .invoke(&call(
                "read_project_manifest",
                &json!({"max_output":1}),
                None,
            ))
            .unwrap();
        assert!(manifests.truncated);
        assert_eq!(manifests.value["Cargo.toml"], "[");
        assert_eq!(manifests.value["package.json"], "{");
    }
}
