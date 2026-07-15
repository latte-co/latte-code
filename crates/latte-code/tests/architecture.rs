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

#[test]
fn ci_workflow_exposes_a_fail_closed_pr_gate_contract() {
    let workflow = include_str!("../../../.github/workflows/ci.yml");

    let trigger_contract = "on:\n  push:\n    branches: [main]\n  pull_request:\n    branches: [main]\n    types: [opened, synchronize, reopened, edited, ready_for_review]\n  merge_group:\n  workflow_dispatch:\n\npermissions:";
    assert!(workflow.contains(trigger_contract));
    assert!(!workflow.contains("paths:"));
    assert!(!workflow.contains("paths-ignore:"));
    assert!(!workflow.contains("continue-on-error:"));
    assert!(!workflow.contains("rerun"));
    assert!(workflow.contains(
        "group: ci-${{ github.event.pull_request.number || github.event.merge_group.head_sha || github.ref }}"
    ));
    assert!(workflow.contains("cancel-in-progress: ${{ github.event_name == 'pull_request' }}"));

    for (job, name) in [
        ("static", "Static (${{ matrix.os }})"),
        ("unit-tests", "UT (${{ matrix.os }})"),
        ("contract-tests", "Contract (${{ matrix.os }})"),
        ("e2e-tests", "E2E (${{ matrix.os }})"),
        ("doc-tests", "Documentation tests"),
        ("coverage-unit", "Coverage - UT (95%)"),
        ("coverage-e2e", "Coverage - E2E (80%)"),
        ("coverage-total", "Coverage - total (90%)"),
        ("dependency-audit", "Dependency audit"),
        ("windows-compile", "Windows compile"),
    ] {
        let header = format!("  {job}:\n    name: {name}\n    timeout-minutes:");
        assert!(
            workflow.contains(&header),
            "unstable or unbounded job: {job}"
        );
    }
    assert!(!workflow.contains("\n  coverage:\n"));
    for command in [
        "      - run: make coverage-unit",
        "      - run: make coverage-e2e",
        "      - run: make coverage-total",
    ] {
        assert!(workflow.contains(command), "missing independent {command}");
    }

    let pr_gate_start = workflow.find("  pr-gate:\n").expect("PR Gate job");
    assert!(!workflow[..pr_gate_start].contains("\n    if:"));
    let release_start = workflow
        .find("  release-build:\n")
        .expect("release build job");
    let pr_gate = &workflow[pr_gate_start..release_start];
    assert!(pr_gate.contains("    name: PR Gate\n"));
    assert!(pr_gate.contains("    if: ${{ always() }}\n"));
    assert!(pr_gate.contains("    timeout-minutes: 5\n"));

    let required_jobs = [
        "static",
        "unit-tests",
        "contract-tests",
        "e2e-tests",
        "doc-tests",
        "coverage-unit",
        "coverage-e2e",
        "coverage-total",
        "dependency-audit",
        "windows-compile",
    ];
    let needs_start = pr_gate.find("    needs:\n").expect("PR Gate needs");
    let steps_start = pr_gate.find("    steps:\n").expect("PR Gate steps");
    let actual_needs = pr_gate[needs_start + "    needs:\n".len()..steps_start]
        .lines()
        .map(|line| line.trim().trim_start_matches("- "))
        .collect::<Vec<_>>();
    assert_eq!(actual_needs, required_jobs);

    for job in required_jobs {
        assert!(
            pr_gate.contains(&format!("      - {job}\n")),
            "PR Gate does not need {job}"
        );
        assert!(
            pr_gate.contains(&format!("${{{{ needs.{job}.result }}}}")),
            "PR Gate does not inspect {job}'s result"
        );
    }
    assert!(pr_gate.contains("if [[ \"$result\" != \"success\" ]]"));
    assert!(!pr_gate.contains("release-build"));

    let release = &workflow[release_start..];
    assert!(release.contains("    name: Release build (${{ matrix.os }})\n"));
    assert!(release.contains("    timeout-minutes: 30\n"));
    assert!(release.contains(
        "    if: ${{ github.event_name == 'push' || github.event_name == 'workflow_dispatch' }}\n"
    ));
}
