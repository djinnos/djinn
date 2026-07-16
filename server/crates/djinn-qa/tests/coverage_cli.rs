use std::{fs, process::Command};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn fixture(status: &str, blocked: bool) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("qa")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(temp.path().join("qa/taxonomy.yaml"), "version: 1\ncoverage:\n- id: task.state-machine.legal-transitions\n  subsystem: task-state-machine\n  required_profiles: [smoke-ci]\n").unwrap();
    fs::write(temp.path().join("qa/scenarios.yaml"), format!("version: 1\nscenarios:\n- id: fixture.scenario\n  version: 1\n  enabled: true\n  profiles: [smoke-ci]\n  sources: [{{kind: memory, id: fixture}}]\n  primary_coverage: task.state-machine.legal-transitions\n  execution: {{kind: cargo-package, package: fixture, selector: fixture::test}}\n  isolation: {{database: isolated, providers: isolated, channel: isolated}}\n  watch_paths: [src/lib.rs]\n{}", if blocked { "  blocked_dependency: runner-pending\n" } else { "" })).unwrap();
    if !status.is_empty() {
        fs::write(temp.path().join("qa/evidence.yaml"), format!("version: 1\nevidence:\n- scenario_id: fixture.scenario\n  scenario_version: 1\n  taxonomy_version: 1\n  requirement_id: task.state-machine.legal-transitions\n  covered_ids: [task.state-machine.legal-transitions]\n  profile: smoke-ci\n  status: {status}\n  evidence_sha: {SHA}\n  started_at: 2026-01-01T00:00:00Z\n  finished_at: 2026-01-01T00:00:01Z\n  runner: {{name: fixture, version: '1'}}\n")).unwrap();
    }
    temp
}

fn coverage(root: &std::path::Path, format: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_djinn-qa"))
        .args([
            "coverage",
            "--profile",
            "smoke-ci",
            "--format",
            format,
            "--repo-root",
        ])
        .arg(root)
        .args(["--current-sha", SHA])
        .output()
        .unwrap()
}

/// Runner output intentionally has a different schema from the legacy EvidenceSet YAML
/// fixture above. Keep this helper at the CLI boundary so these tests exercise directory
/// detection, deserialization, and coverage classification together.
fn runner_artifact(status: &str, sha: &str, scenario_version: u32) -> String {
    serde_json::json!({
        "scenario_id": "fixture.scenario",
        "scenario_version": scenario_version,
        "taxonomy_version": 1,
        "requirement_id": "task.state-machine.legal-transitions",
        "covered_ids": ["task.state-machine.legal-transitions"],
        "profile": "smoke-ci",
        "status": status,
        "git_sha": sha,
        "runner": { "name": "djinn-qa", "version": "0.1.0" },
        "sources": [{ "kind": "memory", "id": "fixture" }],
        "watch_paths": ["src/lib.rs"],
        "started_at": "2026-01-01T00:00:00Z",
        "finished_at": "2026-01-01T00:00:01Z",
        "diagnostics": ["fixture"]
    })
    .to_string()
}

fn coverage_with_evidence(
    root: &std::path::Path,
    evidence: &std::path::Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_djinn-qa"))
        .args([
            "coverage",
            "--profile",
            "smoke-ci",
            "--format",
            "json",
            "--repo-root",
        ])
        .arg(root)
        .args(["--current-sha", SHA, "--evidence"])
        .arg(evidence)
        .output()
        .unwrap()
}

#[test]
fn table_json_output_and_exit_states_are_contractual() {
    for (status, blocked, expected, success) in [
        ("passed", false, "proven", true),
        ("", false, "unproven", false),
        ("failed", false, "failing", false),
        ("passed", true, "stale", false),
    ] {
        let repo = fixture(status, blocked);
        let output = coverage(repo.path(), "json");
        assert_eq!(
            output.status.success(),
            success,
            "{expected}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let row = &rows[0];
        assert_eq!(row["state"], expected);
        assert_eq!(row.as_object().unwrap().len(), 10);
        for key in [
            "coverage_id",
            "subsystem",
            "required_profiles",
            "scenario_ids",
            "evidence_path",
            "last_passed_at",
            "last_evidence_sha",
            "stale_reasons",
            "memory_sources",
        ] {
            assert!(row.get(key).is_some(), "missing {key}");
        }
        let table = coverage(repo.path(), "table");
        let table = String::from_utf8(table.stdout).unwrap();
        assert!(table.starts_with("coverage_id\tsubsystem\t"));
        assert!(table.contains(&format!("\t{expected}\t")));
    }
}

#[test]
fn json_output_file_is_written() {
    let repo = fixture("passed", false);
    let output_path = repo.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_djinn-qa"))
        .args([
            "coverage",
            "--profile",
            "smoke-ci",
            "--format",
            "json",
            "--repo-root",
        ])
        .arg(repo.path())
        .args(["--current-sha", SHA, "--output"])
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(output_path).unwrap())
            .unwrap()[0]["state"],
        "proven"
    );
}

#[test]
fn runner_artifact_directory_proves_coverage_and_creates_report_parent() {
    let repo = fixture("", false);
    let evidence = repo.path().join("qa/evidence/smoke-ci");
    fs::create_dir_all(&evidence).unwrap();
    fs::write(
        evidence.join("fixture.scenario.json"),
        runner_artifact("passed", SHA, 1),
    )
    .unwrap();

    let report = evidence.join("reports/coverage.json");
    let output = Command::new(env!("CARGO_BIN_EXE_djinn-qa"))
        .args([
            "coverage",
            "--profile",
            "smoke-ci",
            "--format",
            "json",
            "--repo-root",
        ])
        .arg(repo.path())
        .args(["--current-sha", SHA, "--evidence"])
        .arg(&evidence)
        .args(["--output"])
        .arg(&report)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(report).unwrap()).unwrap()[0]
            ["state"],
        "proven"
    );
}

#[test]
fn runner_artifact_directory_fails_closed_for_invalid_or_non_passing_evidence() {
    for (name, artifact, expected_state) in [
        ("missing", None, "unproven"),
        ("malformed", Some("not JSON".to_owned()), "unproven"),
        ("failed", Some(runner_artifact("failed", SHA, 1)), "failing"),
        (
            "identity-mismatch",
            Some(runner_artifact("passed", SHA, 2)),
            "stale",
        ),
        (
            "stale-sha",
            Some(runner_artifact("passed", &"b".repeat(40), 1)),
            "stale",
        ),
    ] {
        let repo = fixture("", false);
        let evidence = repo.path().join("qa/evidence/smoke-ci");
        fs::create_dir_all(&evidence).unwrap();
        if let Some(artifact) = artifact {
            fs::write(evidence.join("fixture.scenario.json"), artifact).unwrap();
        }

        let output = coverage_with_evidence(repo.path(), &evidence);
        assert!(
            !output.status.success(),
            "{name}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()[0]["state"],
            expected_state,
            "{name}"
        );
    }
}
