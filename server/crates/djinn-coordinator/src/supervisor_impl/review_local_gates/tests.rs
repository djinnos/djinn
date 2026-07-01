//! Focused tests for the reviewer pre-approve local gate path.
//!
//! These tests exercise the pure `evaluate_review_gates` function with
//! stubbed `CommandRunner` implementations — no real processes, no DB.

use super::*;
use crate::local_gates::ExecOutput;
use djinn_core::models::Task;
use std::time::Duration;

// ── Test doubles ──────────────────────────────────────────────────────────────

/// A `CommandRunner` that returns a pre-configured `ExecOutput` for every call.
struct StubRunner {
    output: ExecOutput,
}

impl CommandRunner for StubRunner {
    fn run(
        &self,
        _repo_root: &Path,
        _command: &[&str],
        _cwd: &str,
        _timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecOutput> + Send>> {
        let out = self.output.clone();
        Box::pin(async move { out })
    }
}

fn passed_output() -> ExecOutput {
    ExecOutput {
        exit_code: Some(0),
        stdout: "ok".to_string(),
        stderr: String::new(),
        duration: Duration::from_millis(10),
        unavailable: false,
    }
}

fn failed_output() -> ExecOutput {
    ExecOutput {
        exit_code: Some(1),
        stdout: String::new(),
        stderr: "error: fmt check failed".to_string(),
        duration: Duration::from_millis(20),
        unavailable: false,
    }
}

fn unavailable_output() -> ExecOutput {
    ExecOutput {
        exit_code: None,
        stdout: String::new(),
        stderr: "command not found: cargo".to_string(),
        duration: Duration::from_millis(1),
        unavailable: true,
    }
}

/// Build a minimal `Task` with the given CI metadata.
fn task_with_ci(check_names: &str, fingerprint: Option<&str>) -> Task {
    Task {
        id: "task-1".into(),
        project_id: "proj-1".into(),
        short_id: "t1".into(),
        epic_id: None,
        title: "Test task".into(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".into(),
        status: "in_task_review".into(),
        priority: 0,
        owner: String::new(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: Some("https://github.com/test/repo/pull/1".into()),
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: String::new(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: check_names.to_string(),
        ci_failure_fingerprint: fingerprint.map(|s| s.to_string()),
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        unresolved_blocker_count: 0,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn advisory_only_gates_do_not_block_approval() {
    // A task with no CI check names matches no gates at all.
    let task = task_with_ci("", None);
    let runner = StubRunner {
        output: passed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    assert!(matches!(verdict, ReviewGateVerdict::Allow));
}

#[tokio::test]
async fn non_applicable_gates_do_not_block_approval() {
    // A task whose check names don't match any gate in the registry.
    let task = task_with_ci(r#"["Vercel Preview", "Deploy to Staging"]"#, None);
    let runner = StubRunner {
        output: passed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    assert!(matches!(verdict, ReviewGateVerdict::Allow));
}

#[tokio::test]
async fn passing_required_gates_allow_approval() {
    // The task has a check name that matches a required gate, and the runner
    // returns success.
    let task = task_with_ci(r#"["server-size-guard"]"#, None);
    let runner = StubRunner {
        output: passed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    assert!(matches!(verdict, ReviewGateVerdict::Allow));
}

#[tokio::test]
async fn failing_required_gate_blocks_approval() {
    // The task matches a required gate, but the runner returns a non-zero exit.
    let task = task_with_ci(r#"["rustfmt"]"#, None);
    let runner = StubRunner {
        output: failed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    match &verdict {
        ReviewGateVerdict::Block { block_json, result } => {
            // The block JSON should contain the gate id and failure details.
            assert!(block_json.contains("rustfmt"));
            assert!(block_json.contains("has_unavailable"));
            // has_unavailable should be false (exit code was 1, not unavailable).
            let parsed: serde_json::Value = serde_json::from_str(block_json).unwrap();
            assert_eq!(parsed["has_unavailable"], false);
            // The result should have a blocking failure.
            assert!(result.has_blocking_failure());
            assert!(result.blocking_gate_ids().contains(&"rustfmt"));
        }
        ReviewGateVerdict::Allow => panic!("expected Block, got Allow"),
    }
}

#[tokio::test]
async fn unavailable_required_gate_blocks_approval_with_unavailable_flag() {
    // The task matches a required gate, but the command binary is missing.
    let task = task_with_ci(r#"["server-size-guard"]"#, None);
    let runner = StubRunner {
        output: unavailable_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    match &verdict {
        ReviewGateVerdict::Block { block_json, result } => {
            let parsed: serde_json::Value = serde_json::from_str(block_json).unwrap();
            // has_unavailable should be true.
            assert_eq!(parsed["has_unavailable"], true);
            // blocking_reason should mention "unavailable".
            let reason = parsed["blocking_reason"].as_str().unwrap();
            assert!(
                reason.contains("unavailable"),
                "expected 'unavailable' in reason, got: {reason}"
            );
            // The result should have a blocking failure.
            assert!(result.has_blocking_failure());
        }
        ReviewGateVerdict::Allow => panic!("expected Block, got Allow"),
    }
}

#[tokio::test]
async fn empty_check_names_produces_allow() {
    // Empty check names means no gates apply.
    let task = task_with_ci("", None);
    let runner = StubRunner {
        output: failed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    assert!(matches!(verdict, ReviewGateVerdict::Allow));
}

#[tokio::test]
async fn malformed_check_names_json_produces_allow() {
    // Malformed JSON should be tolerated (returns empty vec → no gates).
    let task = task_with_ci("not-valid-json", None);
    let runner = StubRunner {
        output: failed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    assert!(matches!(verdict, ReviewGateVerdict::Allow));
}

#[tokio::test]
async fn block_json_contains_structured_gate_details() {
    // Verify the block JSON carries structured details suitable for activity
    // logging.
    let task = task_with_ci(r#"["rustfmt"]"#, Some("rustfmt-failure-abc"));
    let runner = StubRunner {
        output: failed_output(),
    };
    let tmp = tempfile::tempdir().unwrap();

    let verdict = evaluate_review_gates(&task, tmp.path(), &runner).await;
    match &verdict {
        ReviewGateVerdict::Block { block_json, .. } => {
            let parsed: serde_json::Value = serde_json::from_str(block_json).unwrap();
            let details = parsed["details"].as_array().unwrap();
            assert!(!details.is_empty(), "expected at least one detail entry");
            let first = &details[0];
            assert_eq!(first["gate_id"], "rustfmt");
            assert_eq!(first["outcome"], "failed");
            // command, cwd, timeout_secs should be present.
            assert!(first["command"].is_array());
            assert!(first["cwd"].is_string());
            assert!(first["timeout_secs"].is_number());
            // exit_code should be 1.
            assert_eq!(first["exit_code"], 1);
            // stderr_summary should contain the error.
            assert!(
                first["stderr_summary"]
                    .as_str()
                    .unwrap()
                    .contains("fmt check failed")
            );
        }
        ReviewGateVerdict::Allow => panic!("expected Block, got Allow"),
    }
}
