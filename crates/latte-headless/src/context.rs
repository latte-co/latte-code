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
#[allow(clippy::too_many_lines)]
pub fn build(
    root: &Path,
    focus: Option<&Path>,
    cap: usize,
) -> Result<ContextBundle, std::io::Error> {
    let root = fs::canonicalize(root)?;
    let mut paths = Vec::new();
    for doc_name in ["AGENTS.md", "CLAUDE.md"] {
        let root_doc = root.join(doc_name);
        if let Some(canonical) = safe_contained_file(&root, &root_doc)
            && !paths.contains(&canonical)
        {
            paths.push(canonical);
        }
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
            for doc_name in ["CLAUDE.md", "AGENTS.md"] {
                let candidate = dir.join(doc_name);
                if let Some(canonical) = safe_contained_file(&root, &candidate)
                    && !paths.contains(&canonical)
                    && !nested.contains(&canonical)
                {
                    nested.push(canonical);
                }
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
        if let Some(canonical) = safe_contained_file(&root, &path)
            && !paths.contains(&canonical)
        {
            paths.push(canonical);
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
    if !truncated
        && cap >= 512
        && let Some(overview) = build_workspace_overview(&root)
    {
        let section = format!("\n--- workspace overview ---\n{overview}");
        let remaining = cap.saturating_sub(text.len());
        if section.len() <= remaining {
            text.push_str(&section);
        }
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

fn safe_contained_file(root: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return None;
    }
    Some(canonical)
}

fn read_safe_git_head(root: &Path) -> Option<String> {
    use std::io::Read;
    let git_dir = root.join(".git");
    let git_meta = fs::symlink_metadata(&git_dir).ok()?;
    if git_meta.file_type().is_symlink() || !git_meta.file_type().is_dir() {
        return None;
    }
    let head_path = git_dir.join("HEAD");
    let head_meta = fs::symlink_metadata(&head_path).ok()?;
    if head_meta.file_type().is_symlink() || !head_meta.file_type().is_file() {
        return None;
    }
    let canonical = safe_contained_file(root, &head_path)?;
    let file = fs::File::open(canonical).ok()?;
    let mut bytes = Vec::with_capacity(256);
    file.take(256).read_to_end(&mut bytes).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?.trim();
    if let Some(branch) = text.strip_prefix("ref: refs/heads/") {
        if !branch.is_empty()
            && branch.len() <= 200
            && !branch.contains("..")
            && !branch.starts_with('/')
            && !branch.ends_with('/')
            && branch
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
        {
            return Some(format!("git_branch: {branch}"));
        }
        return None;
    }
    if (40..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(format!("git_head: {}", &text[..12]));
    }
    None
}

fn build_workspace_overview(root: &Path) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(head_line) = read_safe_git_head(root) {
        lines.push(head_line);
    }
    if let Ok(read_dir) = fs::read_dir(root) {
        let mut entries = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(
                name.as_str(),
                ".git" | ".latte" | ".oh-my-code" | ".opencode" | ".worktree" | ".tmp" | "target"
            ) {
                continue;
            }
            let suffix = match entry.file_type() {
                Ok(ft) if ft.is_dir() => "/",
                Ok(ft) if ft.is_symlink() => "@",
                _ => "",
            };
            entries.push(format!("{name}{suffix}"));
        }
        entries.sort();
        if !entries.is_empty() {
            let total = entries.len();
            entries.truncate(60);
            let mut listing = entries.join(", ");
            if total > 60 {
                let _ = std::fmt::Write::write_fmt(
                    &mut listing,
                    format_args!(", … (+{} more)", total - 60),
                );
            }
            lines.push(format!("top_level: {listing}"));
        }
    }
    (!lines.is_empty()).then(|| format!("{}\n", lines.join("\n")))
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
        // Section sizes: root = 23 bytes, folder = 30 bytes. A cap of 50
        // leaves 27 bytes for the folder section, which lands on the
        // continuation byte of the first `é`, forcing the char-boundary
        // back-off.
        let bundle = build(dir.path(), Some(Path::new("folder")), 50).unwrap();
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
    fn skips_external_symlinked_manifests_without_reading_external_secrets() {
        use std::os::unix::fs::symlink;

        for name in ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"] {
            let workspace = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            fs::write(external.path().join(name), "external secret").unwrap();
            symlink(external.path().join(name), workspace.path().join(name)).unwrap();
            let bundle = build(workspace.path(), None, 1024).unwrap();
            assert!(!bundle.sources.iter().any(|s| s == name));
            assert!(!bundle.text.contains("external secret"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_external_symlink_focus_ancestors_and_skips_external_agents_or_claude_files() {
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
        fs::write(workspace.path().join("CLAUDE.md"), "claude root guide").unwrap();
        symlink(
            external.path().join("AGENTS.md"),
            workspace.path().join("safe/AGENTS.md"),
        )
        .unwrap();
        let bundle = build(workspace.path(), Some(Path::new("safe/file.rs")), 1024).unwrap();
        assert_eq!(bundle.sources, vec!["CLAUDE.md"]);
        assert!(bundle.text.contains("claude root guide"));
        assert!(!bundle.text.contains("external secret"));
    }

    #[test]
    fn workspace_overview_validates_git_head_and_rejects_symlinks_and_non_utf8_slices() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        fs::create_dir(workspace.path().join(".git")).unwrap();

        // Valid symbolic ref.
        fs::write(
            workspace.path().join(".git/HEAD"),
            "ref: refs/heads/feat/safe-head\n",
        )
        .unwrap();
        let branch_bundle = build(workspace.path(), None, 4096).unwrap();
        assert!(
            branch_bundle.text.contains("git_branch: feat/safe-head"),
            "{}",
            branch_bundle.text
        );

        // Valid 40-hex detached OID.
        fs::write(
            workspace.path().join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        let oid_bundle = build(workspace.path(), None, 4096).unwrap();
        assert!(
            oid_bundle.text.contains("git_head: 0123456789ab"),
            "{}",
            oid_bundle.text
        );

        // Multi-byte UTF-8 where raw `&head[..12]` would split a 4-byte codepoint, plus arbitrary secret text.
        for bad_head in ["😀😀😀😀😀\n", "super-secret-outside-line-123456\n"] {
            fs::write(workspace.path().join(".git/HEAD"), bad_head).unwrap();
            let bad_bundle = build(workspace.path(), None, 4096).unwrap();
            assert!(!bad_bundle.text.contains("git_branch:"));
            assert!(!bad_bundle.text.contains("git_head:"));
            assert!(!bad_bundle.text.contains("super-secret"));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            let secret_head = outside.path().join("HEAD");
            fs::write(&secret_head, "ref: refs/heads/leaked-outside-branch\n").unwrap();
            fs::remove_file(workspace.path().join(".git/HEAD")).unwrap();
            symlink(&secret_head, workspace.path().join(".git/HEAD")).unwrap();
            let sym_bundle = build(workspace.path(), None, 4096).unwrap();
            assert!(!sym_bundle.text.contains("leaked-outside-branch"));
            assert!(!sym_bundle.text.contains("git_branch:"));
            assert!(!sym_bundle.text.contains("git_head:"));
        }
    }
}
