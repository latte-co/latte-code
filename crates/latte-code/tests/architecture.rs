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
    // Git may materialize the workflow with CRLF on Windows. Normalize the
    // fixture so the contract checks its YAML semantics, not checkout EOLs.
    let workflow = include_str!("../../../.github/workflows/ci.yml").replace("\r\n", "\n");

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
        ("repository-quality", "Repository quality"),
        ("platform-check", "Check (${{ matrix.label }})"),
        ("platform-clippy", "Clippy (${{ matrix.label }})"),
        ("unit-tests", "UT (${{ matrix.label }})"),
        ("contract-tests", "Contract (${{ matrix.label }})"),
        ("e2e-portable", "E2E portable (${{ matrix.label }})"),
        ("e2e-unix", "E2E Unix PTY/process (${{ matrix.label }})"),
        ("release-build", "Release build (${{ matrix.label }})"),
        ("msrv", "MSRV (Rust 1.93)"),
        ("doc-tests", "Documentation tests"),
        ("coverage-unit", "Coverage - UT (95%)"),
        ("coverage-e2e", "Coverage - E2E (80%)"),
        ("coverage-total", "Coverage - total (90%)"),
        ("dependency-audit", "Dependency audit"),
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
    assert!(workflow.contains("uses: docker://rhysd/actionlint:1.7.12"));
    assert!(workflow.contains("koalaman/shellcheck:v0.11.0 scripts/*.sh"));
    assert!(workflow.contains("uses: actions/checkout@v7"));
    assert!(workflow.contains("uses: actions/upload-artifact@v7"));
    assert!(!workflow.contains("uses: actions/checkout@v4"));
    assert!(workflow.contains("toolchain: 1.93.0"));

    for line in workflow.lines().filter(|line| line.contains("run: cargo ")) {
        if !line.contains("cargo fmt") {
            assert!(
                line.contains("--locked"),
                "unlocked Cargo CI command: {line}"
            );
        }
    }

    let section = |job: &str, next: &str| {
        let start = workflow
            .find(&format!("  {job}:\n"))
            .unwrap_or_else(|| panic!("missing {job}"));
        let end = workflow[start..]
            .find(&format!("\n  {next}:\n"))
            .map_or(workflow.len(), |offset| start + offset);
        &workflow[start..end]
    };
    for job in [
        "platform-check",
        "platform-clippy",
        "unit-tests",
        "contract-tests",
        "e2e-portable",
        "release-build",
    ] {
        let next = match job {
            "platform-check" => "platform-clippy",
            "platform-clippy" => "unit-tests",
            "unit-tests" => "contract-tests",
            "contract-tests" => "e2e-portable",
            "e2e-portable" => "e2e-unix",
            "release-build" => "msrv",
            _ => unreachable!(),
        };
        let platform_job = section(job, next);
        for os in ["ubuntu-latest", "macos-latest", "windows-latest"] {
            assert!(platform_job.contains(os), "{job} does not run on {os}");
        }
    }
    let portable = section("e2e-portable", "e2e-unix");
    assert!(portable.contains("--test e2e_portable"));
    let unix = section("e2e-unix", "release-build");
    assert!(unix.contains("--test e2e_unix"));
    assert!(unix.contains("ubuntu-latest"));
    assert!(unix.contains("macos-latest"));
    assert!(!unix.contains("windows-latest"));

    let pr_gate_start = workflow.find("  pr-gate:\n").expect("PR Gate job");
    assert!(!workflow[..pr_gate_start].contains("\n    if:"));
    let pr_gate = &workflow[pr_gate_start..];
    assert!(pr_gate.contains("    name: PR Gate\n"));
    assert!(pr_gate.contains("    if: ${{ always() }}\n"));
    assert!(pr_gate.contains("    timeout-minutes: 5\n"));

    let required_jobs = [
        "repository-quality",
        "platform-check",
        "platform-clippy",
        "unit-tests",
        "contract-tests",
        "e2e-portable",
        "e2e-unix",
        "release-build",
        "msrv",
        "doc-tests",
        "coverage-unit",
        "coverage-e2e",
        "coverage-total",
        "dependency-audit",
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
    assert!(pr_gate.contains("${{ needs.release-build.result }}"));
}

#[test]
fn portable_and_unix_e2e_targets_have_explicit_platform_boundaries() {
    let portable = include_str!("e2e_portable.rs");
    let unix = include_str!("e2e_unix.rs");
    let portable_scenarios = include_str!("e2e/portable.rs");
    let support = include_str!("e2e/support.rs");

    assert!(!portable.contains("cfg(unix)"));
    assert!(portable.contains("e2e/portable.rs"));
    assert!(support.contains("CARGO_BIN_EXE_latte-code"));
    assert!(portable_scenarios.contains("ScriptedProvider"));
    assert!(portable_scenarios.contains("database_path"));
    assert!(unix.starts_with("#![cfg(unix)]"));
    assert!(unix.contains("e2e/mod.rs"));
}
