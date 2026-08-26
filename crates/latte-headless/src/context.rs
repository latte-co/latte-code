use std::{
    fs,
    path::{Component, Path, PathBuf},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextBundle {
    pub text: String,
    pub truncated: bool,
    pub sources: Vec<String>,
}
pub fn build(
    root: &Path,
    focus: Option<&Path>,
    cap: usize,
) -> Result<ContextBundle, std::io::Error> {
    let root = fs::canonicalize(root)?;
    let mut paths = Vec::new();
    let root_agents = root.join("AGENTS.md");
    if root_agents.is_file() {
        paths.push(contained_canonical(&root, &root_agents)?)
    }
    if let Some(focus) = focus {
        if focus.as_os_str().is_empty()
            || focus.components().any(|component| {
                matches!(
                    component,
                    Component::Prefix(_) | Component::RootDir | Component::ParentDir
                )
            })
        {
            return Err(focus_escape());
        }
        let focused = root.join(focus);
        // Resolve the closest existing ancestor. This catches both an existing
        // symlink focus and a not-yet-created focus below a symlink directory.
        let mut ancestor = focused.as_path();
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or_else(focus_escape)?;
        }
        let canonical_ancestor = fs::canonicalize(ancestor)?;
        if !canonical_ancestor.starts_with(&root) {
            return Err(focus_escape());
        }
        let focused = if focused.exists() {
            fs::canonicalize(&focused)?
        } else {
            focused
        };
        if !focused.starts_with(&root) {
            return Err(focus_escape());
        }
        let mut current = if focused.is_dir() {
            Some(focused)
        } else {
            focused.parent().map(Path::to_owned)
        };
        let mut nested = Vec::new();
        while let Some(dir) = current {
            if !dir.starts_with(&root) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "focus escapes workspace",
                ));
            }
            let candidate = dir.join("AGENTS.md");
            if candidate.is_file() && !paths.contains(&candidate) {
                nested.push(contained_canonical(&root, &candidate)?)
            }
            if dir == root {
                break;
            }
            current = dir.parent().map(Path::to_owned)
        }
        nested.reverse();
        paths.extend(nested);
    }
    for name in ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"] {
        let path = root.join(name);
        if path.is_file() {
            paths.push(path)
        }
    }
    let mut text = String::new();
    let mut sources = Vec::new();
    let mut truncated = false;
    for path in paths {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read_to_string(&path)?;
        let section = format!("\n--- {relative} ---\n{content}");
        let remaining = cap.saturating_sub(text.len());
        if section.len() > remaining {
            let mut end = remaining.min(section.len());
            while !section.is_char_boundary(end) {
                end -= 1
            }
            text.push_str(&section[..end]);
            truncated = true;
            break;
        }
        text.push_str(&section);
        sources.push(relative)
    }
    Ok(ContextBundle {
        text,
        truncated,
        sources,
    })
}

fn focus_escape() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "focus must be workspace-relative and remain within the workspace",
    )
}

fn contained_canonical(root: &Path, path: &Path) -> Result<PathBuf, std::io::Error> {
    let canonical = fs::canonicalize(path)?;
    if !canonical.starts_with(root) {
        return Err(focus_escape());
    }
    Ok(canonical)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orders_root_nearest_manifest_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "manifest").unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        fs::write(dir.path().join("src/AGENTS.md"), "near").unwrap();
        let bundle = build(dir.path(), Some(Path::new("src/deep/file.rs")), 1024).unwrap();
        assert_eq!(
            bundle.sources,
            vec!["AGENTS.md", "src/AGENTS.md", "Cargo.toml"]
        );
        assert!(bundle.text.find("root").unwrap() < bundle.text.find("near").unwrap());
        assert!(build(dir.path(), None, 8).unwrap().truncated);
        assert!(build(&dir.path().join("missing"), None, 8).is_err());
        assert_eq!(
            build(dir.path(), Some(Path::new("")), 8)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        fs::create_dir_all(dir.path().join("folder")).unwrap();
        fs::write(dir.path().join("folder/AGENTS.md"), "éé").unwrap();
        let unicode = build(dir.path(), Some(Path::new("folder")), 33).unwrap();
        assert!(unicode.truncated);
        assert!(unicode.text.is_char_boundary(unicode.text.len()));
    }

    #[test]
    fn rejects_non_relative_and_parent_focuses() {
        let dir = tempfile::tempdir().unwrap();
        for focus in [
            Path::new("/tmp"),
            Path::new("../outside"),
            Path::new("a/../../b"),
        ] {
            assert_eq!(
                build(dir.path(), Some(focus), 1024).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn truncation_backs_off_to_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root").unwrap();
        fs::create_dir_all(dir.path().join("folder")).unwrap();
        fs::write(dir.path().join("folder/AGENTS.md"), "éé").unwrap();
        // Section sizes: root = 24 bytes, folder = 31 bytes. A cap of 52
        // leaves 28 bytes for the folder section, which lands inside the
        // first multi-byte `é`, forcing the char-boundary back-off.
        let bundle = build(dir.path(), Some(Path::new("folder")), 52).unwrap();
        assert!(bundle.truncated);
        assert!(bundle.text.is_char_boundary(bundle.text.len()));
        assert!(bundle.text.ends_with("folder/AGENTS.md ---\n"));
    }

    #[test]
    fn invalid_utf8_manifest_fails_the_build() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), &[0xFF, 0xFE, 0xFD][..]).unwrap();
        assert!(build(dir.path(), None, 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_external_symlink_ancestors_and_agents_files() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("AGENTS.md"), "external secret").unwrap();

        symlink(external.path(), workspace.path().join("linked")).unwrap();
        assert_eq!(
            build(
                workspace.path(),
                Some(Path::new("linked/not-created/file.rs")),
                1024
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::PermissionDenied
        );

        fs::create_dir(workspace.path().join("safe")).unwrap();
        symlink(
            external.path().join("AGENTS.md"),
            workspace.path().join("safe/AGENTS.md"),
        )
        .unwrap();
        assert_eq!(
            build(workspace.path(), Some(Path::new("safe/file.rs")), 1024)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
