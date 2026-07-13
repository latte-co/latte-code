use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

#[test]
fn workspace_dependency_matrix_is_exact() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let workspace = BTreeSet::from([
        "latte-core",
        "latte-engine",
        "latte-headless",
        "latte-tui",
        "latte-code",
    ]);
    let mut actual = BTreeMap::new();
    for package in metadata["packages"].as_array().unwrap() {
        let name = package["name"].as_str().unwrap();
        if workspace.contains(name) {
            let dependencies = package["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|dependency| dependency["name"].as_str())
                .filter(|dependency| workspace.contains(dependency))
                .collect::<BTreeSet<_>>();
            actual.insert(name, dependencies);
        }
    }
    assert_eq!(actual["latte-core"], BTreeSet::new());
    assert_eq!(actual["latte-engine"], BTreeSet::from(["latte-core"]));
    assert_eq!(
        actual["latte-headless"],
        BTreeSet::from(["latte-core", "latte-engine"])
    );
    assert_eq!(actual["latte-tui"], BTreeSet::from(["latte-core"]));
    assert_eq!(
        actual["latte-code"],
        BTreeSet::from(["latte-core", "latte-engine", "latte-headless", "latte-tui"])
    );
}
