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
    fs::write(temp.path().join("qa/scenarios.yaml"), format!("version: 1\nscenarios:\n- id: fixture.scenario\n  version: 1\n  enabled: true\n  profiles: [smoke-ci]\n  sources: [{{kind: memory, id: fixture}}]\n  primary_coverage: task.state-machine.legal-transitions\n  execution: {{kind: cargo-package, package: fixture}}\n  isolation: {{database: isolated, providers: isolated, channel: isolated}}\n  watch_paths: [src/lib.rs]\n{}", if blocked { "  blocked_dependency: runner-pending\n" } else { "" })).unwrap();
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
        assert!(
            String::from_utf8(table.stdout)
                .unwrap()
                .starts_with("coverage_id\tsubsystem\t")
        );
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
