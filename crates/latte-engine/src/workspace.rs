use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum PathError {
    #[error(
        "workspace path must be relative and may not contain parent, root, or platform-prefix components"
    )]
    Invalid,
    #[error("path escapes the canonical workspace")]
    Escape,
    #[error("symbolic links are forbidden for mutations: {0}")]
    Symlink(String),
    #[error("protected VCS metadata path cannot be modified: {0}")]
    Protected(String),
    #[error("path does not exist: {0}")]
    Missing(String),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("safe handle-relative mutation is unsupported on this platform")]
    #[allow(dead_code)]
    UnsupportedSafePathOperation,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkspacePath {
    root: PathBuf,
    safe_mutations_supported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub hash: String,
    pub size: u64,
    pub modified_ns: u128,
}

pub(crate) fn is_protected_vcs_component(name: &str) -> bool {
    if name.eq_ignore_ascii_case(".git") {
        return true;
    }
    if name.len() > 4 && name[..4].eq_ignore_ascii_case("git~") {
        return name[4..].bytes().all(|b| b.is_ascii_digit());
    }
    false
}

impl WorkspacePath {
    pub(crate) fn new(root: &Path) -> Result<Self, PathError> {
        Ok(Self {
            root: fs::canonicalize(root)?,
            safe_mutations_supported: cfg!(any(unix, windows)),
        })
    }
    #[cfg(test)]
    pub(crate) fn new_with_safe_support(root: &Path, supported: bool) -> Result<Self, PathError> {
        let mut value = Self::new(root)?;
        value.safe_mutations_supported = supported;
        Ok(value)
    }
    pub(crate) fn validate_mutation_support(&self) -> Result<(), PathError> {
        if self.safe_mutations_supported {
            Ok(())
        } else {
            Err(PathError::UnsupportedSafePathOperation)
        }
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    fn lexical(&self, input: &str) -> Result<PathBuf, PathError> {
        let path = Path::new(input);
        if input.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(PathError::Invalid);
        }
        Ok(self.root.join(path))
    }
    pub(crate) fn display(&self, input: &str) -> Result<String, PathError> {
        let p = self.lexical(input)?;
        p.strip_prefix(&self.root)
            .map_err(|_| PathError::Escape)?
            .to_str()
            .map(|value| value.replace('\\', "/"))
            .ok_or(PathError::Invalid)
    }
    pub(crate) fn read(&self, input: &str) -> Result<PathBuf, PathError> {
        let lexical = self.lexical(input)?;
        let canonical = fs::canonicalize(&lexical).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PathError::Missing(input.into())
            } else {
                e.into()
            }
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(PathError::Escape);
        }
        Ok(canonical)
    }
    pub(crate) fn mutation(&self, input: &str, allow_missing: bool) -> Result<PathBuf, PathError> {
        let target = self.lexical(input)?;
        let relative = target
            .strip_prefix(&self.root)
            .map_err(|_| PathError::Escape)?;
        let parts = relative.components().collect::<Vec<_>>();
        if parts.first().is_some_and(|part| {
            part.as_os_str()
                .to_str()
                .is_some_and(is_protected_vcs_component)
        }) {
            return Err(PathError::Protected(self.display(input)?));
        }
        let mut cursor = self.root.clone();
        for (index, part) in parts.iter().enumerate() {
            cursor.push(part.as_os_str());
            match fs::symlink_metadata(&cursor) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(PathError::Symlink(self.display(input)?));
                }
                Ok(meta) if index + 1 < parts.len() && !meta.file_type().is_dir() => {
                    return Err(PathError::Invalid);
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && allow_missing => {
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(PathError::Missing(cursor.display().to_string()));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(target)
    }
    pub(crate) fn read_with_identity(path: &Path) -> Result<(Vec<u8>, FileIdentity), PathError> {
        let data = fs::read(path)?;
        let meta = fs::metadata(path)?;
        let modified_ns = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        let identity = FileIdentity {
            hash: format!("{:x}", Sha256::digest(&data)),
            size: meta.len(),
            modified_ns,
        };
        Ok((data, identity))
    }
    pub(crate) fn identity(path: &Path) -> Result<FileIdentity, PathError> {
        Self::read_with_identity(path).map(|(_, identity)| identity)
    }

    #[cfg(any(unix, windows))]
    pub(crate) fn atomic_replace(
        &self,
        input: &str,
        data: &[u8],
        create: bool,
    ) -> Result<(), PathError> {
        self.validate_mutation_support()?;
        self.atomic_replace_with(input, data, create, || {})
    }
    #[cfg(any(unix, windows))]
    fn atomic_replace_with(
        &self,
        input: &str,
        data: &[u8],
        create: bool,
        after_parent_open: impl FnOnce(),
    ) -> Result<(), PathError> {
        use cap_std::{
            ambient_authority,
            fs::{Dir, OpenOptions},
        };
        let target = self.mutation(input, create)?;
        let relative = target
            .strip_prefix(&self.root)
            .map_err(|_| PathError::Escape)?;
        let name = relative.file_name().ok_or(PathError::Invalid)?;
        let mut parent = Dir::open_ambient_dir(&self.root, ambient_authority())?;
        if let Some(components) = relative.parent() {
            for component in components.components() {
                let next = match parent.open_dir(component.as_os_str()) {
                    Ok(dir) => dir,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                        parent.create_dir(component.as_os_str())?;
                        parent.open_dir(component.as_os_str())?
                    }
                    Err(error) => return Err(error.into()),
                };
                parent = next;
            }
        }
        after_parent_open();
        let existing_metadata = match parent.symlink_metadata(name) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(PathError::Symlink(input.into()));
                }
                Some(meta)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let temp = format!(".latte-{}.tmp", uuid::Uuid::now_v7());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = parent.open_with(&temp, &options)?;
        if let Err(error) = (|| {
            file.write_all(data)?;
            if let Some(meta) = &existing_metadata
                && meta.is_file()
            {
                let _ = file.set_permissions(meta.permissions());
            }
            file.sync_all()?;
            parent.rename(&temp, &parent, name)?;
            Ok::<_, std::io::Error>(())
        })() {
            let _ = parent.remove_file(&temp);
            return Err(error.into());
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    pub(crate) fn atomic_replace(
        &self,
        _input: &str,
        _data: &[u8],
        _create: bool,
    ) -> Result<(), PathError> {
        Err(PathError::UnsupportedSafePathOperation)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn held_parent_capability_prevents_swap_redirect() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("parent")).unwrap();
        fs::write(root.path().join("parent/a"), "old").unwrap();
        let workspace = WorkspacePath::new(root.path()).unwrap();
        workspace
            .atomic_replace_with("parent/a", b"new", false, || {
                fs::rename(root.path().join("parent"), root.path().join("held")).unwrap();
                symlink(outside.path(), root.path().join("parent")).unwrap();
            })
            .unwrap();
        assert!(!outside.path().join("a").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("held/a")).unwrap(),
            "new"
        );
    }

    #[test]
    fn atomic_replace_creates_missing_parent_dirs_and_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let workspace = WorkspacePath::new(root.path()).unwrap();
        workspace
            .atomic_replace("nested/deep/script.sh", b"#!/bin/sh\necho v1\n", true)
            .unwrap();
        let script = root.path().join("nested/deep/script.sh");
        assert!(script.is_file());
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        workspace
            .atomic_replace("nested/deep/script.sh", b"#!/bin/sh\necho v2\n", false)
            .unwrap();
        assert_eq!(fs::read_to_string(&script).unwrap(), "#!/bin/sh\necho v2\n");
        let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o111, 0o111);

        // Non-directory intermediate component is still rejected.
        assert!(matches!(
            workspace.mutation("nested/deep/script.sh/child.txt", true),
            Err(PathError::Invalid)
        ));
    }

    #[test]
    fn mutation_rejects_protected_git_paths_while_allowing_dotgithub_and_gitignore() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".git/config"), "[core]\n").unwrap();
        let workspace = WorkspacePath::new(root.path()).unwrap();

        for forbidden in [
            ".git",
            ".git/config",
            "./.git/hooks/pre-commit",
            ".GIT/config",
            "GIT~1/config",
            "git~2/HEAD",
        ] {
            assert!(
                matches!(
                    workspace.mutation(forbidden, true),
                    Err(PathError::Protected(_))
                ),
                "expected Protected for {forbidden}"
            );
            assert!(
                matches!(
                    workspace.atomic_replace(forbidden, b"pwned", true),
                    Err(PathError::Protected(_))
                ),
                "expected atomic_replace Protected for {forbidden}"
            );
        }

        workspace
            .atomic_replace(".gitignore", b"target/\n", true)
            .unwrap();
        workspace
            .atomic_replace(".github/workflows/ci.yml", b"name: ci\n", true)
            .unwrap();
        assert!(root.path().join(".gitignore").is_file());
        assert!(root.path().join(".github/workflows/ci.yml").is_file());
    }
}
