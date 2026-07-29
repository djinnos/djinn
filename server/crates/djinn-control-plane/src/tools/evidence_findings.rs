//! Closed V1 finalization for frozen refinement-evidence investigations.
//!
//! The request deliberately contains anchors, not caller-owned command results.
//! Command provenance and health are hydrated from the immutable invocation ledger
//! while the projection is inserted in the same transaction as validation.

use std::collections::{HashMap, HashSet};

use djinn_core::models::{EvidenceCommandInvocation, EvidenceFinalizedProjection};
use djinn_db::{EvidenceRepository, InsertEvidenceFinalizedProjection};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use super::evidence_command::{
    EvidenceCommandHealth, EvidenceCommandInvocationSelection, derive_command_health,
};
use super::evidence_plan::{
    EvidencePlanError, EvidencePlanIdentity, EvidenceTerminalResult, reconcile_terminal_results,
};

/// Recursively closed caller input for the structured completion protocol.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCompletionV1 {
    pub schema_version: u8,
    pub plan_id: String,
    pub terminal_results: Vec<EvidenceTerminalResult>,
    pub findings: Vec<EvidenceFindingV1>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFindingV1 {
    pub check_id: String,
    pub summary: String,
    pub anchor: EvidenceAnchorV1,
}

/// Exactly one tagged, method-compatible anchor is required for each finding.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceAnchorV1 {
    Code {
        path: String,
        start_line: u32,
        end_line: u32,
        captured_commit_sha: String,
    },
    Graph {
        node_id: String,
        graph_identity: String,
    },
    Command {
        invocation: EvidenceCommandInvocationSelection,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCompletionOutcome {
    Resolved,
    Partial,
    Unresolved,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvidenceCompletionProjectionV1 {
    pub schema_version: u8,
    pub plan_id: String,
    pub captured_commit_sha: String,
    pub worktree_fingerprint: String,
    pub outcome: EvidenceCompletionOutcome,
    pub checks: Vec<HydratedEvidenceCheckV1>,
    pub findings: Vec<HydratedEvidenceFindingV1>,
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HydratedEvidenceCheckV1 {
    pub check_id: String,
    pub question: String,
    pub method: String,
    pub terminal: bool,
    pub health: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HydratedEvidenceFindingV1 {
    pub check_id: String,
    pub summary: String,
    pub anchor: HydratedEvidenceAnchorV1,
}

// Flat Command fields preserve the frozen serialized EvidenceCompletionV1
// projection shape; this enum is serialized only at hand-off.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HydratedEvidenceAnchorV1 {
    Code {
        path: String,
        start_line: u32,
        end_line: u32,
        captured_commit_sha: String,
    },
    Graph {
        node_id: String,
        graph_identity: String,
    },
    Command {
        invocation_id: String,
        argv: Vec<String>,
        canonical_cwd: String,
        launch_state: String,
        process_state: String,
        launched_at: Option<String>,
        finished_at: Option<String>,
        exit_code: Option<i32>,
        signal: Option<i32>,
        runner_failure: Option<String>,
        elapsed_millis: Option<i64>,
        timeout_millis: Option<i64>,
        timed_out: bool,
        stdout_digest: Option<String>,
        stdout_excerpt: Option<String>,
        stdout_truncated: bool,
        stderr_digest: Option<String>,
        stderr_excerpt: Option<String>,
        stderr_truncated: bool,
        health: EvidenceCommandHealth,
    },
}

#[derive(Debug)]
pub enum EvidenceCompletionError {
    Plan(EvidencePlanError),
    Invalid(&'static str),
    UnknownCheck(String),
    MethodMismatch(String),
    InvalidAnchor(String),
    UnknownInvocation(String),
    InvocationCheckMismatch(String),
    AlreadyFinalized,
    Persistence(String),
}
impl std::fmt::Display for EvidenceCompletionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(e) => e.fmt(f),
            Self::Invalid(s) => write!(f, "invalid evidence completion: {s}"),
            Self::UnknownCheck(s) => write!(f, "unknown check '{s}'"),
            Self::MethodMismatch(s) => write!(f, "anchor method does not match check '{s}'"),
            Self::InvalidAnchor(s) => write!(f, "invalid anchor: {s}"),
            Self::UnknownInvocation(s) => write!(f, "unknown invocation '{s}'"),
            Self::InvocationCheckMismatch(s) => {
                write!(f, "invocation does not belong to check '{s}'")
            }
            Self::AlreadyFinalized => write!(f, "evidence plan already finalized"),
            Self::Persistence(s) => write!(f, "evidence finalization failed: {s}"),
        }
    }
}
impl std::error::Error for EvidenceCompletionError {}

/// Validate, hydrate, and persist V1 exactly once. All reads and the projection
/// insert share one transaction, so rejection cannot leave a partial projection.
pub async fn finalize_evidence_completion_v1(
    repository: &EvidenceRepository,
    identity: &EvidencePlanIdentity,
    request: EvidenceCompletionV1,
) -> Result<EvidenceFinalizedProjection, EvidenceCompletionError> {
    validate_shape(&request)?;
    repository
        .db()
        .ensure_initialized()
        .await
        .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?;
    let mut tx = repository
        .db()
        .pool()
        .begin()
        .await
        .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?;
    let hydration = EvidenceRepository::hydrate_by_identity_in_transaction(
        &mut tx,
        &identity.spike_task_id,
        &identity.session_id,
    )
    .await
    .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?
    .ok_or(EvidenceCompletionError::Plan(
        EvidencePlanError::NoFrozenPlan,
    ))?;
    if hydration.plan.captured_commit_sha != identity.captured_commit_sha
        || hydration.plan.worktree_fingerprint != identity.worktree_fingerprint
    {
        return Err(EvidenceCompletionError::Plan(
            EvidencePlanError::NoFrozenPlan,
        ));
    }
    if hydration.finalized_projection.is_some() {
        return Err(EvidenceCompletionError::AlreadyFinalized);
    }
    if request.plan_id.trim() != hydration.plan.id {
        return Err(EvidenceCompletionError::Invalid(
            "plan_id does not match the frozen plan",
        ));
    }
    reconcile_terminal_results(&hydration.plan, &request.terminal_results)
        .map_err(EvidenceCompletionError::Plan)?;
    let projection = build_projection(
        &hydration.plan.id,
        &hydration.plan.captured_commit_sha,
        &hydration.plan.worktree_fingerprint,
        &hydration.plan.checks,
        &hydration.invocations,
        &request,
    )?;
    let payload = serde_json::to_value(&projection)
        .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?;
    let stored = EvidenceRepository::insert_finalized_projection_in_transaction(
        &mut tx,
        InsertEvidenceFinalizedProjection {
            id: uuid::Uuid::now_v7().to_string(),
            plan_id: hydration.plan.id,
            version: 1,
            payload,
        },
    )
    .await
    .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?;
    Ok(stored)
}

/// Validate, hydrate, and insert V1 using a caller-owned transaction.
/// This companion deliberately does not commit, allowing the refinement hand-off
/// to persist its legacy debate record in the same transaction.
pub async fn finalize_evidence_completion_v1_in_transaction(
    _repository: &EvidenceRepository,
    tx: &mut Transaction<'_, Postgres>,
    identity: &EvidencePlanIdentity,
    request: EvidenceCompletionV1,
) -> Result<EvidenceFinalizedProjection, EvidenceCompletionError> {
    validate_shape(&request)?;
    let hydration = EvidenceRepository::hydrate_by_identity_in_transaction(
        tx,
        &identity.spike_task_id,
        &identity.session_id,
    )
    .await
    .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?
    .ok_or(EvidenceCompletionError::Plan(
        EvidencePlanError::NoFrozenPlan,
    ))?;
    if hydration.plan.captured_commit_sha != identity.captured_commit_sha
        || hydration.plan.worktree_fingerprint != identity.worktree_fingerprint
    {
        return Err(EvidenceCompletionError::Plan(
            EvidencePlanError::NoFrozenPlan,
        ));
    }
    if hydration.finalized_projection.is_some() {
        return Err(EvidenceCompletionError::AlreadyFinalized);
    }
    if request.plan_id.trim() != hydration.plan.id {
        return Err(EvidenceCompletionError::Invalid(
            "plan_id does not match the frozen plan",
        ));
    }
    reconcile_terminal_results(&hydration.plan, &request.terminal_results)
        .map_err(EvidenceCompletionError::Plan)?;
    let projection = build_projection(
        &hydration.plan.id,
        &hydration.plan.captured_commit_sha,
        &hydration.plan.worktree_fingerprint,
        &hydration.plan.checks,
        &hydration.invocations,
        &request,
    )?;
    let payload = serde_json::to_value(&projection)
        .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))?;
    EvidenceRepository::insert_finalized_projection_in_transaction(
        tx,
        InsertEvidenceFinalizedProjection {
            id: uuid::Uuid::now_v7().to_string(),
            plan_id: hydration.plan.id,
            version: 1,
            payload,
        },
    )
    .await
    .map_err(|e| EvidenceCompletionError::Persistence(e.to_string()))
}

fn validate_shape(request: &EvidenceCompletionV1) -> Result<(), EvidenceCompletionError> {
    if request.schema_version != 1 {
        return Err(EvidenceCompletionError::Invalid(
            "schema_version must be literal 1",
        ));
    }
    if request.plan_id.trim().is_empty() {
        return Err(EvidenceCompletionError::Invalid("plan_id must be nonempty"));
    }
    let mut seen = HashSet::new();
    for finding in &request.findings {
        if finding.check_id.trim().is_empty() || finding.summary.trim().is_empty() {
            return Err(EvidenceCompletionError::Invalid(
                "findings require a check id and nonempty summary",
            ));
        }
        if !seen.insert((finding.check_id.trim(), finding.summary.trim())) {
            return Err(EvidenceCompletionError::Invalid("duplicate finding"));
        }
    }
    Ok(())
}

fn build_projection(
    plan_id: &str,
    commit: &str,
    worktree: &str,
    checks: &[djinn_core::models::EvidencePlanCheck],
    invocations: &[EvidenceCommandInvocation],
    request: &EvidenceCompletionV1,
) -> Result<EvidenceCompletionProjectionV1, EvidenceCompletionError> {
    let planned: HashMap<&str, &djinn_core::models::EvidencePlanCheck> =
        checks.iter().map(|c| (c.check_id.as_str(), c)).collect();
    let mut hydrated_findings = Vec::with_capacity(request.findings.len());
    let mut check_health: HashMap<&str, String> = checks
        .iter()
        .map(|c| {
            (
                c.check_id.as_str(),
                if c.method == "command" {
                    "not_run".to_string()
                } else {
                    "ok".to_string()
                },
            )
        })
        .collect();
    for finding in &request.findings {
        let check = planned
            .get(finding.check_id.trim())
            .ok_or_else(|| EvidenceCompletionError::UnknownCheck(finding.check_id.clone()))?;
        let anchor = match &finding.anchor {
            EvidenceAnchorV1::Code {
                path,
                start_line,
                end_line,
                captured_commit_sha,
            } => {
                if check.method != "code" {
                    return Err(EvidenceCompletionError::MethodMismatch(
                        finding.check_id.clone(),
                    ));
                }
                if path.trim().is_empty()
                    || *start_line == 0
                    || end_line < start_line
                    || captured_commit_sha != commit
                {
                    return Err(EvidenceCompletionError::InvalidAnchor(
                        "code anchor must name a valid captured-commit line range".into(),
                    ));
                }
                HydratedEvidenceAnchorV1::Code {
                    path: path.clone(),
                    start_line: *start_line,
                    end_line: *end_line,
                    captured_commit_sha: captured_commit_sha.clone(),
                }
            }
            EvidenceAnchorV1::Graph {
                node_id,
                graph_identity,
            } => {
                if check.method != "graph" {
                    return Err(EvidenceCompletionError::MethodMismatch(
                        finding.check_id.clone(),
                    ));
                }
                if node_id.trim().is_empty()
                    || graph_identity.trim().is_empty()
                    || graph_identity != commit
                {
                    return Err(EvidenceCompletionError::InvalidAnchor(
                        "graph anchor must use captured graph identity".into(),
                    ));
                }
                HydratedEvidenceAnchorV1::Graph {
                    node_id: node_id.clone(),
                    graph_identity: graph_identity.clone(),
                }
            }
            EvidenceAnchorV1::Command { invocation } => {
                if check.method != "command" {
                    return Err(EvidenceCompletionError::MethodMismatch(
                        finding.check_id.clone(),
                    ));
                }
                let selected = invocations
                    .iter()
                    .find(|i| i.id == invocation.invocation_id.trim())
                    .ok_or_else(|| {
                        EvidenceCompletionError::UnknownInvocation(invocation.invocation_id.clone())
                    })?;
                if selected.check_id != check.check_id {
                    return Err(EvidenceCompletionError::InvocationCheckMismatch(
                        check.check_id.clone(),
                    ));
                }
                if selected.plan_id != plan_id
                    || selected.captured_commit_sha != commit
                    || selected.worktree_fingerprint != worktree
                {
                    return Err(EvidenceCompletionError::InvalidAnchor(
                        "command identity differs from frozen plan".into(),
                    ));
                }
                let health = derive_command_health(selected);
                check_health.insert(
                    check.check_id.as_str(),
                    format!("{:?}", health).to_lowercase(),
                );
                HydratedEvidenceAnchorV1::Command {
                    invocation_id: selected.id.clone(),
                    argv: selected.argv.clone(),
                    canonical_cwd: selected.canonical_cwd.clone(),
                    launch_state: selected.launch_state.clone(),
                    process_state: selected.process_state.clone(),
                    launched_at: selected.launched_at.clone(),
                    finished_at: selected.finished_at.clone(),
                    exit_code: selected.exit_code,
                    signal: selected.signal,
                    runner_failure: selected.runner_failure.clone(),
                    elapsed_millis: selected.elapsed_millis,
                    timeout_millis: selected.timeout_millis,
                    timed_out: selected.timed_out,
                    stdout_digest: selected.stdout_digest.clone(),
                    stdout_excerpt: selected.stdout_excerpt.clone(),
                    stdout_truncated: selected.stdout_truncated,
                    stderr_digest: selected.stderr_digest.clone(),
                    stderr_excerpt: selected.stderr_excerpt.clone(),
                    stderr_truncated: selected.stderr_truncated,
                    health,
                }
            }
        };
        hydrated_findings.push(HydratedEvidenceFindingV1 {
            check_id: finding.check_id.trim().to_owned(),
            summary: finding.summary.trim().to_owned(),
            anchor,
        });
    }
    let all_ok = checks.iter().all(|c| {
        check_health
            .get(c.check_id.as_str())
            .is_some_and(|h| h == "ok")
    });
    let outcome = if hydrated_findings.is_empty() {
        EvidenceCompletionOutcome::Unresolved
    } else if all_ok {
        EvidenceCompletionOutcome::Resolved
    } else {
        EvidenceCompletionOutcome::Partial
    };
    let gaps = checks
        .iter()
        .filter_map(|c| {
            let health = check_health.get(c.check_id.as_str())?;
            (health != "ok").then(|| format!("{}: {health}", c.check_id))
        })
        .collect();
    Ok(EvidenceCompletionProjectionV1 {
        schema_version: 1,
        plan_id: plan_id.to_owned(),
        captured_commit_sha: commit.to_owned(),
        worktree_fingerprint: worktree.to_owned(),
        outcome,
        checks: checks
            .iter()
            .map(|c| HydratedEvidenceCheckV1 {
                check_id: c.check_id.clone(),
                question: c.question.clone(),
                method: c.method.clone(),
                terminal: true,
                health: check_health
                    .remove(c.check_id.as_str())
                    .unwrap_or_else(|| "not_run".into()),
            })
            .collect(),
        findings: hydrated_findings,
        gaps,
    })
}

/// Render a complete, deliberately non-positive Judge hand-off from persisted V1.
pub fn render_evidence_judge_projection(
    payload: &serde_json::Value,
) -> Result<String, EvidenceCompletionError> {
    let object = payload.as_object().ok_or(EvidenceCompletionError::Invalid(
        "stored projection is not V1",
    ))?;
    if object
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Err(EvidenceCompletionError::Invalid(
            "stored projection is not V1",
        ));
    }
    let outcome = object
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection is not V1",
        ))?;
    let plan_id = object
        .get("plan_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection lacks plan identity",
        ))?;
    let captured_commit_sha = object
        .get("captured_commit_sha")
        .and_then(serde_json::Value::as_str)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection lacks commit identity",
        ))?;
    let worktree_fingerprint = object
        .get("worktree_fingerprint")
        .and_then(serde_json::Value::as_str)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection lacks worktree identity",
        ))?;
    let mut out = format!(
        "Evidence completion: {outcome}\nplan_id={plan_id} captured_commit_sha={captured_commit_sha} worktree_fingerprint={worktree_fingerprint}\n"
    );
    if outcome == "unresolved" {
        out.push_str("No anchored findings were established; this is not positive evidence.\n");
    }
    for check in object
        .get("checks")
        .and_then(serde_json::Value::as_array)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection lacks checks",
        ))?
    {
        out.push_str(&format!(
            "check {} [{}] terminal={} health={}\n",
            check
                .get("check_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
            check
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
            check
                .get("terminal")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            check
                .get("health")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
        ));
    }
    for gap in object
        .get("gaps")
        .and_then(serde_json::Value::as_array)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection lacks gaps",
        ))?
    {
        out.push_str(&format!("gap: {}\n", gap.as_str().unwrap_or("?")));
    }
    for finding in object
        .get("findings")
        .and_then(serde_json::Value::as_array)
        .ok_or(EvidenceCompletionError::Invalid(
            "stored projection lacks findings",
        ))?
    {
        out.push_str(&format!(
            "finding {}: {} anchor={}\n",
            finding
                .get("check_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
            finding
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
            finding
                .get("anchor")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        ));
    }
    Ok(out)
}
