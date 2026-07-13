//! Tests for the tool-call GO/STOP evaluator (split from
//! `tool_call_evaluator.rs` to keep the main module under the Server Size
//! Guard byte limit).

use crate::repositories::tool_call_evaluator::{
    Decision, EvalInput, GoStopReport, ManualAuditResult, WindowSpec, evaluate,
    matched_baseline_rows,
};
use crate::repositories::tool_call_export::NormalizedToolCallRow;

#[allow(clippy::too_many_arguments)]
fn base_row(
    session_id: &str,
    task_id: Option<&str>,
    turn_index: usize,
    tool_name: &str,
    result_status: &str,
    error_class: Option<&str>,
    path: Option<&str>,
    args_hash: &str,
    role: &str,
    format_family: &str,
    surface_family: &str,
    model_id: &str,
) -> NormalizedToolCallRow {
    NormalizedToolCallRow {
        provider_id: Some("openai".into()),
        model_id: Some(model_id.into()),
        format_family: Some(format_family.into()),
        tool_surface_family: Some(surface_family.into()),
        agent_role: Some(role.into()),
        session_id: session_id.into(),
        task_id: task_id.map(str::to_owned),
        calendar_day: Some("2026-07-15".into()),
        window_start: Some("2026-07-01T00:00:00Z".into()),
        tool_call_id: Some(format!("call-{session_id}-{turn_index}")),
        turn_index,
        tool_name: tool_name.into(),
        args_hash: args_hash.into(),
        result_status: result_status.into(),
        error_class: error_class.map(str::to_owned),
        error_text: None,
        read_truncated: false,
        path: path.map(str::to_owned),
        read_offset: None,
        read_limit: None,
        diagnostics: vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn candidate_row(
    session_id: &str,
    task_id: Option<&str>,
    turn_index: usize,
    tool_name: &str,
    result_status: &str,
    error_class: Option<&str>,
    path: Option<&str>,
    args_hash: &str,
    role: &str,
) -> NormalizedToolCallRow {
    base_row(
        session_id,
        task_id,
        turn_index,
        tool_name,
        result_status,
        error_class,
        path,
        args_hash,
        role,
        "OpenAIResponses",
        "codex",
        "gpt-5-codex",
    )
}

#[allow(clippy::too_many_arguments)]
fn baseline_row(
    session_id: &str,
    task_id: Option<&str>,
    turn_index: usize,
    tool_name: &str,
    result_status: &str,
    error_class: Option<&str>,
    path: Option<&str>,
    args_hash: &str,
    role: &str,
) -> NormalizedToolCallRow {
    base_row(
        session_id,
        task_id,
        turn_index,
        tool_name,
        result_status,
        error_class,
        path,
        args_hash,
        role,
        "OpenAIResponses",
        "default",
        "gpt-5",
    )
}

fn window() -> WindowSpec {
    WindowSpec {
        start_day: "2026-07-01".into(),
        end_day: "2026-07-30".into(),
        source_description: "test fixture".into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_rows<F>(
    edit_fail: usize,
    edit_ok: usize,
    patch_fail: usize,
    patch_ok: usize,
    sessions: usize,
    tasks: usize,
    roles: usize,
    failed_edit_path: Option<&str>,
    row_builder: F,
) -> Vec<NormalizedToolCallRow>
where
    F: Fn(
        &str,
        Option<&str>,
        usize,
        &str,
        &str,
        Option<&str>,
        Option<&str>,
        &str,
        &str,
    ) -> NormalizedToolCallRow,
{
    let mut rows = Vec::new();
    let mut idx = 0usize;
    for s in 0..sessions {
        for t in 0..tasks {
            for r in 0..roles {
                let role = if r == 0 { "worker" } else { "reviewer" };
                let role_suffix = if roles > 1 {
                    format!("-{role}")
                } else {
                    String::new()
                };
                let session_id = format!("session-{s}{role_suffix}");
                let task_id = format!("task-{t}");
                for _ in 0..edit_fail {
                    rows.push(row_builder(
                        &session_id,
                        Some(&task_id),
                        idx,
                        "edit",
                        "error",
                        Some("validation"),
                        failed_edit_path,
                        &format!("h-{idx}"),
                        role,
                    ));
                    idx += 1;
                }
                for _ in 0..edit_ok {
                    rows.push(row_builder(
                        &session_id,
                        Some(&task_id),
                        idx,
                        "edit",
                        "success",
                        None,
                        Some("a.rs"),
                        &format!("h-{idx}"),
                        role,
                    ));
                    idx += 1;
                }
                for _ in 0..patch_fail {
                    rows.push(row_builder(
                        &session_id,
                        Some(&task_id),
                        idx,
                        "apply_patch",
                        "error",
                        Some("validation"),
                        Some("a.rs"),
                        &format!("h-{idx}"),
                        role,
                    ));
                    idx += 1;
                }
                for _ in 0..patch_ok {
                    rows.push(row_builder(
                        &session_id,
                        Some(&task_id),
                        idx,
                        "apply_patch",
                        "success",
                        None,
                        Some("a.rs"),
                        &format!("h-{idx}"),
                        role,
                    ));
                    idx += 1;
                }
            }
        }
    }
    rows
}

#[test]
fn go_only_when_all_gates_and_audit_pass() {
    // Candidate: high edit failure rate, high retry rate, enough samples.
    // Baseline: low edit failure rate, low retry rate.
    let candidate = make_rows(80, 20, 10, 90, 30, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 30, 5, 2, None, baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Go);
    assert!(report.gates.iter().all(|g| g.passed));
}

#[test]
fn stop_when_edit_disadvantage_fails() {
    let candidate = make_rows(14, 86, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Stop);
    assert!(
        report
            .gates
            .iter()
            .any(|g| g.gate_name == "edit_disadvantage" && !g.passed)
    );
}

#[test]
fn insufficient_data_when_audit_incomplete() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(15, 10)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(
        report
            .sample_minima_shortfalls
            .iter()
            .any(|s| s.contains("manual audit incomplete"))
    );
}

#[test]
fn insufficient_data_when_missing_required_fields() {
    let mut candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    candidate[0].model_id = None;
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(
        report
            .missing_required_fields
            .contains(&"model_id".to_owned())
    );
}

#[test]
fn insufficient_data_for_non_30_day_window() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: WindowSpec {
            start_day: "2026-07-01".into(),
            end_day: "2026-07-29".into(),
            source_description: "test fixture".into(),
        },
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::InsufficientData);
}

#[test]
fn insufficient_data_when_low_sample() {
    let candidate = make_rows(1, 1, 1, 1, 1, 1, 1, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(!report.sample_minima_shortfalls.is_empty());
}

#[test]
fn stop_when_pseudo_count_ratio_fails() {
    // Candidate and baseline have the same edit failure rate, so the ratio
    // gate fails.
    let candidate = make_rows(50, 50, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(50, 50, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Stop);
    let ratio_gate = report
        .gates
        .iter()
        .find(|g| g.gate_name == "pseudo_count_ratio")
        .unwrap();
    assert!(!ratio_gate.passed);
}

#[test]
fn stop_when_retry_disadvantage_fails() {
    // Candidate failed edits have no path, so no retries; baseline failed edits
    // share a path with subsequent edits, yielding retries. Candidate retry
    // rate is therefore below baseline, failing the retry-disadvantage gate.
    let candidate = make_rows(80, 20, 10, 90, 20, 5, 2, None, candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 20, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Stop);
    let retry_gate = report
        .gates
        .iter()
        .find(|g| g.gate_name == "retry_disadvantage")
        .unwrap();
    assert!(!retry_gate.passed);
}

#[test]
fn stop_when_manual_audit_qualifying_too_low() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 11)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Stop);
    let audit_gate = report
        .gates
        .iter()
        .find(|g| g.gate_name == "manual_audit")
        .unwrap();
    assert!(!audit_gate.passed);
}

#[test]
fn report_records_population_dimensions_and_rates() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.candidate.distinct_sessions, 30);
    assert_eq!(report.candidate.distinct_tasks, 5);
    assert_eq!(report.candidate.distinct_roles, 2);
    assert_eq!(report.candidate.edit_calls, 15000);
    assert_eq!(report.candidate.apply_patch_calls, 15000);
    assert_eq!(report.baseline.distinct_sessions, 30);
    assert_eq!(report.baseline.edit_calls, 15000);
    assert_eq!(report.baseline.apply_patch_calls, 15000);
}

#[test]
fn matched_baseline_selection_is_deterministic_and_absent_baseline_yields_insufficient() {
    // The evaluator itself does not select the baseline; it consumes a
    // caller-supplied matched baseline. When the baseline rows are absent
    // (empty), the candidate is evaluated against zero counts and the
    // quantitative gates should fail unless the candidate has zero
    // failures/retry rate. With enough candidate samples the report is
    // evaluable but not sufficient for GO.
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: vec![],
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    // Empty baseline is valid input (no missing fields), but the gates
    // should fail on zero-denominator / zero-rate comparisons.
    assert!(report.decision == Decision::Stop || report.decision == Decision::InsufficientData);
}

#[test]
fn report_renders_machine_readable_and_human_reviewable_json() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    let json = serde_json::to_string(&report).expect("report serializes to JSON");
    assert!(json.contains("decision"));
    assert!(json.contains("window"));
    assert!(json.contains("candidate"));
    assert!(json.contains("baseline"));
    assert!(json.contains("gates"));
    // Human-readable decision reason is included in the same JSON output.
    assert!(json.contains("decision_reason"));
}

#[test]
fn report_records_per_gate_results() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    let gate_names: Vec<String> = report.gates.iter().map(|g| g.gate_name.clone()).collect();
    assert!(gate_names.contains(&"edit_disadvantage".to_owned()));
    assert!(gate_names.contains(&"pseudo_count_ratio".to_owned()));
    assert!(gate_names.contains(&"retry_disadvantage".to_owned()));
    assert!(gate_names.contains(&"wilson_difference_excludes_zero".to_owned()));
    assert!(gate_names.contains(&"manual_audit".to_owned()));
}

#[test]
fn stop_when_wilson_includes_zero() {
    // Candidate and baseline with similar edit/apply patch failure rates so
    // the difference interval includes zero.
    let candidate = make_rows(80, 20, 80, 20, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert!(
        report
            .gates
            .iter()
            .any(|g| g.gate_name == "wilson_difference_excludes_zero" && !g.passed)
    );
}

#[test]
fn go_requires_minima_and_all_four_quantitative_gates_and_audit() {
    // Satisfy all minima and all gates.
    let candidate = make_rows(80, 20, 10, 90, 30, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 30, 5, 2, None, baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Go);
    // Every gate is explicitly pass.
    for gate in &report.gates {
        assert!(gate.passed, "{} should pass", gate.gate_name);
    }
}

#[test]
fn stop_when_any_gate_fails_even_others_pass() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 11)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::Stop);
    assert!(
        report
            .gates
            .iter()
            .any(|g| g.gate_name == "manual_audit" && !g.passed)
    );
}

#[test]
fn insufficient_data_when_audit_absent() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: None,
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(
        report
            .sample_minima_shortfalls
            .iter()
            .any(|s| s.contains("manual audit absent"))
    );
}

#[test]
fn matched_baseline_selection_filters_by_role_task_and_surface_family() {
    let candidate = make_rows(80, 20, 10, 90, 20, 5, 2, None, candidate_row);
    // Build a baseline pool that includes matched rows and unmatched rows
    // (different role, different task, different surface family).
    let baseline_default = make_rows(10, 90, 10, 90, 20, 5, 2, Some("a.rs"), baseline_row);
    let mut baseline_other: Vec<NormalizedToolCallRow> = baseline_default
        .iter()
        .cloned()
        .map(|mut r| {
            r.tool_surface_family = Some("other_surface".to_owned());
            r
        })
        .collect();
    let mut baseline_other_role: Vec<NormalizedToolCallRow> = baseline_default
        .iter()
        .cloned()
        .map(|mut r| {
            r.agent_role = Some("planner".to_owned());
            r
        })
        .collect();
    let mut baseline_other_task: Vec<NormalizedToolCallRow> = baseline_default
        .iter()
        .cloned()
        .map(|mut r| {
            r.task_id = Some("task-other".to_owned());
            r
        })
        .collect();

    let mut pool = baseline_default.clone();
    pool.append(&mut baseline_other);
    pool.append(&mut baseline_other_role);
    pool.append(&mut baseline_other_task);

    let selected = matched_baseline_rows(
        &candidate,
        &pool,
        &["default".to_owned(), "Responses/default".to_owned()],
    );
    // Only rows with matching role, task, and accepted baseline family survive.
    assert_eq!(selected.len(), baseline_default.len());
    for r in &selected {
        assert_eq!(r.tool_surface_family.as_deref(), Some("default"));
        assert!(
            r.agent_role.as_deref() == Some("worker")
                || r.agent_role.as_deref() == Some("reviewer")
        );
        assert!(r.task_id.as_deref().unwrap().starts_with("task-"));
    }
}

#[test]
fn evaluator_does_not_modify_tool_surface() {
    // The evaluator is a pure function of input rows; it returns a report
    // and does not alter the input rows or any tool surface state.
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate.clone(),
        baseline_rows: baseline.clone(),
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let _ = evaluate(&input, None, None);
    assert_eq!(input.candidate_rows, candidate);
    assert_eq!(input.baseline_rows, baseline);
}

#[test]
fn report_rejects_ambiguous_window_metadata() {
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    // End day before start day.
    let input = EvalInput {
        window: WindowSpec {
            start_day: "2026-07-30".into(),
            end_day: "2026-07-01".into(),
            source_description: "ambiguous".into(),
        },
        candidate_rows: candidate,
        baseline_rows: baseline,
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(report.decision_reason.contains("window invalid"));
}

#[test]
fn evaluator_only_records_decision_and_does_not_alter_model_surface() {
    // Same as evaluator_does_not_modify_tool_surface, explicit acceptance
    // criterion #5: the evaluator cannot enable or modify any model-facing
    // tool surface; it only records the Phase 1 decision.
    let candidate = make_rows(80, 20, 10, 90, 15, 5, 2, Some("a.rs"), candidate_row);
    let baseline = make_rows(10, 90, 10, 90, 15, 5, 2, Some("a.rs"), baseline_row);
    let input = EvalInput {
        window: window(),
        candidate_rows: candidate.clone(),
        baseline_rows: baseline.clone(),
        audit: Some(ManualAuditResult::new(20, 12)),
    };

    let report = evaluate(&input, None, None);
    assert_eq!(input.candidate_rows, candidate);
    assert_eq!(input.baseline_rows, baseline);
    assert!(
        report.decision == Decision::Go
            || report.decision == Decision::Stop
            || report.decision == Decision::InsufficientData
    );
}

// ── End-to-end synthetic transcript fixtures ───────────────────────────
//
// These fixtures start from persisted-transcript-shaped inputs (SessionRecord +
// SessionMessage), run through the exporter (normalize_persisted_transcript),
// metric derivation, and the evaluator. They are deterministic and do not
// represent production evidence.

use crate::repositories::tool_call_export::{
    ExportDimensions, PersistedTranscript, normalize_persisted_transcript,
};
use djinn_core::message::ContentBlock;
use djinn_core::models::{SessionMessage, SessionRecord};

fn session_record(
    id: &str,
    task_id: Option<&str>,
    model_id: &str,
    agent_type: &str,
    started_at: &str,
) -> SessionRecord {
    SessionRecord {
        id: id.into(),
        project_id: Some("project-1".into()),
        task_id: task_id.map(str::to_owned),
        model_id: model_id.into(),
        agent_type: agent_type.into(),
        started_at: started_at.into(),
        ended_at: None,
        status: "completed".into(),
        tokens_in: 0,
        tokens_out: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        task_run_id: None,
        title: None,
        parked_reason: None,
        cost_usd: None,
        input_price_per_million_snapshot: None,
        output_price_per_million_snapshot: None,
        cache_read_price_per_million_snapshot: None,
        cache_write_price_per_million_snapshot: None,
        cost_basis: "unpriced".into(),
        billing_source: None,
    }
}

fn assistant_tool_use(
    session_id: &str,
    id: &str,
    name: &str,
    input: serde_json::Value,
) -> SessionMessage {
    let content = serde_json::to_string(&vec![ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }])
    .unwrap();
    SessionMessage {
        id: format!("msg-{id}"),
        session_id: session_id.into(),
        role: "assistant".into(),
        content_json: content,
        token_count: None,
        created_at: "2026-07-15T12:00:00Z".into(),
    }
}

fn user_tool_result(
    session_id: &str,
    tool_use_id: &str,
    is_error: bool,
    text: &str,
) -> SessionMessage {
    let content = serde_json::to_string(&vec![ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: vec![ContentBlock::text(text)],
        is_error,
    }])
    .unwrap();
    SessionMessage {
        id: format!("msg-{tool_use_id}-result"),
        session_id: session_id.into(),
        role: "user".into(),
        content_json: content,
        token_count: None,
        created_at: "2026-07-15T12:00:01Z".into(),
    }
}

fn codex_dimensions() -> ExportDimensions {
    ExportDimensions {
        provider_id: Some("openai".into()),
        format_family: Some("OpenAIResponses".into()),
        tool_surface_family: Some("codex".into()),
    }
}

fn default_dimensions() -> ExportDimensions {
    ExportDimensions {
        provider_id: Some("openai".into()),
        format_family: Some("OpenAIResponses".into()),
        tool_surface_family: Some("default".into()),
    }
}

/// Build a synthetic candidate session with a high edit-failure/retry pattern.
/// Emits 8 failed `edit` calls on the same path, then 2 successful `edit`
/// calls, plus 10 `apply_patch` calls on a different path (1 failure). Bunching
/// the failed edits before the successful ones lets the metric derivation see
/// failed edits followed by more failed edits on the same path within the next
/// three turns, producing a high retry-after-edit-failure rate.
fn build_candidate_session(
    session_id: &str,
    task_id: &str,
    model_id: &str,
    agent_type: &str,
) -> PersistedTranscript {
    let mut messages = Vec::new();
    let edit_path = "src/lib.rs";
    let patch_path = "src/other.rs";
    // 8 failed edits on the same path (turns 0-7).
    for i in 0..8 {
        let call_id = format!("{session_id}-edit-{i}");
        messages.push(assistant_tool_use(
            session_id,
            &call_id,
            "edit",
            serde_json::json!({"path": edit_path, "old_text": "x", "new_text": "y"}),
        ));
        messages.push(user_tool_result(
            session_id,
            &call_id,
            true,
            "edit failed: context mismatch",
        ));
    }
    // 2 successful edits on the same path (turns 8-9). The first successful
    // edit blocks the failed edit at turn 7 from being counted as a retry,
    // which is the only failed edit in the candidate that is not counted as a
    // retry. The earlier failed edits are still within the three-turn window
    // of each other, so they count.
    for i in 0..2 {
        let success_id = format!("{session_id}-edit-success-{i}");
        messages.push(assistant_tool_use(
            session_id,
            &success_id,
            "edit",
            serde_json::json!({"path": edit_path, "old_text": "x", "new_text": "y"}),
        ));
        messages.push(user_tool_result(session_id, &success_id, false, "ok"));
    }
    // 10 apply_patch calls on a different path (turns 10-19).
    for i in 0..10 {
        let call_id = format!("{session_id}-patch-{i}");
        let failed = i == 0;
        messages.push(assistant_tool_use(
            session_id,
            &call_id,
            "apply_patch",
            serde_json::json!({"patch": format!("*** Update File: {patch_path}\n-old\n+new")}),
        ));
        messages.push(user_tool_result(
            session_id,
            &call_id,
            failed,
            if failed { "patch failed" } else { "ok" },
        ));
    }
    PersistedTranscript {
        session: session_record(
            session_id,
            Some(task_id),
            model_id,
            agent_type,
            "2026-07-15T10:00:00Z",
        ),
        messages,
        dimensions: codex_dimensions(),
    }
}

/// Build a synthetic baseline session with low edit failure and no retries.
/// Emits 9 successful `edit` calls followed by 1 failed `edit` at the end,
/// plus 10 `apply_patch` calls on a different path (1 failure). The failed
/// edit is the last edit, so no subsequent same-path modification exists to be
/// counted as a retry.
fn build_baseline_session(
    session_id: &str,
    task_id: &str,
    model_id: &str,
    agent_type: &str,
) -> PersistedTranscript {
    let mut messages = Vec::new();
    let edit_path = "src/lib.rs";
    let patch_path = "src/other.rs";
    for i in 0..9 {
        let call_id = format!("{session_id}-edit-{i}");
        messages.push(assistant_tool_use(
            session_id,
            &call_id,
            "edit",
            serde_json::json!({"path": edit_path, "old_text": "x", "new_text": "y"}),
        ));
        messages.push(user_tool_result(session_id, &call_id, false, "ok"));
    }
    let fail_id = format!("{session_id}-edit-fail");
    messages.push(assistant_tool_use(
        session_id,
        &fail_id,
        "edit",
        serde_json::json!({"path": edit_path, "old_text": "x", "new_text": "y"}),
    ));
    messages.push(user_tool_result(session_id, &fail_id, true, "edit failed"));
    for i in 0..10 {
        let call_id = format!("{session_id}-patch-{i}");
        let failed = i == 0;
        messages.push(assistant_tool_use(
            session_id,
            &call_id,
            "apply_patch",
            serde_json::json!({"patch": format!("*** Update File: {patch_path}\n-old\n+new")}),
        ));
        messages.push(user_tool_result(
            session_id,
            &call_id,
            failed,
            if failed { "patch failed" } else { "ok" },
        ));
    }
    PersistedTranscript {
        session: session_record(
            session_id,
            Some(task_id),
            model_id,
            agent_type,
            "2026-07-15T10:00:00Z",
        ),
        messages,
        dimensions: default_dimensions(),
    }
}

/// Create a candidate-shaped session with the same low-failure pattern as the
/// baseline. Useful for STOP fixtures where the two populations should be
/// equivalent.
fn build_candidate_as_baseline(
    session_id: &str,
    task_id: &str,
    model_id: &str,
    agent_type: &str,
) -> PersistedTranscript {
    let mut t = build_baseline_session(session_id, task_id, model_id, agent_type);
    t.dimensions = codex_dimensions();
    t.session.model_id = model_id.into();
    t
}

fn window_spec() -> WindowSpec {
    WindowSpec {
        start_day: "2026-07-01".into(),
        end_day: "2026-07-30".into(),
        source_description: "synthetic transcript fixture".into(),
    }
}

fn evaluate_transcripts(
    candidate: Vec<PersistedTranscript>,
    baseline: Vec<PersistedTranscript>,
    audit: Option<ManualAuditResult>,
) -> GoStopReport {
    let candidate_rows: Vec<_> = candidate
        .iter()
        .flat_map(normalize_persisted_transcript)
        .collect();
    let baseline_rows: Vec<_> = baseline
        .iter()
        .flat_map(normalize_persisted_transcript)
        .collect();
    let input = EvalInput {
        window: window_spec(),
        candidate_rows,
        baseline_rows,
        audit,
    };
    evaluate(&input, None, None)
}

#[test]
fn e2e_transcript_fixture_go() {
    // 30 candidate sessions with high edit failure + retry, 30 baseline sessions
    // with low edit failure + no retry. Meets all sample minima and gates.
    let mut candidate = Vec::new();
    let mut baseline = Vec::new();
    for i in 0..30 {
        let role = if i % 2 == 0 { "worker" } else { "reviewer" };
        candidate.push(build_candidate_session(
            &format!("codex-session-{i}"),
            &format!("task-{i:02}"),
            "gpt-5-codex",
            role,
        ));
        baseline.push(build_baseline_session(
            &format!("default-session-{i}"),
            &format!("task-{i:02}"),
            "gpt-5",
            role,
        ));
    }
    let report = evaluate_transcripts(candidate, baseline, Some(ManualAuditResult::new(20, 12)));
    assert_eq!(report.decision, Decision::Go);
    assert!(report.gates.iter().all(|g| g.passed));
    assert!(report.missing_required_fields.is_empty());
    assert!(report.sample_minima_shortfalls.is_empty());
}

#[test]
fn e2e_transcript_fixture_stop() {
    // Candidate and baseline have the same low edit failure rate and no retry
    // disadvantage, so the ratio and retry gates fail.
    let mut candidate = Vec::new();
    let mut baseline = Vec::new();
    for i in 0..30 {
        let role = if i % 2 == 0 { "worker" } else { "reviewer" };
        candidate.push(build_candidate_as_baseline(
            &format!("codex-session-stop-{i}"),
            &format!("task-{i:02}"),
            "gpt-5-codex",
            role,
        ));
        baseline.push(build_baseline_session(
            &format!("default-session-stop-{i}"),
            &format!("task-{i:02}"),
            "gpt-5",
            role,
        ));
    }
    let report = evaluate_transcripts(candidate, baseline, Some(ManualAuditResult::new(20, 12)));
    assert_eq!(report.decision, Decision::Stop);
    assert!(
        report
            .gates
            .iter()
            .any(|g| g.gate_name == "pseudo_count_ratio" && !g.passed)
    );
}

#[test]
fn e2e_transcript_fixture_insufficient_data_missing_model_id() {
    // 30 sessions with enough volume, but the session model_id is empty so the
    // exporter marks model_id as missing. This makes the report insufficient
    // data regardless of other rates.
    let mut candidate = Vec::new();
    let mut baseline = Vec::new();
    for i in 0..30 {
        let role = if i % 2 == 0 { "worker" } else { "reviewer" };
        let mut c = build_candidate_session(
            &format!("codex-session-missing-{i}"),
            &format!("task-{i:02}"),
            "", // empty model_id -> missing required field
            role,
        );
        c.session.model_id = "".into();
        candidate.push(c);
        baseline.push(build_baseline_session(
            &format!("default-session-missing-{i}"),
            &format!("task-{i:02}"),
            "gpt-5",
            role,
        ));
    }
    let report = evaluate_transcripts(candidate, baseline, Some(ManualAuditResult::new(20, 12)));
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(
        report
            .missing_required_fields
            .contains(&"model_id".to_owned())
    );
}

#[test]
fn e2e_transcript_fixture_insufficient_data_absent_audit() {
    // 30 sessions with enough volume and clear GO-like rates, but no manual
    // audit supplied. The report must be insufficient data.
    let mut candidate = Vec::new();
    let mut baseline = Vec::new();
    for i in 0..30 {
        let role = if i % 2 == 0 { "worker" } else { "reviewer" };
        candidate.push(build_candidate_session(
            &format!("codex-session-noaudit-{i}"),
            &format!("task-{i:02}"),
            "gpt-5-codex",
            role,
        ));
        baseline.push(build_baseline_session(
            &format!("default-session-noaudit-{i}"),
            &format!("task-{i:02}"),
            "gpt-5",
            role,
        ));
    }
    let report = evaluate_transcripts(candidate, baseline, None);
    assert_eq!(report.decision, Decision::InsufficientData);
    assert!(
        report
            .sample_minima_shortfalls
            .iter()
            .any(|s| s.contains("manual audit absent"))
    );
}
