use std::{
    fs,
    path::{Path, PathBuf},
};

#[test]
fn local_markdown_links_resolve() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("lattecode crate must be inside the workspace")
        .to_path_buf();
    let mut markdown = vec![root.join("README.md"), root.join("AGENTS.md")];
    collect_markdown(&root.join("docs"), &mut markdown);

    assert!(
        markdown.iter().any(|path| path == &root.join("README.md")),
        "workspace README was not checked"
    );
    assert!(
        markdown.iter().any(|path| path == &root.join("AGENTS.md")),
        "workspace AGENTS.md was not checked"
    );

    let mut broken = Vec::new();
    for source in markdown {
        let body = fs::read_to_string(&source).expect("Markdown must be valid UTF-8");
        for (line_index, line) in body.lines().enumerate() {
            for destination in markdown_destinations(line) {
                if is_external_or_document_anchor(destination) {
                    continue;
                }
                let path_part = destination.split('#').next().unwrap_or_default();
                if path_part.is_empty() {
                    continue;
                }
                let target = source
                    .parent()
                    .expect("Markdown source must have a parent")
                    .join(path_part);
                if !target.exists() {
                    broken.push(format!(
                        "{}:{} -> {}",
                        source.strip_prefix(&root).unwrap_or(&source).display(),
                        line_index + 1,
                        destination
                    ));
                }
            }
        }
    }

    assert!(
        broken.is_empty(),
        "broken local Markdown links:\n{}",
        broken.join("\n")
    );
}

fn collect_markdown(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("workspace directory must be readable") {
        let entry = entry.expect("directory entry must be readable");
        let path = entry.path();
        if path.is_dir() {
            collect_markdown(&path, output);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
            output.push(path);
        }
    }
}

fn markdown_destinations(line: &str) -> Vec<&str> {
    let mut destinations = Vec::new();
    let mut remainder = line;
    while let Some(start) = remainder.find("](") {
        remainder = &remainder[start + 2..];
        let Some(end) = remainder.find(')') else {
            break;
        };
        let destination = remainder[..end].trim();
        if !destination.is_empty() && !destination.contains(char::is_whitespace) {
            destinations.push(destination);
        }
        remainder = &remainder[end + 1..];
    }
    destinations
}

fn is_external_or_document_anchor(destination: &str) -> bool {
    destination.starts_with('#')
        || destination.starts_with("http://")
        || destination.starts_with("https://")
        || destination.starts_with("mailto:")
}
