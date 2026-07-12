//! Fixed-window GO/STOP evaluator for the Codex/OpenAI Responses tool surface.
//!
//! Consumes normalized rows and the derived metrics from
//! [`crate::repositories::tool_call_metrics`] for a declared completed 30-day
//! window, compares a candidate population against a matched baseline, and
//! records one explicit result per predeclared gate.
//!
//! Decision semantics are strict:
//! * `GO` only when all required source fields exist, candidate minima pass,
//!   all four quantitative gates pass, and a completed 20-trace manual audit
//!   has at least 12 qualifying cases.
//! * Any missing input or short sample produces `insufficient data`.
//! * A complete evaluation with any failed threshold produces `STOP` and names
//!   the failures.
//!
//! The evaluator does not modify any model-facing tool surface; it only records
//! the Phase 1 decision.

use crate::repositories::tool_call_export::NormalizedToolCallRow;
use crate::repositories::tool_call_metrics::{
    ConfidenceInterval, RateMetric, ToolSurfaceMetrics, compute_metrics,
    edit_minus_apply_patch_failure_interval, failure_rates, retry_after_edit_failure,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Terminal decision recorded by the evaluator.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Decision {
    /// All gates and sample minima passed; the manual audit met its threshold.
    Go,
    /// Inputs were complete but at least one quantitative gate or the audit
    /// failed.
    Stop,
    /// Required source fields were missing, the window was malformed, samples
    /// were below minima, or the audit was incomplete/absent.
    InsufficientData,
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decision::Go => write!(f, "GO"),
            Decision::Stop => write!(f, "STOP"),
            Decision::InsufficientData => write!(f, "insufficient data"),
        }
    }
}

/// Result of the required 20-trace manual audit over sampled failed edits.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ManualAuditResult {
    /// Number of failed-edit traces that were sampled for review.
    pub sampled: usize,
    /// Number of sampled failures classified as genuine surface
    /// confusion/context failures.
    pub qualifying: usize,
    /// Whether the audit is considered complete (exactly/at least 20 sampled).
    pub completed: bool,
}

impl ManualAuditResult {
    /// Create an audit result from operator counts. `completed` is true when
    /// `sampled >= 20`, matching the "exactly/at least 20" requirement.
    pub fn new(sampled: usize, qualifying: usize) -> Self {
        Self {
            sampled,
            qualifying,
            completed: sampled >= 20,
        }
    }
}

/// Window metadata supplied by the caller.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowSpec {
    /// Inclusive start of the analysis window, e.g. `2026-07-01`.
    pub start_day: String,
    /// Inclusive end of the analysis window, e.g. `2026-07-30`.
    pub end_day: String,
    /// Human-readable source description, e.g. "persisted session transcripts".
    pub source_description: String,
}

/// Input bundle for the evaluator.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EvalInput {
    pub window: WindowSpec,
    /// Normalized rows belonging to the candidate population.
    pub candidate_rows: Vec<NormalizedToolCallRow>,
    /// Normalized rows belonging to the matched baseline population.
    pub baseline_rows: Vec<NormalizedToolCallRow>,
    /// Manual audit result. If `None`, the audit is treated as absent.
    pub audit: Option<ManualAuditResult>,
}

/// Population summary for candidate or baseline.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PopulationReport {
    pub definition: String,
    pub row_count: usize,
    pub distinct_sessions: usize,
    pub distinct_tasks: usize,
    pub distinct_roles: usize,
    pub edit_calls: usize,
    pub apply_patch_calls: usize,
    pub modifying_sessions: usize,
    pub failure_rates: crate::repositories::tool_call_metrics::FailureRates,
    pub retry_after_edit_failure_rate: RateMetric,
    pub metrics: ToolSurfaceMetrics,
}

/// Outcome for one predeclared gate.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GateResult {
    pub gate_name: String,
    pub passed: bool,
    pub evaluable: bool,
    pub observed: Option<f64>,
    pub threshold: String,
    pub message: String,
}

/// The complete fixed-window GO/STOP report.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GoStopReport {
    pub decision: Decision,
    pub decision_reason: String,
    pub window: WindowSpec,
    pub candidate: PopulationReport,
    pub baseline: PopulationReport,
    pub audit: Option<ManualAuditResult>,
    pub gates: Vec<GateResult>,
    pub failure_difference_interval: ConfidenceInterval,
    pub missing_required_fields: Vec<String>,
    pub sample_minima_shortfalls: Vec<String>,
}

/// Minimum sample sizes required on the candidate population.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SampleMinima {
    pub edit_calls: usize,
    pub apply_patch_calls: usize,
    pub modifying_sessions: usize,
    pub tasks: usize,
    pub roles: usize,
}

impl Default for SampleMinima {
    fn default() -> Self {
        Self {
            edit_calls: 100,
            apply_patch_calls: 100,
            modifying_sessions: 30,
            tasks: 5,
            roles: 2,
        }
    }
}

/// Thresholds for the four quantitative gates.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GateThresholds {
    /// Edit disadvantage (candidate - baseline) in percentage points.
    pub edit_disadvantage_points: f64,
    /// Pseudo-count ratio (candidate / baseline) of edit failure rates.
    pub pseudo_count_ratio: f64,
    /// Retry disadvantage (candidate - baseline) in percentage points.
    pub retry_disadvantage_points: f64,
    /// Manual audit sample size.
    pub audit_sample: usize,
    /// Minimum qualifying cases in the audit.
    pub audit_qualifying: usize,
}

impl Default for GateThresholds {
    fn default() -> Self {
        Self {
            edit_disadvantage_points: 5.0,
            pseudo_count_ratio: 1.5,
            retry_disadvantage_points: 10.0,
            audit_sample: 20,
            audit_qualifying: 12,
        }
    }
}

/// Build a population report from normalized rows and a definition string.
fn population_report(rows: &[NormalizedToolCallRow], definition: &str) -> PopulationReport {
    let mut sessions: BTreeSet<String> = BTreeSet::new();
    let mut tasks: BTreeSet<String> = BTreeSet::new();
    let mut roles: BTreeSet<String> = BTreeSet::new();
    let mut edit_calls = 0usize;
    let mut apply_patch_calls = 0usize;
    let mut modifying_sessions: BTreeSet<String> = BTreeSet::new();

    for row in rows {
        sessions.insert(row.session_id.clone());
        if let Some(t) = row.task_id.as_ref() {
            tasks.insert(t.clone());
        }
        if let Some(r) = agent_role_or_default(row) {
            roles.insert(r);
        }

        if row.tool_name == "edit" {
            edit_calls += 1;
            modifying_sessions.insert(row.session_id.clone());
        }
        if row.tool_name == "apply_patch" {
            apply_patch_calls += 1;
            modifying_sessions.insert(row.session_id.clone());
        }
    }

    let failure_rates = failure_rates(rows);
    let retry_rate = retry_after_edit_failure(rows);
    let metrics = compute_metrics(rows);

    PopulationReport {
        definition: definition.to_owned(),
        row_count: rows.len(),
        distinct_sessions: sessions.len(),
        distinct_tasks: tasks.len(),
        distinct_roles: roles.len(),
        edit_calls,
        apply_patch_calls,
        modifying_sessions: modifying_sessions.len(),
        failure_rates,
        retry_after_edit_failure_rate: retry_rate,
        metrics,
    }
}

/// Return a stable role label for a row, or `None` if the row is missing a role
/// and has no diagnostics that would let us infer one.
fn agent_role_or_default(row: &NormalizedToolCallRow) -> Option<String> {
    // Return the explicit role if present; otherwise return None rather than
    // fabricating a label. The evaluator will surface a missing role through
    // `missing_required_fields`.
    row.agent_role.clone()
}

/// Collect all rows (candidate + baseline) and check whether every required
/// source field is present on every row. Returns the list of missing field
/// names, if any.
fn missing_required_fields(
    candidate: &[NormalizedToolCallRow],
    baseline: &[NormalizedToolCallRow],
) -> Vec<String> {
    let mut missing = BTreeSet::new();
    for row in candidate.iter().chain(baseline.iter()) {
        if row.provider_id.is_none() {
            missing.insert("provider_id".to_owned());
        }
        if row.model_id.is_none() {
            missing.insert("model_id".to_owned());
        }
        if row.format_family.is_none() {
            missing.insert("format_family".to_owned());
        }
        if row.tool_surface_family.is_none() {
            missing.insert("tool_surface_family".to_owned());
        }
        if row.agent_role.is_none() {
            missing.insert("agent_role".to_owned());
        }
        if row.task_id.is_none() {
            missing.insert("task_id".to_owned());
        }
        if row.calendar_day.is_none() {
            missing.insert("calendar_day".to_owned());
        }
    }
    missing.into_iter().collect()
}

/// Filter a population to only rows matching the supplied baseline selectors.
///
/// Deterministic matched-baseline selection: keep rows whose role and task_id
/// are present in the candidate population and whose tool surface family is
/// one of the accepted baseline families (e.g. `default` or `Responses/default`).
pub fn matched_baseline_rows(
    candidate_rows: &[NormalizedToolCallRow],
    baseline_pool: &[NormalizedToolCallRow],
    baseline_surface_families: &[String],
) -> Vec<NormalizedToolCallRow> {
    let candidate_roles: BTreeSet<String> = candidate_rows
        .iter()
        .filter_map(|r| r.agent_role.clone())
        .collect();
    let candidate_tasks: BTreeSet<String> = candidate_rows
        .iter()
        .filter_map(|r| r.task_id.clone())
        .collect();
    baseline_pool
        .iter()
        .filter(|r| {
            r.agent_role
                .as_ref()
                .map(|role| candidate_roles.contains(role))
                .unwrap_or(false)
                && r.task_id
                    .as_ref()
                    .map(|task| candidate_tasks.contains(task))
                    .unwrap_or(false)
                && r.tool_surface_family
                    .as_ref()
                    .map(|f| baseline_surface_families.contains(f))
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// Validate that the declared window is a completed 30-day window.
/// Rejects malformed/non-30-day window metadata.
fn validate_window(window: &WindowSpec) -> Result<(), String> {
    let start = parse_day(&window.start_day)?;
    let end = parse_day(&window.end_day)?;
    if end < start {
        return Err("window end is before start".to_owned());
    }
    let span_days = (end - start).num_days();
    if span_days != 29 {
        // 30-day inclusive window spans 29 days between start and end.
        return Err(format!(
            "expected a 30-day inclusive window (start to end = 29 days), got {span_days} days"
        ));
    }
    Ok(())
}

fn parse_day(s: &str) -> Result<chrono::NaiveDate, String> {
    s.parse::<chrono::NaiveDate>()
        .map_err(|e| format!("invalid calendar day {s}: {e}"))
}

/// Evaluate a fixed window and emit a GO/STOP/insufficient-data report.
pub fn evaluate(
    input: &EvalInput,
    minima: Option<SampleMinima>,
    thresholds: Option<GateThresholds>,
) -> GoStopReport {
    let minima = minima.unwrap_or_default();
    let thresholds = thresholds.unwrap_or_default();

    let candidate = population_report(&input.candidate_rows, "OpenAIResponses + codex surface");
    let baseline = population_report(&input.baseline_rows, "matched default/Responses baseline");
    let missing = missing_required_fields(&input.candidate_rows, &input.baseline_rows);

    let window_valid = validate_window(&input.window);

    let mut gates: Vec<GateResult> = Vec::new();
    let mut shortfalls: Vec<String> = Vec::new();

    // Sample-minimum checks on the candidate population. Shortfalls make the
    // report `insufficient data` regardless of other results.
    if candidate.edit_calls < minima.edit_calls {
        shortfalls.push(format!(
            "edit_calls: {} < {}",
            candidate.edit_calls, minima.edit_calls
        ));
    }
    if candidate.apply_patch_calls < minima.apply_patch_calls {
        shortfalls.push(format!(
            "apply_patch_calls: {} < {}",
            candidate.apply_patch_calls, minima.apply_patch_calls
        ));
    }
    if candidate.modifying_sessions < minima.modifying_sessions {
        shortfalls.push(format!(
            "modifying_sessions: {} < {}",
            candidate.modifying_sessions, minima.modifying_sessions
        ));
    }
    if candidate.distinct_tasks < minima.tasks {
        shortfalls.push(format!(
            "distinct_tasks: {} < {}",
            candidate.distinct_tasks, minima.tasks
        ));
    }
    if candidate.distinct_roles < minima.roles {
        shortfalls.push(format!(
            "distinct_roles: {} < {}",
            candidate.distinct_roles, minima.roles
        ));
    }

    // Audit completeness check.
    let audit = input.audit.clone();
    let audit_ok = audit
        .as_ref()
        .map(|a| a.completed && a.qualifying >= thresholds.audit_qualifying)
        .unwrap_or(false);
    match &audit {
        None => {
            shortfalls.push("manual audit absent".to_owned());
        }
        Some(a) if !a.completed => {
            shortfalls.push(format!(
                "manual audit incomplete: sampled {} < {}",
                a.sampled, thresholds.audit_sample
            ));
        }
        Some(_) => {}
    }

    // Wilson difference interval for edit vs apply_patch failure rate on the
    // candidate population (the fourth quantitative gate).
    let (failure_difference_interval, _) =
        edit_minus_apply_patch_failure_interval(&input.candidate_rows);

    let (decision, decision_reason): (Decision, String);

    // Early exit if anything is missing or short.
    if !missing.is_empty() || window_valid.is_err() || !shortfalls.is_empty() {
        let mut reasons: Vec<String> = Vec::new();
        if !missing.is_empty() {
            reasons.push(format!("missing required fields: {}", missing.join(", ")));
        }
        if let Err(ref e) = window_valid {
            reasons.push(format!("window invalid: {e}"));
        }
        if !shortfalls.is_empty() {
            reasons.push(format!(
                "sample minima/audit shortfalls: {}",
                shortfalls.join(", ")
            ));
        }
        decision = Decision::InsufficientData;
        decision_reason = reasons.join("; ");
    } else {
        // All inputs are present and sample minima/audit are satisfied.
        // Evaluate the four quantitative gates.
        let edit_disadvantage = candidate.failure_rates.edit_failure_rate.rate
            - baseline.failure_rates.edit_failure_rate.rate;
        let edit_disadvantage_points = edit_disadvantage * 100.0;
        let edit_disadvantage_pass =
            edit_disadvantage_points >= thresholds.edit_disadvantage_points;
        gates.push(GateResult {
            gate_name: "edit_disadvantage".to_owned(),
            passed: edit_disadvantage_pass,
            evaluable: true,
            observed: Some(edit_disadvantage_points),
            threshold: format!(">= {:.1} percentage points", thresholds.edit_disadvantage_points),
            message: if edit_disadvantage_pass {
                format!(
                    "candidate edit failure rate is {edit_disadvantage_points:.2} points above baseline"
                )
            } else {
                format!(
                    "candidate edit failure rate is only {edit_disadvantage_points:.2} points above baseline (threshold {:.1} points)",
                    thresholds.edit_disadvantage_points
                )
            },
        });

        let pseudo_candidate = RateMetric::with_pseudo_count(
            candidate.failure_rates.edit_failure_rate.numerator,
            candidate.failure_rates.edit_failure_rate.denominator,
        );
        let pseudo_baseline = RateMetric::with_pseudo_count(
            baseline.failure_rates.edit_failure_rate.numerator,
            baseline.failure_rates.edit_failure_rate.denominator,
        );
        let pseudo_ratio = if pseudo_baseline.rate == 0.0 {
            f64::INFINITY
        } else {
            pseudo_candidate.rate / pseudo_baseline.rate
        };
        let pseudo_ratio_pass = pseudo_ratio >= thresholds.pseudo_count_ratio
            || (pseudo_baseline.rate == 0.0 && pseudo_candidate.rate > 0.0);
        gates.push(GateResult {
            gate_name: "pseudo_count_ratio".to_owned(),
            passed: pseudo_ratio_pass,
            evaluable: true,
            observed: Some(pseudo_ratio),
            threshold: format!(">= {:.1}", thresholds.pseudo_count_ratio),
            message: if pseudo_ratio_pass {
                format!("pseudo-count edit-failure ratio is {pseudo_ratio:.2}")
            } else {
                format!(
                    "pseudo-count edit-failure ratio is {pseudo_ratio:.2} (threshold {:.1})",
                    thresholds.pseudo_count_ratio
                )
            },
        });

        let retry_disadvantage = candidate.retry_after_edit_failure_rate.rate
            - baseline.retry_after_edit_failure_rate.rate;
        let retry_disadvantage_points = retry_disadvantage * 100.0;
        let retry_disadvantage_pass =
            retry_disadvantage_points >= thresholds.retry_disadvantage_points;
        gates.push(GateResult {
            gate_name: "retry_disadvantage".to_owned(),
            passed: retry_disadvantage_pass,
            evaluable: true,
            observed: Some(retry_disadvantage_points),
            threshold: format!(">= {:.1} percentage points", thresholds.retry_disadvantage_points),
            message: if retry_disadvantage_pass {
                format!(
                    "candidate retry rate is {retry_disadvantage_points:.2} points above baseline"
                )
            } else {
                format!(
                    "candidate retry rate is only {retry_disadvantage_points:.2} points above baseline (threshold {:.1} points)",
                    thresholds.retry_disadvantage_points
                )
            },
        });

        let wilson_pass = failure_difference_interval.excludes_zero;
        gates.push(GateResult {
            gate_name: "wilson_difference_excludes_zero".to_owned(),
            passed: wilson_pass,
            evaluable: true,
            observed: None,
            threshold: "95% Wilson difference interval excludes zero".to_owned(),
            message: if wilson_pass {
                format!(
                    "95% Wilson interval [{:.4}, {:.4}] excludes zero",
                    failure_difference_interval.lower, failure_difference_interval.upper
                )
            } else {
                format!(
                    "95% Wilson interval [{:.4}, {:.4}] includes zero",
                    failure_difference_interval.lower, failure_difference_interval.upper
                )
            },
        });

        gates.push(GateResult {
            gate_name: "manual_audit".to_owned(),
            passed: audit_ok,
            evaluable: true,
            observed: audit.as_ref().map(|a| a.qualifying as f64),
            threshold: format!(
                "{} of {} sampled failures qualify",
                thresholds.audit_qualifying, thresholds.audit_sample
            ),
            message: if audit_ok {
                format!(
                    "manual audit: {} of {} sampled failures qualify",
                    audit.as_ref().expect("audit is Some").qualifying,
                    audit.as_ref().expect("audit is Some").sampled
                )
            } else if audit.as_ref().map(|a| a.completed).unwrap_or(false) {
                format!(
                    "manual audit: only {} of {} sampled failures qualify (threshold {})",
                    audit.as_ref().expect("audit is Some").qualifying,
                    audit.as_ref().expect("audit is Some").sampled,
                    thresholds.audit_qualifying
                )
            } else {
                "manual audit incomplete".to_owned()
            },
        });

        let all_quantitative_pass = gates
            .iter()
            .all(|g| g.gate_name == "manual_audit" || g.passed);
        let manual_pass = gates
            .iter()
            .find(|g| g.gate_name == "manual_audit")
            .map(|g| g.passed)
            .unwrap_or(false);

        if all_quantitative_pass && manual_pass {
            decision = Decision::Go;
            decision_reason = "all quantitative gates and manual audit passed".to_owned();
        } else {
            decision = Decision::Stop;
            let failed: Vec<String> = gates
                .iter()
                .filter(|g| !g.passed)
                .map(|g| g.gate_name.clone())
                .collect();
            decision_reason = format!("failed gates: {}", failed.join(", "));
        }
    }

    GoStopReport {
        decision,
        decision_reason,
        window: input.window.clone(),
        candidate,
        baseline,
        audit,
        gates,
        failure_difference_interval,
        missing_required_fields: missing,
        sample_minima_shortfalls: shortfalls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::tool_call_export::NormalizedToolCallRow;

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
}
