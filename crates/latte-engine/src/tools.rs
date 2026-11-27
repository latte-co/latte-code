#[cfg(test)]
use crate::policy::PendingApproval;
use crate::{
    policy::{self, EffectClass, OperationBinding, PermissionPolicy, PolicyDecision},
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
    pub turn_revision: u64,
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
    snapshot_scope: String,
}
enum Action {
    Read {
        path: std::path::PathBuf,
        offset: Option<usize>,
        limit: Option<usize>,
        max: usize,
    },
    List {
        path: std::path::PathBuf,
        max_depth: usize,
        glob: Option<String>,
        max: usize,
    },
    Search {
        path: std::path::PathBuf,
        glob: Option<String>,
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
        replace_all: bool,
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

pub(crate) fn is_readonly_tool_name(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "list_directory" | "search" | "read_project_manifest" | "git_diff"
    )
}

fn is_ignored_walk_dir(name: Option<&str>) -> bool {
    matches!(
        name,
        Some(
            ".git"
                | ".latte"
                | ".oh-my-code"
                | ".opencode"
                | ".worktree"
                | ".tmp"
                | "target"
                | "node_modules"
                | ".venv"
                | "__pycache__"
                | ".pytest_cache"
        )
    )
}

struct Builtin {
    name: &'static str,
    effect: EffectClass,
}
impl Tool for Builtin {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.into(),
            description: tool_description(self.name).into(),
            input_schema: tool_schema(self.name),
            version: 1,
            effect: format!("{:?}", self.effect).to_lowercase(),
        }
    }
    #[allow(clippy::too_many_lines)]
    fn prepare(&self, input: &Value, context: &Context<'_>) -> Result<Prepared, ToolError> {
        let path = || string(input, "path");
        let max = || usize_field(input, "max_output", DEFAULT_CAP, 1, DEFAULT_CAP);
        let (target, action) = match self.name {
            "read_file" => {
                let p = path()?;
                let offset = optional_usize_field(input, "offset", 1, 1_000_000)?;
                let limit = optional_usize_field(input, "limit", 1, 10_000)?;
                (
                    context.workspace.display(p)?,
                    Action::Read {
                        path: context.workspace.read(p)?,
                        offset,
                        limit,
                        max: max()?,
                    },
                )
            }
            "list_directory" => {
                let p = optional_string(input, "path")?.unwrap_or(".");
                let max_depth = usize_field(input, "max_depth", 1, 1, 8)?;
                let glob = optional_string(input, "glob")?.map(str::to_owned);
                if let Some(pattern) = &glob {
                    globset::Glob::new(pattern)
                        .map_err(|error| ToolError::Input(format!("invalid glob: {error}")))?;
                }
                (
                    context.workspace.display(p)?,
                    Action::List {
                        path: context.workspace.read(p)?,
                        max_depth,
                        glob,
                        max: usize_field(input, "max_entries", 1000, 1, 10_000)?,
                    },
                )
            }
            "search" => {
                let p = optional_string(input, "path")?.unwrap_or(".");
                let glob = optional_string(input, "glob")?.map(str::to_owned);
                if let Some(pattern) = &glob {
                    globset::Glob::new(pattern)
                        .map_err(|error| ToolError::Input(format!("invalid glob: {error}")))?;
                }
                (
                    context.workspace.display(p)?,
                    Action::Search {
                        path: context.workspace.read(p)?,
                        glob,
                        query: string(input, "query")?.into(),
                        regex: bool_field(input, "regex", false)?,
                        max_results: usize_field(input, "max_results", 100, 1, 10_000)?,
                        max_output: max()?,
                    },
                )
            }
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
                let replace_all = optional_bool_field(input, "replace_all")?.unwrap_or(false);
                (
                    context.workspace.display(p)?,
                    Action::Edit {
                        path: context.workspace.mutation(p, false)?,
                        before: before.into(),
                        after: raw_string(input, "after")?.into(),
                        replace_all,
                    },
                )
            }
            "write_file" => {
                let p = path()?;
                let display = context.workspace.display(p)?;
                let explicit_create = optional_bool_field(input, "create_intent")?;
                let create = match explicit_create {
                    Some(explicit) => explicit,
                    None => context
                        .workspace
                        .mutation(p, false)
                        .is_err_and(|error| matches!(error, PathError::Missing(_))),
                };
                (
                    display,
                    Action::Write {
                        path: context.workspace.mutation(p, create)?,
                        content: raw_string(input, "content")?.into(),
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
            snapshot_scope: String::new(),
        })
    }
}

/// Model-visible tool documentation. The schema constrains shape; this states
/// the contracts a schema cannot express, above all that `edit_file` and
/// `write_file` require reading an existing file first (with optional
/// `sha256` `precondition`).
pub(crate) fn tool_description(name: &str) -> &'static str {
    match name {
        "read_file" => {
            "Read a UTF-8 text file inside the workspace. Supports optional 1-based line \
             pagination via `offset` and `limit` for large files. Returns {content, \
             start_line, end_line, total_lines, has_more, sha256, size, modified_ns}. \
             Reading a file records its fresh snapshot so subsequent `edit_file` or \
             `write_file` calls on the same path succeed without copying `sha256` \
             (though passing `sha256` as `precondition` is also supported)."
        }
        "list_directory" => {
            "List workspace entries under `path` (default `.`), sorted, appending `/` to \
             directories. Set `max_depth` (1..=8) or `glob` (e.g. `**/*.rs`) to discover \
             files across subdirectories while honouring .gitignore."
        }
        "search" => {
            "Search workspace file contents line by line, honouring .gitignore. Optional \
             `path` scopes the directory/file and optional `glob` filters filenames. \
             Returns {matches} as `path:line:text` strings. `query` is a literal substring \
             unless `regex` is true. Results stop at `max_results` or `max_output` bytes."
        }
        "read_project_manifest" => {
            "Read the workspace's dependency manifests (Cargo.toml, package.json, \
             pyproject.toml, go.mod) that exist at its root. Returns one entry per file \
             found. Use this to learn the language, dependencies, and toolchain before \
             proposing changes."
        }
        "git_diff" => {
            "Show the workspace's uncommitted changes (stat + unified diff against HEAD \
             plus untracked files). Returns an empty result when nothing is modified. Use \
             it to review your own edits before reporting them as complete."
        }
        "edit_file" => {
            "Replace a span of an existing file. Call `read_file` on the same path first \
             (the engine tracks the read `sha256` automatically, or you may pass an explicit \
             `precondition`). `before` must match the target span verbatim and occur exactly once \
             (unless `replace_all` is true) — widen `before` with surrounding lines until it is \
             unique. `after` replaces it and may be empty to delete the span. Prefer this over \
             `write_file` for partial edits."
        }
        "write_file" => {
            "Write a file's complete contents, automatically creating missing parent \
             directories when creating a new file. When `create_intent` is omitted, new \
             paths are created and existing paths overwrite after a prior `read_file` \
             (with optional `sha256` `precondition`). Prefer `edit_file` for partial edits."
        }
        "process" => {
            "Run a command inside the workspace under engine supervision. Prefer `argv`, \
             an argument vector executed directly with no shell, so no quoting or \
             expansion applies. `shell` runs a `/bin/sh -c` command line instead and \
             requires policy approval; reserve it for pipelines and redirection. Output is \
             captured up to `stdout_cap`/`stderr_cap` bytes and the process group is \
             terminated at `timeout_ms`."
        }
        _ => "Engine-owned workspace operation.",
    }
}

pub(crate) fn tool_schema(name: &str) -> Value {
    let path = || json!({"type":"string","minLength":1,"maxLength":4096});
    let cap = || json!({"type":"integer","minimum":1,"maximum":65536});
    let digest = || json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    match name {
        "read_file" => {
            json!({"type":"object","required":["path"],"properties":{"path":path(),"offset":{"type":"integer","minimum":1,"maximum":1_000_000},"limit":{"type":"integer","minimum":1,"maximum":10_000},"max_output":cap()},"additionalProperties":false})
        }
        "list_directory" => {
            json!({"type":"object","required":[],"properties":{"path":path(),"max_depth":{"type":"integer","minimum":1,"maximum":8},"glob":{"type":"string","minLength":1,"maxLength":1024},"max_entries":{"type":"integer","minimum":1,"maximum":10_000}},"additionalProperties":false})
        }
        "search" => {
            json!({"type":"object","required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":4096},"path":path(),"glob":{"type":"string","minLength":1,"maxLength":1024},"regex":{"type":"boolean"},"max_results":{"type":"integer","minimum":1,"maximum":10_000},"max_output":cap()},"additionalProperties":false})
        }
        "read_project_manifest" | "git_diff" => {
            json!({"type":"object","required":[],"properties":{"max_output":cap()},"additionalProperties":false})
        }
        "edit_file" => {
            json!({"type":"object","required":["path","after"],"properties":{"path":path(),"before":{"type":"string","minLength":1},"anchor":{"type":"string","minLength":1},"after":{"type":"string"},"replace_all":{"type":"boolean"},"precondition":digest()},"additionalProperties":false})
        }
        "write_file" => {
            json!({"type":"object","required":["path","content"],"properties":{"path":path(),"content":{"type":"string"},"create_intent":{"type":"boolean"},"precondition":digest()},"additionalProperties":false})
        }
        "process" => {
            json!({"type":"object","required":[],"properties":{"argv":{"type":"array","minItems":1,"maxItems":256,"items":{"type":"string","minLength":1,"maxLength":4096}},"shell":{"type":"string","minLength":1,"maxLength":16384},"cwd":path(),"env":{"type":"object","maxProperties":128,"additionalProperties":{"type":"string","maxLength":16384}},"timeout_ms":{"type":"integer","minimum":1,"maximum":600_000},"grace_ms":{"type":"integer","minimum":0,"maximum":30_000},"stdout_cap":{"type":"integer","minimum":1,"maximum":1_048_576},"stderr_cap":{"type":"integer","minimum":1,"maximum":1_048_576}},"additionalProperties":false})
        }
        _ => json!({"type":"object","required":[],"properties":{},"additionalProperties":false}),
    }
}

pub(crate) struct ToolRegistry {
    workspace: WorkspacePath,
    database_path: Option<std::path::PathBuf>,
    policy: PermissionPolicy,
    tools: BTreeMap<String, Box<dyn Tool>>,
    snapshots: Mutex<BTreeMap<(String, String), FileIdentity>>,
    #[cfg(test)]
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
    pub(crate) fn display_path(&self, input: &str) -> Result<String, ToolError> {
        Ok(self.workspace.display(input)?)
    }
    pub(crate) fn single_file_manifest_digest(
        &self,
        display_path: &str,
    ) -> Result<String, ToolError> {
        let target = self.workspace.root().join(display_path);
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_file() => Ok(WorkspacePath::identity(&target)?.hash),
            Ok(meta) if meta.file_type().is_symlink() => {
                let raw = fs::read_link(&target)?;
                Ok(format!(
                    "symlink:{:x}",
                    Sha256::digest(raw.to_string_lossy().as_bytes())
                ))
            }
            Ok(_) => Ok("nonfile".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("missing".into()),
            Err(error) => Err(ToolError::Io(error)),
        }
    }
    pub(crate) fn workspace_manifest(&self) -> Result<BTreeMap<String, String>, ToolError> {
        const STREAM_HASH_CAP_BYTES: u64 = 16 * 1024 * 1024;
        let mut result = BTreeMap::new();
        for entry in WalkBuilder::new(self.workspace.root())
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .require_git(false)
            .filter_entry(|entry| !is_ignored_walk_dir(entry.file_name().to_str()))
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
                let normalized = self.safe_symlink_target(entry.path(), &raw_target).ok();
                let target_identity = normalized.as_ref().and_then(|_| {
                    fs::canonicalize(entry.path())
                        .ok()
                        .filter(|canonical| {
                            canonical.starts_with(self.workspace.root()) && canonical.is_file()
                        })
                        .and_then(|canonical| WorkspacePath::identity(&canonical).ok())
                        .map(|identity| identity.hash)
                });
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
                    "raw_target": {
                        "absolute": raw_target.is_absolute(),
                        "components": raw_target_components,
                    },
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
            let file_hash = if before.len() > STREAM_HASH_CAP_BYTES {
                let modified_ns = before_modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(std::io::Error::other)?
                    .as_nanos();
                format!(
                    "{:x}",
                    Sha256::digest(format!("oversized:{}:{modified_ns}", before.len()).as_bytes())
                )
            } else {
                let mut file = fs::File::open(entry.path())?;
                let mut hasher = Sha256::new();
                std::io::copy(&mut file, &mut hasher)?;
                format!("{:x}", hasher.finalize())
            };
            let after = fs::metadata(entry.path())?;
            if before.len() != after.len() || before_modified != after.modified()? {
                return Err(ToolError::WorkspaceUnstable(relative));
            }
            if result.insert(relative, file_hash).is_some() {
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
                std::path::Component::RootDir => Ok(serde_json::json!(["root"])),
                std::path::Component::Prefix(prefix) => Ok(serde_json::json!([
                    "prefix",
                    prefix
                        .as_os_str()
                        .to_str()
                        .ok_or_else(|| ToolError::WorkspaceUnsafe("NonUtf8Path".into()))?
                ])),
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
            #[cfg(test)]
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
    pub(crate) fn resolve_session_mutation_authority(
        &self,
        snapshot_scope: &str,
        mut authority: crate::EffectAuthority,
    ) -> Result<crate::EffectAuthority, ToolError> {
        if !matches!(authority.name.as_str(), "edit_file" | "write_file") {
            return Ok(authority);
        }
        let tool = self
            .tools
            .get(&authority.name)
            .ok_or_else(|| ToolError::Unknown(authority.name.clone()))?;
        let prepared = tool.prepare(
            &authority.input,
            &Context {
                workspace: &self.workspace,
            },
        )?;
        let explicit_precondition = optional_string(&authority.input, "precondition")?.is_some();
        if let Action::Write { create, .. } = &prepared.action
            && optional_bool_field(&authority.input, "create_intent")?.is_none()
            && let Some(obj) = authority.input.as_object_mut()
        {
            obj.insert("create_intent".into(), Value::Bool(*create));
        }
        if prepared.effect == EffectClass::Modify && !explicit_precondition {
            let snap = self
                .snapshots
                .lock()
                .unwrap()
                .get(&(snapshot_scope.to_owned(), prepared.target.clone()))
                .cloned()
                .ok_or(ToolError::ReadRequired(prepared.target))?;
            if let Some(obj) = authority.input.as_object_mut() {
                obj.insert("precondition".into(), Value::String(snap.hash));
            }
        }
        Ok(authority)
    }

    pub(crate) fn prepare_for_engine(
        &self,
        invocation: &ToolInvocation<'_>,
    ) -> Result<(Prepared, PolicyDecision, String), ToolError> {
        self.prepare_for_engine_scoped(invocation, invocation.lease_owner)
    }

    pub(crate) fn prepare_for_engine_scoped(
        &self,
        invocation: &ToolInvocation<'_>,
        snapshot_scope: &str,
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
        snapshot_scope.clone_into(&mut prepared.snapshot_scope);
        if prepared.effect != EffectClass::Read {
            self.workspace.validate_mutation_support()?;
        }
        if prepared.effect == EffectClass::Modify {
            if let Some(hash) = invocation.precondition {
                prepared.expected_hash = Some(hash.to_owned());
            } else {
                let snap = self
                    .snapshots
                    .lock()
                    .unwrap()
                    .get(&(snapshot_scope.to_owned(), prepared.target.clone()))
                    .cloned()
                    .ok_or_else(|| ToolError::ReadRequired(prepared.target.clone()))?;
                prepared.expected_hash = Some(snap.hash);
            }
        }
        let digest = policy::digest(&OperationBinding {
            descriptor_version: tool.descriptor().version,
            turn_revision: invocation.turn_revision,
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
    #[cfg(test)]
    pub(crate) fn invoke(&self, invocation: &ToolInvocation<'_>) -> Result<ToolOutput, ToolError> {
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
        prepared.snapshot_scope = invocation.lease_owner.to_owned();
        if prepared.effect == EffectClass::Modify {
            if let Some(hash) = invocation.precondition {
                prepared.expected_hash = Some(hash.to_owned());
            } else if let Some(snap) = self
                .snapshots
                .lock()
                .unwrap()
                .get(&(prepared.snapshot_scope.clone(), prepared.target.clone()))
                .cloned()
            {
                prepared.expected_hash = Some(snap.hash);
            }
        }
        let digest = policy::digest(&OperationBinding {
            descriptor_version: tool.descriptor().version,
            turn_revision: invocation.turn_revision,
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
            Action::Read {
                path,
                offset,
                limit,
                max,
            } => {
                let (bytes, id) = WorkspacePath::read_with_identity(&path)?;
                if bytes[..bytes.len().min(8192)].contains(&0) {
                    return Err(ToolError::Input(format!(
                        "cannot read binary file {} (contains NUL bytes)",
                        prepared.target
                    )));
                }
                self.snapshots.lock().unwrap().insert(
                    (prepared.snapshot_scope.clone(), prepared.target),
                    id.clone(),
                );
                let full_text = String::from_utf8_lossy(&bytes);
                let total_lines = full_text.lines().count();
                if offset.is_some() || limit.is_some() {
                    let lines: Vec<&str> = full_text.lines().collect();
                    let start_idx = offset.unwrap_or(1).saturating_sub(1);
                    let take_count = limit.unwrap_or(2000);
                    if start_idx >= lines.len() {
                        return Ok(ToolOutput {
                            value: json!({
                                "content": "",
                                "start_line": start_idx + 1,
                                "end_line": start_idx,
                                "total_lines": total_lines,
                                "has_more": false,
                                "sha256": id.hash,
                                "size": id.size,
                                "modified_ns": id.modified_ns,
                            }),
                            truncated: false,
                        });
                    }
                    let end_idx = lines.len().min(start_idx.saturating_add(take_count));
                    let mut sliced = lines[start_idx..end_idx].join("\n");
                    if end_idx < lines.len() || full_text.ends_with('\n') {
                        sliced.push('\n');
                    }
                    let (text, byte_truncated) = bounded(&sliced, max);
                    let shown_lines = text.lines().count();
                    let end_line = start_idx.saturating_add(shown_lines);
                    let has_more = end_idx < lines.len() || byte_truncated;
                    return Ok(ToolOutput {
                        value: json!({
                            "content": text,
                            "start_line": start_idx + 1,
                            "end_line": end_line,
                            "total_lines": total_lines,
                            "has_more": has_more,
                            "sha256": id.hash,
                            "size": id.size,
                            "modified_ns": id.modified_ns,
                        }),
                        truncated: has_more,
                    });
                }
                let (text, truncated) = bounded(&full_text, max);
                let end_line = text.lines().count();
                Ok(ToolOutput {
                    value: json!({
                        "content": text,
                        "start_line": usize::from(!full_text.is_empty()),
                        "end_line": end_line,
                        "total_lines": total_lines,
                        "has_more": truncated,
                        "sha256": id.hash,
                        "size": id.size,
                        "modified_ns": id.modified_ns,
                    }),
                    truncated,
                })
            }
            Action::List {
                path,
                max_depth,
                glob,
                max,
            } => {
                let glob_matcher = glob
                    .as_deref()
                    .map(|pattern| {
                        globset::Glob::new(pattern)
                            .map(|compiled| compiled.compile_matcher())
                            .map_err(|error| ToolError::Input(format!("invalid glob: {error}")))
                    })
                    .transpose()?;
                let mut entries = Vec::new();
                if max_depth == 1 && glob_matcher.is_none() {
                    for dir_entry in fs::read_dir(&path)? {
                        let dir_entry = dir_entry?;
                        let name = dir_entry.file_name().to_string_lossy().into_owned();
                        if is_ignored_walk_dir(Some(name.as_str())) {
                            continue;
                        }
                        let is_dir = dir_entry.file_type().is_ok_and(|kind| kind.is_dir());
                        let entry_path = dir_entry.path();
                        if let Ok(rel) = entry_path.strip_prefix(self.workspace.root()) {
                            let rel_str = rel.to_string_lossy().replace('\\', "/");
                            if self.policy.is_denied(&rel_str)
                                || (is_dir && self.policy.is_denied(&format!("{rel_str}/_")))
                            {
                                continue;
                            }
                        }
                        entries.push(if is_dir { format!("{name}/") } else { name });
                    }
                } else {
                    for walked in WalkBuilder::new(&path)
                        .max_depth(Some(max_depth))
                        .hidden(false)
                        .git_ignore(true)
                        .git_global(false)
                        .git_exclude(true)
                        .require_git(false)
                        .filter_entry(|entry| {
                            entry.depth() == 0 || !is_ignored_walk_dir(entry.file_name().to_str())
                        })
                        .build()
                        .filter_map(Result::ok)
                    {
                        if walked.depth() == 0 {
                            continue;
                        }
                        let is_dir = walked.file_type().is_some_and(|kind| kind.is_dir());
                        let Ok(ws_rel) = walked.path().strip_prefix(self.workspace.root()) else {
                            continue;
                        };
                        let ws_rel_str = ws_rel.to_string_lossy().replace('\\', "/");
                        if self.policy.is_denied(&ws_rel_str)
                            || (is_dir && self.policy.is_denied(&format!("{ws_rel_str}/_")))
                        {
                            continue;
                        }
                        let Ok(rel) = walked.path().strip_prefix(&path) else {
                            continue;
                        };
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        if let Some(matcher) = &glob_matcher {
                            let file_name = walked.file_name().to_string_lossy();
                            if !matcher.is_match(&rel_str)
                                && !matcher.is_match(&ws_rel_str)
                                && !matcher.is_match(file_name.as_ref())
                            {
                                continue;
                            }
                        }
                        entries.push(if is_dir {
                            format!("{rel_str}/")
                        } else {
                            rel_str
                        });
                    }
                }
                entries.sort();
                let truncated = entries.len() > max;
                entries.truncate(max);
                Ok(ToolOutput {
                    value: json!({"entries":entries}),
                    truncated,
                })
            }
            Action::Search {
                path,
                glob,
                query,
                regex,
                max_results,
                max_output,
            } => {
                const MAX_SEARCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
                const MAX_MATCH_LINE_BYTES: usize = 512;
                let matcher = regex.then(|| Regex::new(&query)).transpose()?;
                let glob_matcher = glob
                    .as_deref()
                    .map(|pattern| {
                        globset::Glob::new(pattern)
                            .map(|compiled| compiled.compile_matcher())
                            .map_err(|error| ToolError::Input(format!("invalid glob: {error}")))
                    })
                    .transpose()?;
                let mut results = Vec::new();
                let mut used = 0;
                let mut truncated = false;
                for entry in WalkBuilder::new(&path)
                    .hidden(false)
                    .git_ignore(true)
                    .git_global(false)
                    .git_exclude(true)
                    .require_git(false)
                    .filter_entry(|entry| {
                        entry.depth() == 0 || !is_ignored_walk_dir(entry.file_name().to_str())
                    })
                    .build()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                {
                    let Ok(rel_path) = entry.path().strip_prefix(self.workspace.root()) else {
                        continue;
                    };
                    let rel = rel_path.to_string_lossy().replace('\\', "/");
                    if self.policy.is_denied(&rel) {
                        continue;
                    }
                    if let Some(glob_matcher) = &glob_matcher {
                        let file_name = entry.file_name().to_string_lossy();
                        if !glob_matcher.is_match(&rel)
                            && !glob_matcher.is_match(file_name.as_ref())
                        {
                            continue;
                        }
                    }
                    if entry
                        .metadata()
                        .is_ok_and(|meta| meta.len() > MAX_SEARCH_FILE_BYTES)
                    {
                        continue;
                    }
                    let Ok(text) = fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    for (i, line) in text.lines().enumerate() {
                        if matcher
                            .as_ref()
                            .map_or_else(|| line.contains(&query), |r| r.is_match(line))
                        {
                            let (bounded_line, line_cut) = bounded(line, MAX_MATCH_LINE_BYTES);
                            let display_line = if line_cut {
                                format!("{bounded_line}…")
                            } else {
                                bounded_line
                            };
                            let item = format!("{rel}:{}:{display_line}", i + 1);
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
                replace_all,
            } => {
                let checked = self.workspace.mutation(&prepared.target, false)?;
                if checked != path {
                    return Err(ToolError::Path(
                        "target identity changed before mutation".into(),
                    ));
                }
                self.require_fresh(
                    &prepared.snapshot_scope,
                    &prepared.target,
                    &path,
                    prepared.expected_hash.as_deref(),
                )?;
                let content = fs::read_to_string(&path)?;
                let next = apply_edit_span(&content, &before, &after, replace_all)?;
                self.workspace
                    .atomic_replace(&prepared.target, next.as_bytes(), false)?;
                let id = WorkspacePath::identity(&path)?;
                self.snapshots.lock().unwrap().insert(
                    (prepared.snapshot_scope.clone(), prepared.target),
                    id.clone(),
                );
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
                    self.require_fresh(
                        &prepared.snapshot_scope,
                        &prepared.target,
                        &path,
                        prepared.expected_hash.as_deref(),
                    )?;
                }
                self.workspace
                    .atomic_replace(&prepared.target, content.as_bytes(), create)?;
                let id = WorkspacePath::identity(&path)?;
                self.snapshots.lock().unwrap().insert(
                    (prepared.snapshot_scope.clone(), prepared.target),
                    id.clone(),
                );
                Ok(ToolOutput {
                    value: json!({"sha256":id.hash,"size":id.size}),
                    truncated: false,
                })
            }
            Action::GitDiff { max } => {
                let (text, truncated) =
                    crate::process::supervise_git_diff(self.workspace.root(), max, |rel_path| {
                        self.is_git_diff_path_allowed(rel_path)
                    })
                    .map_err(|e| ToolError::Git(e.to_string()))?;
                Ok(ToolOutput {
                    value: json!({"summary":text}),
                    truncated,
                })
            }
        }
    }
    fn is_git_diff_path_allowed(&self, rel_path: &str) -> bool {
        if rel_path.is_empty() {
            return false;
        }
        let Ok(display) = self.workspace.display(rel_path) else {
            return false;
        };
        if self
            .workspace
            .root()
            .join(rel_path)
            .symlink_metadata()
            .is_ok()
            && self.workspace.read(rel_path).is_err()
        {
            return false;
        }
        !self.policy.is_denied(rel_path)
            && !self.policy.is_denied(&display)
            && self.policy.decide(EffectClass::Read, &display) == PolicyDecision::Allow
    }
    fn require_fresh(
        &self,
        scope: &str,
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
            .get(&(scope.to_owned(), target.to_owned()))
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
fn raw_string<'a>(v: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Input(format!("{key} must be a string")))
}
fn optional_string<'a>(v: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(ToolError::Input(format!(
            "{key} must be a non-empty string"
        ))),
    }
}
fn optional_bool_field(v: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::Input(format!("{key} must be boolean"))),
    }
}
fn bool_field(v: &Value, key: &str, default: bool) -> Result<bool, ToolError> {
    optional_bool_field(v, key).map(|value| value.unwrap_or(default))
}
fn optional_usize_field(
    v: &Value,
    key: &str,
    min: usize,
    max: usize,
) -> Result<Option<usize>, ToolError> {
    let Some(raw) = v.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let n = raw
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| ToolError::Input(format!("{key} must be an integer")))?;
    if !(min..=max).contains(&n) {
        return Err(ToolError::Input(format!("{key} is out of bounds")));
    }
    Ok(Some(n))
}
fn usize_field(
    v: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ToolError> {
    optional_usize_field(v, key, min, max).map(|value| value.unwrap_or(default))
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

#[derive(Clone, Copy)]
struct LineSpan<'a> {
    start: usize,
    end_body: usize,
    end_full: usize,
    body: &'a str,
}

fn split_line_spans(text: &str) -> Vec<LineSpan<'_>> {
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for chunk in text.split_inclusive('\n') {
        let start = cursor;
        let end_full = start + chunk.len();
        let body = chunk.strip_suffix('\n').unwrap_or(chunk);
        let body = body.strip_suffix('\r').unwrap_or(body);
        spans.push(LineSpan {
            start,
            end_body: start + body.len(),
            end_full,
            body,
        });
        cursor = end_full;
    }
    spans
}

fn adapt_line_endings(after: &str, use_crlf: bool) -> String {
    if use_crlf && !after.contains("\r\n") && after.contains('\n') {
        after.replace('\n', "\r\n")
    } else {
        after.to_owned()
    }
}

fn normalize_quotes_with_map(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut norm_to_orig = Vec::with_capacity(text.len() + 1);
    for (orig_idx, ch) in text.char_indices() {
        let mapped = match ch {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            other => other,
        };
        for _ in 0..mapped.len_utf8() {
            norm_to_orig.push(orig_idx);
        }
        out.push(mapped);
    }
    norm_to_orig.push(text.len());
    (out, norm_to_orig)
}

fn min_space_indent<'a>(lines: impl Iterator<Item = &'a str>) -> Option<(usize, usize)> {
    let mut min_indent = usize::MAX;
    let mut non_empty = 0usize;
    for line in lines {
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('\t') {
            return None;
        }
        let spaces = trimmed.bytes().take_while(|b| *b == b' ').count();
        min_indent = min_indent.min(spaces);
        non_empty += 1;
    }
    (non_empty > 0).then_some((min_indent, non_empty))
}

fn shift_space_indent(text: &str, delta: isize, use_crlf: bool) -> String {
    let newline = if use_crlf { "\r\n" } else { "\n" };
    let trailing_newline = text.ends_with('\n');
    let raw_lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let slice = if trailing_newline && raw_lines.last() == Some(&"") {
        &raw_lines[..raw_lines.len() - 1]
    } else {
        &raw_lines[..]
    };
    let mut shifted = Vec::with_capacity(slice.len());
    for line in slice {
        if line.trim().is_empty() {
            shifted.push(String::new());
            continue;
        }
        if delta >= 0 {
            let pad = " ".repeat(delta.unsigned_abs());
            shifted.push(format!("{pad}{line}"));
        } else {
            let drop_spaces = delta.unsigned_abs();
            let leading = line.bytes().take_while(|b| *b == b' ').count();
            shifted.push(line[leading.min(drop_spaces)..].to_owned());
        }
    }
    let mut joined = shifted.join(newline);
    if trailing_newline {
        joined.push_str(newline);
    }
    joined
}

/// Replaces `before` with `after` in `content`.
///
/// Tries exact verbatim matching first. When exact matching finds zero hits,
/// runs three deterministic, uniqueness-checked normalization stages:
/// 1. CRLF / trailing-whitespace line comparison;
/// 2. typographic curly-quote normalization (`‘’` / `“”`);
/// 3. uniform space-indentation shift for multi-line blocks (`>= 2` non-empty
///    lines) when `replace_all` is `false`.
#[allow(clippy::too_many_lines)]
fn apply_edit_span(
    content: &str,
    before: &str,
    after: &str,
    replace_all: bool,
) -> Result<String, ToolError> {
    let exact_count = content.match_indices(before).count();
    if exact_count == 1 {
        return Ok(content.replacen(before, after, 1));
    }
    if exact_count > 1 {
        return if replace_all {
            Ok(content.replace(before, after))
        } else {
            Err(ToolError::MatchCount(exact_count))
        };
    }

    let use_crlf = content.contains("\r\n");
    let before_has_trailing_nl = before.ends_with('\n');
    let needle_spans = split_line_spans(before);
    let cand_spans = split_line_spans(content);

    // Collect candidates across all lenient normalization stages before accepting
    // a single match so an ambiguous match in a later stage is never masked by an
    // earlier stage's single hit.
    let adapted_after = adapt_line_endings(after, use_crlf);
    let mut stage1_hits: Vec<(usize, usize)> = Vec::new();

    // Stage 1: line-trimmed-end & CRLF normalization.
    if !needle_spans.is_empty()
        && needle_spans
            .iter()
            .any(|s| !s.body.trim_end_matches([' ', '\t']).is_empty())
        && cand_spans.len() >= needle_spans.len()
    {
        let win = needle_spans.len();
        let mut idx = 0usize;
        while idx + win <= cand_spans.len() {
            let matched = (0..win).all(|offset| {
                cand_spans[idx + offset].body.trim_end_matches([' ', '\t'])
                    == needle_spans[offset].body.trim_end_matches([' ', '\t'])
            });
            if matched {
                let start = cand_spans[idx].start;
                let last = &cand_spans[idx + win - 1];
                let end = if before_has_trailing_nl {
                    last.end_full
                } else {
                    last.end_body
                };
                stage1_hits.push((start, end));
                idx += if replace_all { win } else { 1 };
            } else {
                idx += 1;
            }
        }
    }

    // Stage 2: typographic curly-quote normalization.
    let mut stage2_hits: Vec<(usize, usize)> = Vec::new();
    let (norm_content, norm_map) = normalize_quotes_with_map(content);
    let (norm_before, _) = normalize_quotes_with_map(before);
    if !norm_before.is_empty() {
        stage2_hits = norm_content
            .match_indices(&norm_before)
            .map(|(start, matched)| (norm_map[start], norm_map[start + matched.len()]))
            .collect();
    }

    // Stage 3: uniform space-indentation shift for multi-line blocks (`!replace_all`).
    let mut stage3_hits: Vec<(usize, usize, String)> = Vec::new();
    if !replace_all
        && let Some((needle_indent, non_empty)) =
            min_space_indent(needle_spans.iter().map(|s| s.body))
        && non_empty >= 2
        && cand_spans.len() >= needle_spans.len()
    {
        let win = needle_spans.len();
        let dedented_needle: Vec<&str> = needle_spans
            .iter()
            .map(|s| {
                let trimmed = s.body.trim_end_matches([' ', '\t']);
                if trimmed.is_empty() {
                    ""
                } else {
                    &trimmed[needle_indent..]
                }
            })
            .collect();
        for idx in 0..=(cand_spans.len() - win) {
            let window = &cand_spans[idx..idx + win];
            let Some((cand_indent, cand_non_empty)) =
                min_space_indent(window.iter().map(|s| s.body))
            else {
                continue;
            };
            if cand_non_empty != non_empty {
                continue;
            }
            let matched = window.iter().zip(&dedented_needle).all(|(cand, expected)| {
                let trimmed = cand.body.trim_end_matches([' ', '\t']);
                let actual = if trimmed.is_empty() {
                    ""
                } else {
                    &trimmed[cand_indent..]
                };
                actual == *expected
            });
            if matched {
                let start = window[0].start;
                let last = &window[win - 1];
                let end = if before_has_trailing_nl {
                    last.end_full
                } else {
                    last.end_body
                };
                let delta = isize::try_from(cand_indent).unwrap_or(0)
                    - isize::try_from(needle_indent).unwrap_or(0);
                stage3_hits.push((start, end, shift_space_indent(after, delta, use_crlf)));
            }
        }
    }

    // Deduplicate candidate spans by exact `(start, end)` byte range (preserving
    // Stage 1 > Stage 2 > Stage 3 replacement precedence when two stages match the
    // exact same span), while rejecting any distinct multi-span ambiguity.
    let mut unique_candidates: Vec<(usize, usize, String)> = Vec::new();
    for &(start, end) in &stage1_hits {
        if !unique_candidates
            .iter()
            .any(|&(s, e, _)| s == start && e == end)
        {
            unique_candidates.push((start, end, adapted_after.clone()));
        }
    }
    for &(start, end) in &stage2_hits {
        if !unique_candidates
            .iter()
            .any(|&(s, e, _)| s == start && e == end)
        {
            unique_candidates.push((start, end, after.to_owned()));
        }
    }
    for (start, end, shifted) in stage3_hits {
        if !unique_candidates
            .iter()
            .any(|&(s, e, _)| s == start && e == end)
        {
            unique_candidates.push((start, end, shifted));
        }
    }

    if unique_candidates.is_empty() {
        return Err(ToolError::MatchCount(0));
    }
    if !replace_all {
        if unique_candidates.len() > 1 {
            return Err(ToolError::MatchCount(unique_candidates.len()));
        }
        let (start, end, replacement) = &unique_candidates[0];
        let mut out = content.to_owned();
        out.replace_range(*start..*end, replacement);
        return Ok(out);
    }

    // `replace_all`: if both Stage 1 and Stage 2 matched different span sets, or any
    // spans overlap, fail closed rather than mixing incompatible normalizations.
    if !stage1_hits.is_empty() && !stage2_hits.is_empty() && stage1_hits != stage2_hits {
        return Err(ToolError::MatchCount(unique_candidates.len()));
    }
    unique_candidates.sort_by_key(|&(start, end, _)| (start, end));
    if unique_candidates
        .windows(2)
        .any(|pair| pair[0].1 > pair[1].0)
    {
        return Err(ToolError::MatchCount(unique_candidates.len()));
    }
    let mut out = content.to_owned();
    for (start, end, replacement) in unique_candidates.into_iter().rev() {
        out.replace_range(start..end, &replacement);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// The description is the only place a model learns that a mutation's
    /// `precondition` is the digest `read_file` returned. A schema cannot say
    /// it, so losing it here silently breaks every edit against a real model.
    #[test]
    fn mutation_descriptions_state_the_read_before_write_digest_contract() {
        for name in ["edit_file", "write_file"] {
            let text = tool_description(name);
            assert!(
                text.contains("read_file"),
                "{name} must name the tool supplying the digest"
            );
            assert!(
                text.contains("precondition"),
                "{name} must name the argument carrying the digest"
            );
            assert!(
                text.contains("sha256"),
                "{name} must name the field read_file returns"
            );
        }
        let read = tool_description("read_file");
        assert!(read.contains("sha256") && read.contains("precondition"));
    }

    /// Rejections a model must be able to avoid rather than retry blindly.
    #[test]
    fn descriptions_state_the_contracts_that_reject_a_call() {
        let edit = tool_description("edit_file");
        assert!(edit.contains("exactly once"), "unique-match rule missing");
        assert!(edit.contains("verbatim"), "exact-match rule missing");
        let write = tool_description("write_file");
        assert!(
            write.contains("create_intent"),
            "create/overwrite split missing"
        );
        assert!(
            tool_description("process").contains("argv"),
            "argv-first rule missing"
        );
    }

    #[test]
    fn every_advertised_tool_has_a_description_beyond_its_name() {
        let fallback = tool_description("");
        for name in [
            "read_file",
            "list_directory",
            "search",
            "read_project_manifest",
            "edit_file",
            "write_file",
            "git_diff",
            "process",
        ] {
            let text = tool_description(name);
            assert_ne!(
                text, fallback,
                "{name} still falls back to the generic text"
            );
            assert!(
                text.len() > 80,
                "{name} description is too thin to state a contract"
            );
        }
    }

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
            turn_revision: 4,
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
        let dangling = registry.workspace_manifest().unwrap();
        assert!(dangling.contains_key(link_key));
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("/tmp", dir.path().join("link")).unwrap();
        let external_abs = registry.workspace_manifest().unwrap();
        assert!(external_abs.contains_key(link_key));
        assert_ne!(dangling[link_key], external_abs[link_key]);
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("../outside", dir.path().join("link")).unwrap();
        let parent_escape = registry.workspace_manifest().unwrap();
        assert_ne!(external_abs[link_key], parent_escape[link_key]);
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink("link", dir.path().join("link")).unwrap();
        let loop_link = registry.workspace_manifest().unwrap();
        assert_ne!(parent_escape[link_key], loop_link[link_key]);

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
        assert_eq!(ToolRegistry::raw_target_components(Path::new("/absolute")).unwrap()[0], json!(["root"]));
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
        assert!(process_schema.get("oneOf").is_none());
        assert!(process_schema["properties"].get("argv").is_some());
        assert!(process_schema["properties"].get("shell").is_some());
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
        // Because `registry` already read and edited `a.txt`, consecutive mutation without
        // explicit `precondition` succeeds via the engine-recorded snapshot!
        let (consecutive, consecutive_decision, _) = registry
            .prepare_for_engine(&call("write_file", &missing_precondition, None))
            .unwrap();
        assert_eq!(consecutive_decision, PolicyDecision::Ask);
        registry.execute_prepared(consecutive).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "next"
        );

        // A fresh registry that has NOT read `a.txt` still requires `read_file` first.
        let unread = ToolRegistry::new(dir.path(), None, &[], None).unwrap();
        assert!(matches!(
            unread.prepare_for_engine(&call("write_file", &missing_precondition, None)),
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

    #[test]
    fn read_pagination_binary_rejection_scoped_search_and_recursive_list_work() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        fs::create_dir_all(dir.path().join("secret")).unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        fs::write(dir.path().join("src/big.rs"), "l1\nl2\nl3\nl4\nl5\nl6\n").unwrap();
        fs::write(dir.path().join("src/sub/mod.rs"), "pub fn needle_fn() {}\n").unwrap();
        fs::write(dir.path().join("secret/keys.txt"), "needle_fn secret\n").unwrap();
        fs::write(
            dir.path().join("node_modules/pkg/index.js"),
            "needle_fn ignored\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/minified.js"),
            format!("{}needle_fn{}\n", "x".repeat(600), "y".repeat(600)),
        )
        .unwrap();
        fs::write(dir.path().join("src/bin.dat"), b"abc\0def").unwrap();

        let registry = ToolRegistry::new(dir.path(), None, &["secret/**".into()], None).unwrap();

        // Paginated read_file with offset + limit.
        let page = registry
            .invoke(&call(
                "read_file",
                &json!({"path":"src/big.rs","offset":3,"limit":2}),
                None,
            ))
            .unwrap();
        assert!(page.truncated);
        assert_eq!(page.value["content"], "l3\nl4\n");
        assert_eq!(page.value["start_line"], 3);
        assert_eq!(page.value["end_line"], 4);
        assert_eq!(page.value["total_lines"], 6);
        assert_eq!(page.value["has_more"], true);

        // Offset past EOF returns empty content cleanly.
        let past_eof = registry
            .invoke(&call(
                "read_file",
                &json!({"path":"src/big.rs","offset":99,"limit":5}),
                None,
            ))
            .unwrap();
        assert!(!past_eof.truncated);
        assert_eq!(past_eof.value["content"], "");

        // NUL-containing binary file is rejected cleanly.
        assert!(matches!(
            registry.invoke(&call("read_file", &json!({"path":"src/bin.dat"}), None)),
            Err(ToolError::Input(msg)) if msg.contains("binary")
        ));

        // list_directory distinguishes directories with `/` and respects deny_globs + ignored dirs.
        let root_list = registry
            .invoke(&call("list_directory", &json!({}), None))
            .unwrap();
        assert_eq!(root_list.value["entries"], json!(["src/"]));

        let glob_list = registry
            .invoke(&call(
                "list_directory",
                &json!({"path":"src","max_depth":3,"glob":"*.rs"}),
                None,
            ))
            .unwrap();
        assert_eq!(glob_list.value["entries"], json!(["big.rs", "sub/mod.rs"]));

        // search enforces deny_globs, skips node_modules, filters by glob, and truncates long lines.
        let scoped = registry
            .invoke(&call(
                "search",
                &json!({"query":"needle_fn","path":"src","glob":"*.rs"}),
                None,
            ))
            .unwrap();
        assert_eq!(
            scoped.value["matches"],
            json!(["src/sub/mod.rs:1:pub fn needle_fn() {}"])
        );

        let all_matches = registry
            .invoke(&call("search", &json!({"query":"needle_fn"}), None))
            .unwrap();
        let matches = all_matches.value["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert!(
            matches
                .iter()
                .all(|m| !m.as_str().unwrap().contains("secret/"))
        );
        assert!(
            matches
                .iter()
                .all(|m| !m.as_str().unwrap().contains("node_modules"))
        );
        assert!(matches.iter().any(|m| m.as_str().unwrap().ends_with('…')));

        // Auto-inferred create_intent + mkdir -p on missing nested file.
        approve(
            &registry,
            "write_file",
            &json!({"path":"generated/nested/out.txt","content":""}),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("generated/nested/out.txt")).unwrap(),
            ""
        );
        assert_ne!(
            registry
                .single_file_manifest_digest("generated/nested/out.txt")
                .unwrap(),
            "missing"
        );
        assert_eq!(
            registry
                .single_file_manifest_digest("generated/nested/missing.txt")
                .unwrap(),
            "missing"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn edit_file_supports_replace_all_and_uniqueness_checked_fallbacks() {
        // 1. Exact replace_all vs ambiguous rejection when replace_all is false.
        let repeated = "alpha = 1;\nbeta = 2;\nalpha = 1;\n";
        assert!(matches!(
            apply_edit_span(repeated, "alpha = 1;", "alpha = 9;", false),
            Err(ToolError::MatchCount(2))
        ));
        assert_eq!(
            apply_edit_span(repeated, "alpha = 1;", "alpha = 9;", true).unwrap(),
            "alpha = 9;\nbeta = 2;\nalpha = 9;\n"
        );

        // 2. CRLF & trailing whitespace normalization preserves CRLF endings.
        let crlf = "fn greet() {   \r\n    println!(\"hi\");\t\r\n}\r\n";
        let edited_crlf = apply_edit_span(
            crlf,
            "fn greet() {\n    println!(\"hi\");\n}\n",
            "fn greet() {\n    println!(\"hello\");\n}\n",
            false,
        )
        .unwrap();
        assert_eq!(
            edited_crlf,
            "fn greet() {\r\n    println!(\"hello\");\r\n}\r\n"
        );
        let ambig_ws = "item = 1   \nitem = 1\t\n";
        assert!(matches!(
            apply_edit_span(ambig_ws, "item = 1\n", "item = 2\n", false),
            Err(ToolError::MatchCount(2))
        ));
        assert_eq!(
            apply_edit_span(ambig_ws, "item = 1\n", "item = 2\n", true).unwrap(),
            "item = 2\nitem = 2\n"
        );

        // 3. Typographic curly-quote normalization across multi-byte UTF-8 spans.
        let curly = "let msg = \u{201C}don\u{2019}t panic\u{201D};\n";
        assert_eq!(
            apply_edit_span(
                curly,
                "let msg = \"don't panic\";",
                "let msg = \"stay calm\";",
                false
            )
            .unwrap(),
            "let msg = \"stay calm\";\n"
        );
        let ambig_curly = "\u{201C}dup\u{201D}\n\u{201C}dup\u{201D}\n";
        assert!(matches!(
            apply_edit_span(ambig_curly, "\"dup\"", "\"ok\"", false),
            Err(ToolError::MatchCount(2))
        ));
        assert_eq!(
            apply_edit_span(ambig_curly, "\"dup\"", "\"ok\"", true).unwrap(),
            "\"ok\"\n\"ok\"\n"
        );

        // 4. Uniform space-indentation shift (both positive and negative delta).
        let indented = "impl Demo {\n    fn calc(&self) -> u32 {\n        1 + 1\n    }\n}\n";
        let plus_shift = apply_edit_span(
            indented,
            "fn calc(&self) -> u32 {\n    1 + 1\n}",
            "fn calc(&self) -> u32 {\n\n    2 + 2\n}",
            false,
        )
        .unwrap();
        assert_eq!(
            plus_shift,
            "impl Demo {\n    fn calc(&self) -> u32 {\n\n        2 + 2\n    }\n}\n"
        );
        let unindented = "fn calc() -> u32 {\n    1 + 1\n}\n";
        let minus_shift = apply_edit_span(
            unindented,
            "    fn calc() -> u32 {\n        1 + 1\n    }\n",
            "    fn calc() -> u32 {\n        3 + 3\n    }\n",
            false,
        )
        .unwrap();
        assert_eq!(minus_shift, "fn calc() -> u32 {\n    3 + 3\n}\n");

        // Tab-indented or duplicate dedented blocks are safely rejected.
        let tabbed = "\tfn a() {\n\t\t1\n\t}\n";
        assert!(matches!(
            apply_edit_span(tabbed, "fn a() {\n    1\n}", "fn a() {\n    2\n}", false),
            Err(ToolError::MatchCount(0))
        ));
        let dup_indent = "  if ok {\n    run();\n  }\n    if ok {\n      run();\n    }\n";
        assert!(matches!(
            apply_edit_span(
                dup_indent,
                "if ok {\n  run();\n}",
                "if ok {\n  stop();\n}",
                false
            ),
            Err(ToolError::MatchCount(2))
        ));

        // Cross-tier ambiguity: Stage 1 matches line 1..2 (trailing whitespace), while
        // Stage 3 also matches line 4..5 (uniformly indented). Must fail closed with
        // MatchCount(2) instead of silently picking Stage 1's hit!
        let cross_tier =
            "if ready {\n    step();   \n}\n\n    if ready {\n        step();\n    }\n";
        assert!(matches!(
            apply_edit_span(
                cross_tier,
                "if ready {\n    step();\n}",
                "if ready {\n    done();\n}",
                false
            ),
            Err(ToolError::MatchCount(2))
        ));

        // Same-span deduplication: Stage 1 and Stage 3 both match the exact same byte
        // span `(0, len)` -> deduplicated to 1 unique candidate and applied cleanly.
        let same_span = "if ready {\n    step();   \n}\n";
        let edited_same = apply_edit_span(
            same_span,
            "if ready {\n    step();\n}",
            "if ready {\n    done();\n}",
            false,
        )
        .unwrap();
        assert_eq!(edited_same, "if ready {\n    done();\n}\n");
    }

    #[cfg(unix)]
    #[test]
    fn git_diff_excludes_deny_glob_tracked_patches_and_untracked_filenames() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("secret")).unwrap();
        fs::write(dir.path().join("allowed.txt"), "public-v1\n").unwrap();
        fs::write(dir.path().join("secret/keys.env"), "TOP_SECRET_V1\n").unwrap();

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

        fs::write(dir.path().join("allowed.txt"), "public-v2\n").unwrap();
        fs::write(dir.path().join("secret/keys.env"), "TOP_SECRET_V2_LEAK\n").unwrap();
        fs::write(
            dir.path().join("secret/untracked.key"),
            "UNTRACKED_SECRET\n",
        )
        .unwrap();
        fs::write(dir.path().join("visible_new.txt"), "new\n").unwrap();

        let registry =
            ToolRegistry::new(dir.path(), None, &["secret/**".to_owned()], None).unwrap();
        let out = registry
            .invoke(&call("git_diff", &json!({}), None))
            .unwrap();
        let patch = out.value["summary"].as_str().unwrap();
        assert!(patch.contains("public-v2"), "{patch}");
        assert!(patch.contains("visible_new.txt"), "{patch}");
        assert!(!patch.contains("keys.env"), "{patch}");
        assert!(!patch.contains("TOP_SECRET"), "{patch}");
        assert!(!patch.contains("untracked.key"), "{patch}");
    }
}
