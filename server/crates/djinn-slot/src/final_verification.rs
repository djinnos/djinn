//! Authoritative final-verification recording coordinator.
//!
//! This module deliberately owns no fingerprinting, identity derivation, or
//! sandbox policy. Those correctness boundaries live in `djinn-sandbox`; this
//! coordinator owns the one lease, one execution, and one possible durable row.

use std::future::Future;
use std::pin::Pin;

use djinn_core::canonical_verify::{
    CurrentEnvironmentIdentity, EnvironmentIdentityV1, FreshnessCompatibilityInput,
    evaluate_freshness,
};
use djinn_core::models::VerifySource;
use djinn_db::repositories::verify_run::{
    RecordEligibleFinalVerificationPassParams, RequiredFinalVerificationCommand,
    VerifyRunRepository,
};
use djinn_git::{VerificationInputFingerprint, compute_verification_input_fingerprint_with_config};
use djinn_sandbox::final_verification_execution::{
    FinalVerificationExecutionEvidence, FinalVerificationExecutionRequest,
    execute_final_verification,
};
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;
use crate::output_parser::{CompletionIntent, FinalVerificationDisposition};

/// Material the host resolves from the current canonical plan, manifest, and
/// environment. It is intentionally an execution request rather than a second
/// copy of the identity/fingerprint contract.
#[derive(Clone)]
pub struct FinalVerificationResolvedMaterial {
    pub execution_request: FinalVerificationExecutionRequest,
    pub verify_source: VerifySource,
    /// Required check IDs from the same canonical plan used by the request.
    pub required_checks: Vec<String>,
    /// Legacy audit fingerprint. It is not used for final-verification reuse.
    pub diff_fingerprint: String,
}

/// Complete proof carried for a reusable pass, rather than an opaque cache hit.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FinalVerificationSuccessEvidence {
    pub persisted_run_id: String,
    pub completed_at: String,
    pub ordered_commands: serde_json::Value,
    pub covered_checks: serde_json::Value,
    pub required_checks: Vec<String>,
    pub verification_input_fingerprint: String,
    pub manifest_version: String,
    pub environment_identity_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CurrentVerificationInputs {
    fingerprint: String,
    identity: CurrentEnvironmentIdentity,
    manifest_version: String,
}

/// The normal invocation lease. The coordinator explicitly releases it before
/// every return, including ineligible, insert-error, and cancellation paths.
pub trait FinalVerificationInvocationLease: Send {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

#[derive(Clone, Debug)]
pub struct FinalVerificationCoordinatorRequest {
    pub task_id: String,
    pub task_run_id: String,
    pub cancellation: CancellationToken,
}

/// Named checkpoints in the production reuse consultation. Tests inject at
/// these checkpoints to prove uncertainty falls through to the normal writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalVerificationConsultationFailure {
    Lookup,
    Evaluator,
    Context,
    Fingerprint,
    Identity,
    Database,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalVerificationRecordingOutcome {
    Stored {
        verification_attempt_id: String,
        verify_run_id: String,
        evidence: Box<FinalVerificationSuccessEvidence>,
    },
    Reused {
        verification_attempt_id: String,
        evidence: Box<FinalVerificationSuccessEvidence>,
    },
    Ineligible {
        verification_attempt_id: String,
        reason: String,
    },
    Error {
        verification_attempt_id: String,
        detail: String,
    },
    /// The host resolved a typed "no final-verification plan is configured"
    /// signal (`Ok(None)` from `resolve_final_verification`): the project
    /// declares zero canonical commands. No lease is acquired, no command is
    /// executed, no evidence is fabricated, and no row is written; the
    /// completion-intent path treats this as "proceed without evidence"
    /// (the pre-completion-boundary submission behavior). A configured plan
    /// never reaches this variant — its failures stay `Ineligible`/`Error`.
    NotConfigured { verification_attempt_id: String },
}

/// Run the one authoritative completion boundary for either a model tool call
/// or a lifecycle-generated auto-submit settlement.
///
/// `submit_tool_label` names the finalize tool that initiated verification
/// (e.g. `"submit_work"` for a worker or `"submit_review"` for a reviewer) so
/// error messages reference the correct request without hard-coding a role.
///
/// Returns `Ok(Some(evidence))` for a stored/reused pass, `Ok(None)` when the
/// project has no final-verification plan configured (the typed
/// [`FinalVerificationRecordingOutcome::NotConfigured`] skip — the submission
/// proceeds without evidence, as it did before the completion boundary
/// existed), and `Err` for every ineligible/error outcome of a configured
/// plan (submission stays blocked, fail closed).
pub(crate) async fn verify_completion_intent(
    intent: &mut CompletionIntent,
    task_id: &str,
    task_run_id: Option<&str>,
    cancellation: CancellationToken,
    slot_ctx: &SlotContext,
    submit_tool_label: &str,
) -> Result<Option<FinalVerificationSuccessEvidence>, String> {
    // C1 made this typed decision before any reuse consultation. The legacy
    // path has no evidence by design, so inspecting `Option` here would reopen
    // the coordinator and emit a duplicate audit outcome.
    if intent.final_verification_disposition == FinalVerificationDisposition::NotConfigured {
        return Ok(None);
    }
    let task_run_id = match task_run_id {
        Some(task_run_id) => task_run_id.to_owned(),
        None => {
            let runs =
                djinn_db::repositories::task_run::TaskRunRepository::new(slot_ctx.db.clone())
                    .list_for_task(task_id)
                    .await
                    .map_err(|e| format!("could not resolve task run: {e}"))?;
            runs.into_iter()
                .find(|run| matches!(run.status.as_str(), "starting" | "running"))
                .map(|run| run.id)
                .ok_or_else(|| {
                    "no active task run is available for final verification".to_owned()
                })?
        }
    };
    match coordinate_final_verification(
        FinalVerificationCoordinatorRequest {
            task_id: task_id.to_owned(),
            task_run_id,
            cancellation,
        },
        slot_ctx,
    )
    .await
    {
        FinalVerificationRecordingOutcome::Stored { evidence, .. }
        | FinalVerificationRecordingOutcome::Reused { evidence, .. } => {
            let evidence = *evidence;
            intent.final_verification_evidence = Some(evidence.clone());
            intent.final_verification_disposition = FinalVerificationDisposition::Configured;
            Ok(Some(evidence))
        }
        FinalVerificationRecordingOutcome::NotConfigured { .. } => {
            // Typed skip: no plan is configured for this project, so the
            // completion boundary has nothing to enforce. Proceed with no
            // evidence — never synthesize a success record.
            intent.final_verification_evidence = None;
            intent.final_verification_disposition = FinalVerificationDisposition::NotConfigured;
            tracing::info!(
                task_id = %task_id,
                submit_tool = %submit_tool_label,
                "final verification skipped: no final-verification plan is configured; \
                 proceeding without evidence"
            );
            Ok(None)
        }
        FinalVerificationRecordingOutcome::Ineligible { reason, .. } => Err(format!(
            "Final verification rejected this {submit_tool_label} request: {reason}. Fix the worktree and resubmit."
        )),
        FinalVerificationRecordingOutcome::Error { detail, .. } => Err(format!(
            "Final verification could not complete: {detail}. Inspect the worktree and resubmit."
        )),
    }
}

/// Recompute C2 immediately before final consumption. C0/C1 protect command
/// suppression; this closes the interval before a submission is persisted.
pub(crate) async fn validate_or_reverify_completion_intent(
    intent: &mut CompletionIntent,
    task_id: &str,
    task_run_id: Option<&str>,
    cancellation: CancellationToken,
    slot_ctx: &SlotContext,
    submit_tool_label: &str,
) -> Result<Option<FinalVerificationSuccessEvidence>, String> {
    // C1 already made the typed legacy-path decision. Unlike a missing
    // evidence value, this is authoritative: C2 must carry it through without
    // resolving the plan again (and therefore without a second consultation
    // audit outcome.
    if intent.final_verification_disposition == FinalVerificationDisposition::NotConfigured {
        return Ok(None);
    }
    let task_run_id = match task_run_id {
        Some(id) => id.to_owned(),
        None => djinn_db::repositories::task_run::TaskRunRepository::new(slot_ctx.db.clone())
            .list_for_task(task_id)
            .await
            .map_err(|e| format!("could not resolve task run: {e}"))?
            .into_iter()
            .find(|run| matches!(run.status.as_str(), "starting" | "running"))
            .map(|run| run.id)
            .ok_or_else(|| "no active task run is available for final verification".to_owned())?,
    };
    if let Some(candidate) = intent.final_verification_evidence.clone() {
        let current = async {
            match slot_ctx
                .callbacks
                .resolve_final_verification(
                    task_id,
                    &task_run_id,
                    "completion-c2",
                    "completion-c2",
                    slot_ctx,
                )
                .await?
            {
                // An unconfigured plan cannot revalidate prior evidence; the
                // canonical path below resolves the typed skip.
                None => Ok::<_, String>(None),
                Some(material) => Ok(Some((derive_current_inputs(&material).await?, material))),
            }
        }
        .await;
        if let Ok(Some((inputs, material))) = current
            && candidate.verification_input_fingerprint == inputs.fingerprint
            && candidate.environment_identity_digest == inputs.identity.digest
            && candidate.manifest_version == inputs.manifest_version
            && candidate.required_checks == material.required_checks
            && coverage_equals_required(Some(&candidate.covered_checks), &material.required_checks)
        {
            return Ok(Some(candidate));
        }
        // Every mismatch/error discards evidence; only the canonical path below
        // may produce replacement evidence for the current inputs.
        intent.final_verification_evidence = None;
    }
    verify_completion_intent(
        intent,
        task_id,
        Some(&task_run_id),
        cancellation,
        slot_ctx,
        submit_tool_label,
    )
    .await
}

/// Resolve, lease, execute once, and conditionally record exactly one pass.
pub async fn coordinate_final_verification(
    request: FinalVerificationCoordinatorRequest,
    ctx: &SlotContext,
) -> FinalVerificationRecordingOutcome {
    let verification_attempt_id = uuid::Uuid::now_v7().to_string();
    let verify_run_id = uuid::Uuid::now_v7().to_string();

    // Cancellation is authoritative even when tests inject a typed outcome.
    if request.cancellation.is_cancelled() {
        return emit_ineligible(
            &request,
            &verification_attempt_id,
            "cancelled before resolution",
        );
    }

    // Keep production on the complete resolve/lease/execute/persist path while
    // allowing reply-loop tests to deterministically exercise the typed
    // coordinator boundary without a sandbox or durable verify-run fixture.
    #[cfg(any(test, feature = "test-support"))]
    if let Some(outcome) = ctx.callbacks.final_verification_outcome_for_test(&request) {
        return emit_outcome(&request, outcome);
    }
    // Resolve the plan's configured/unconfigured state BEFORE any settings
    // lookup, VerifyRun repository construction, C0 derivation, or cache
    // consultation. A zero-command plan produces exactly one bounded lookup
    // outcome `disabled`/`plan_unconfigured` and returns the typed legacy
    // skip; a configured plan continues to the consult-or-run boundary with
    // all fail-closed resolution/workspace semantics intact.
    let material = match ctx
        .callbacks
        .resolve_final_verification(
            &request.task_id,
            &request.task_run_id,
            &verification_attempt_id,
            &verify_run_id,
            ctx,
        )
        .await
    {
        Ok(Some(material)) => material,
        // Typed skip: the host resolved "no plan configured". Return before
        // any consultation, lease, execution, or persistence — there is
        // nothing to verify and nothing may be recorded. Emit exactly one
        // bounded lookup outcome so audit telemetry attributes the skip to
        // the unconfigured plan rather than to reuse being disabled or a
        // cache miss.
        Ok(None) => {
            emit_lookup_outcome(
                "disabled",
                &request,
                &verification_attempt_id,
                "plan_unconfigured",
                "",
            );
            ctx.callbacks
                .record_final_verification_consultation_outcome_for_test(
                    "disabled",
                    "plan_unconfigured",
                );
            return emit_outcome(
                &request,
                FinalVerificationRecordingOutcome::NotConfigured {
                    verification_attempt_id,
                },
            );
        }
        Err(detail) => return emit_error(&request, &verification_attempt_id, &detail),
    };
    // Consultation runs only for configured plans. The material resolved
    // above is reused for C0 derivation so no second plan resolution or
    // settings-gate round-trip is needed before the lookup.
    if let Some(evidence) =
        consult_reusable_final_verification(&request, &verification_attempt_id, &material, ctx)
            .await
    {
        return emit_outcome(
            &request,
            FinalVerificationRecordingOutcome::Reused {
                verification_attempt_id,
                evidence: Box::new(evidence),
            },
        );
    }
    if request.cancellation.is_cancelled() {
        return emit_ineligible(&request, &verification_attempt_id, "cancelled before lease");
    }
    let mut lease = match ctx
        .callbacks
        .acquire_final_verification_lease(
            &request.task_id,
            &request.task_run_id,
            &verification_attempt_id,
            ctx,
        )
        .await
    {
        Ok(lease) => lease,
        Err(detail) => return emit_error(&request, &verification_attempt_id, &detail),
    };

    let execution_result = if request.cancellation.is_cancelled() {
        Err(ineligible_outcome(
            &verification_attempt_id,
            "cancelled before execution",
        ))
    } else {
        // The delivered executor performs every descriptor in order and returns
        // evidence rather than persistence side effects. Tests inject evidence
        // here while retaining the real resolve/lease/validate/write boundary.
        #[cfg(any(test, feature = "test-support"))]
        let injected_evidence = ctx.callbacks.final_verification_evidence_for_test(&request);
        #[cfg(not(any(test, feature = "test-support")))]
        let injected_evidence: Option<FinalVerificationExecutionEvidence> = None;
        let evidence = match injected_evidence {
            Some(evidence) => evidence,
            None => execute_final_verification(material.execution_request.clone()).await,
        };
        if request.cancellation.is_cancelled() {
            Err(ineligible_outcome(
                &verification_attempt_id,
                "cancelled during execution",
            ))
        } else if !evidence.eligible() {
            Err(ineligible_outcome(
                &verification_attempt_id,
                &format_evidence_reason(&evidence),
            ))
        } else {
            Ok(evidence)
        }
    };

    // Releasing is deliberately before opening the independently committed
    // repository transaction. If it fails, no transaction has begun and so no
    // passing row can survive a failed invocation lease release.
    let outcome = match execution_result {
        Err(outcome) => release_then_return(&mut *lease, &verification_attempt_id, outcome).await,
        Ok(evidence) => {
            release_then_persist(
                &mut *lease,
                &request.cancellation,
                &verification_attempt_id,
                || {
                    persist_evidence(
                        &request,
                        &verification_attempt_id,
                        &verify_run_id,
                        &material,
                        &evidence,
                        ctx,
                    )
                },
            )
            .await
        }
    };
    emit_outcome(&request, outcome)
}

/// Resolve the project gate before constructing a verify-run repository. Every
/// miss, stale verdict, or error returns to the original writer path. The
/// plan's configured material is already resolved by the caller (a
/// zero-command plan never reaches this function), so C0 reuses it rather than
/// performing a second plan resolution.
async fn consult_reusable_final_verification(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    material: &FinalVerificationResolvedMaterial,
    ctx: &SlotContext,
) -> Option<FinalVerificationSuccessEvidence> {
    if injected_consultation_failure(
        ctx,
        FinalVerificationConsultationFailure::Context,
        request,
        attempt_id,
        "task_context",
    ) {
        return None;
    }
    let task = match ctx.load_task(&request.task_id).await {
        Ok(task) => task,
        Err(error) => return lookup_none("error", request, attempt_id, "task_context", &error),
    };
    let key = format!("project.{}.verify_run_reuse_enabled", task.project_id);
    if injected_consultation_failure(
        ctx,
        FinalVerificationConsultationFailure::Database,
        request,
        attempt_id,
        "gate_database",
    ) {
        return None;
    }
    let enabled = match djinn_db::repositories::settings::SettingsRepository::new(
        ctx.db.clone(),
        ctx.event_bus.clone(),
    )
    .get(&key)
    .await
    {
        Ok(Some(setting)) => matches!(setting.value.trim(), "true" | "1"),
        Ok(None) => false,
        Err(error) => return lookup_none("error", request, attempt_id, "gate", &error.to_string()),
    };
    if !enabled {
        return lookup_none("disabled", request, attempt_id, "default_off", "");
    }
    if injected_consultation_failure(
        ctx,
        FinalVerificationConsultationFailure::Identity,
        request,
        attempt_id,
        "c0_identity",
    ) || injected_consultation_failure(
        ctx,
        FinalVerificationConsultationFailure::Fingerprint,
        request,
        attempt_id,
        "c0_fingerprint",
    ) {
        return None;
    }
    let c0 = match derive_current_inputs(material).await {
        Ok(inputs) => inputs,
        Err(error) => return lookup_none("error", request, attempt_id, "c0", &error),
    };
    if injected_consultation_failure(
        ctx,
        FinalVerificationConsultationFailure::Lookup,
        request,
        attempt_id,
        "lookup",
    ) {
        return None;
    }
    let candidate = match VerifyRunRepository::new(ctx.db.clone())
        .latest_compatible_passing_final_verification(
            &request.task_id,
            &c0.fingerprint,
            &c0.manifest_version,
            &c0.identity.version,
            &material.required_checks,
        )
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return lookup_none("miss", request, attempt_id, "no_candidate", ""),
        Err(error) => {
            return lookup_none("error", request, attempt_id, "lookup", &error.to_string());
        }
    };
    let compatibility = FreshnessCompatibilityInput {
        verification_input_fingerprint: Some(c0.fingerprint.clone()),
        environment_identity: Some(c0.identity.clone()),
        manifest_version: Some(c0.manifest_version.clone()),
    };
    if injected_consultation_failure(
        ctx,
        FinalVerificationConsultationFailure::Evaluator,
        request,
        attempt_id,
        "evaluator",
    ) {
        return None;
    }
    if !evaluate_freshness(
        &material.diff_fingerprint,
        Some(&candidate),
        &[],
        &[],
        &material.required_checks,
        &compatibility,
    )
    .fresh
        || !coverage_equals_required(candidate.covered_checks.as_ref(), &material.required_checks)
    {
        return lookup_none("stale", request, attempt_id, "freshness_or_coverage", "");
    }
    // C1 is recomputed immediately before execution is suppressed, while no
    // invocation lease exists.
    let c1_material = match ctx
        .callbacks
        .resolve_final_verification(
            &request.task_id,
            &request.task_run_id,
            attempt_id,
            "reuse-c1",
            ctx,
        )
        .await
    {
        Ok(Some(material)) => material,
        Ok(None) => return lookup_none("miss", request, attempt_id, "c1_not_configured", ""),
        Err(error) => return lookup_none("error", request, attempt_id, "c1_resolution", &error),
    };
    let c1 = match derive_current_inputs(&c1_material).await {
        Ok(inputs) => inputs,
        Err(error) => return lookup_none("error", request, attempt_id, "c1", &error),
    };
    if c0 != c1
        || candidate.verification_input_fingerprint.as_deref() != Some(c1.fingerprint.as_str())
        || candidate.environment_identity_digest.as_deref() != Some(c1.identity.digest.as_str())
        || candidate.manifest_version.as_deref() != Some(c1.manifest_version.as_str())
        || !coverage_equals_required(
            candidate.covered_checks.as_ref(),
            &c1_material.required_checks,
        )
    {
        return lookup_none("stale", request, attempt_id, "c1_mismatch", "");
    }
    emit_lookup_outcome("hit", request, attempt_id, "verified_c1", "");
    Some(FinalVerificationSuccessEvidence {
        persisted_run_id: candidate.id,
        completed_at: candidate.completed_at,
        ordered_commands: candidate.ordered_commands.unwrap_or_default(),
        covered_checks: candidate.covered_checks.unwrap_or_default(),
        required_checks: c1_material.required_checks,
        verification_input_fingerprint: c1.fingerprint,
        manifest_version: c1.manifest_version,
        environment_identity_digest: c1.identity.digest,
    })
}

fn injected_consultation_failure(
    ctx: &SlotContext,
    failure: FinalVerificationConsultationFailure,
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &'static str,
) -> bool {
    if ctx
        .callbacks
        .inject_final_verification_consultation_failure_for_test(failure)
    {
        emit_lookup_outcome("error", request, attempt_id, reason, "injected");
        ctx.callbacks
            .record_final_verification_consultation_outcome_for_test("error", reason);
        true
    } else {
        false
    }
}

async fn derive_current_inputs(
    material: &FinalVerificationResolvedMaterial,
) -> Result<CurrentVerificationInputs, String> {
    let input = (material.execution_request.resolve_environment_identity)()
        .map_err(|error| error.to_string())?;
    let identity =
        EnvironmentIdentityV1::derive(input.clone()).map_err(|error| error.to_string())?;
    let fingerprint = match compute_verification_input_fingerprint_with_config(
        &material.execution_request.worktree,
        &material.execution_request.fingerprint_config,
    )
    .await
    .map_err(|error| error.to_string())?
    {
        VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        VerificationInputFingerprint::Unavailable(reason) => return Err(reason.to_string()),
    };
    Ok(CurrentVerificationInputs {
        fingerprint,
        identity: CurrentEnvironmentIdentity {
            version: "identity-v1".into(),
            digest: identity.digest,
        },
        manifest_version: format!("manifest-v{}", input.input_manifest.version),
    })
}

fn coverage_equals_required(coverage: Option<&serde_json::Value>, required: &[String]) -> bool {
    let Some(values) = coverage.and_then(serde_json::Value::as_array) else {
        return false;
    };
    let actual: std::collections::BTreeSet<_> = values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let expected: std::collections::BTreeSet<_> = required.iter().map(String::as_str).collect();
    actual == expected
}

fn lookup_none<T>(
    outcome: &'static str,
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &str,
    detail: &str,
) -> Option<T> {
    emit_lookup_outcome(outcome, request, attempt_id, reason, detail);
    None
}

fn emit_lookup_outcome(
    outcome: &'static str,
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &str,
    detail: &str,
) {
    // The metric has only the bounded outcome label. Rich identifiers and
    // diagnostics are emitted in the structured audit event below.
    djinn_telemetry::final_verification::increment_lookup(outcome);
    tracing::info!(verify_run_lookup_outcome = outcome, task_id = %request.task_id,
        task_run_id = %request.task_run_id, verification_attempt_id = %attempt_id,
        audit_reason = reason, audit_detail = detail, "final verification reuse consultation");
}

/// Release before any durable write. This is the commit protocol boundary: the
/// repository insert is permitted only after the normal invocation lease has
/// successfully released.
async fn release_then_return(
    lease: &mut dyn FinalVerificationInvocationLease,
    attempt_id: &str,
    outcome: FinalVerificationRecordingOutcome,
) -> FinalVerificationRecordingOutcome {
    match lease.release().await {
        Ok(()) => outcome,
        Err(detail) => error_outcome(attempt_id, &format!("lease release failed: {detail}")),
    }
}

/// Run the durable write only after a successful release and while observing
/// cancellation. `biased` gives cancellation precedence at the write boundary.
async fn release_then_persist<F, Fut>(
    lease: &mut dyn FinalVerificationInvocationLease,
    cancellation: &CancellationToken,
    attempt_id: &str,
    persist: F,
) -> FinalVerificationRecordingOutcome
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = FinalVerificationRecordingOutcome>,
{
    match lease.release().await {
        Err(detail) => error_outcome(attempt_id, &format!("lease release failed: {detail}")),
        Ok(()) if cancellation.is_cancelled() => {
            ineligible_outcome(attempt_id, "cancelled before persistence")
        }
        Ok(()) => tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                ineligible_outcome(attempt_id, "cancelled during persistence")
            }
            outcome = persist() => outcome,
        },
    }
}

async fn persist_evidence(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    verify_run_id: &str,
    material: &FinalVerificationResolvedMaterial,
    evidence: &FinalVerificationExecutionEvidence,
    ctx: &SlotContext,
) -> FinalVerificationRecordingOutcome {
    // This covers cancellation while evidence is being encoded, before the
    // cancellation-aware select directly around the insert below.
    if request.cancellation.is_cancelled() {
        return ineligible_outcome(attempt_id, "cancelled before persistence");
    }
    let (Some(f0), Some(f1), Some(pre), Some(post)) = (
        evidence.fingerprint_f0.as_ref(),
        evidence.fingerprint_f1.as_ref(),
        evidence.pre_environment_identity.as_ref(),
        evidence.post_environment_identity.as_ref(),
    ) else {
        return ineligible_outcome(attempt_id, "eligible evidence was incomplete");
    };
    if f0 != f1 || pre != post {
        return ineligible_outcome(attempt_id, "verification consistency boundary changed");
    }
    let ordered_commands = serde_json::Value::Array(
        evidence
            .commands
            .iter()
            .map(|command| {
                serde_json::json!({
                    "descriptor_id": command.descriptor.check_id,
                    "result": "pass",
                    "passed": true,
                    "started_at_unix_millis": command.started_at_unix_millis,
                    "completed_at_unix_millis": command.completed_at_unix_millis,
                })
            })
            .collect(),
    );
    let covered_checks = serde_json::Value::Array(
        evidence
            .commands
            .iter()
            .map(|command| serde_json::Value::String(command.descriptor.check_id.clone()))
            .collect(),
    );
    let required_commands: Vec<_> = evidence
        .commands
        .iter()
        .map(|command| RequiredFinalVerificationCommand {
            descriptor_id: &command.descriptor.check_id,
        })
        .collect();
    let required_checks = material.required_checks.clone();
    let identity_json = identity_json(pre);
    let completed_at = match time::OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(timestamp) => timestamp,
        Err(error) => {
            return error_outcome(attempt_id, &format!("timestamp formatting failed: {error}"));
        }
    };
    let repo = VerifyRunRepository::new(ctx.db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    let manifest_version = format!("manifest-v{}", evidence.manifest_version);
    let insert =
        repo.record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
            id: &id,
            task_run_id: &request.task_run_id,
            verify_source: material.verify_source.as_str(),
            verify_run_id,
            verification_attempt_id: attempt_id,
            required_commands: &required_commands,
            ordered_commands: &ordered_commands,
            covered_checks: &covered_checks,
            required_checks: &required_checks,
            verification_input_fingerprint: &f0.fingerprint,
            manifest_version: &manifest_version,
            environment_identity_json: &identity_json,
            environment_identity_digest: &pre.digest,
            environment_identity_version: "identity-v1",
            completed_at: &completed_at,
            diff_fingerprint: &material.diff_fingerprint,
        });
    // Do not poll the insert future until cancellation has had priority at the
    // last coordinator boundary before SQL.
    let insert_result = tokio::select! {
        biased;
        _ = request.cancellation.cancelled() => {
            return ineligible_outcome(attempt_id, "cancelled during persistence");
        }
        result = insert => result,
    };
    match insert_result {
        Ok(row) => FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id: attempt_id.to_owned(),
            verify_run_id: verify_run_id.to_owned(),
            evidence: Box::new(FinalVerificationSuccessEvidence {
                persisted_run_id: row.id,
                completed_at: row.completed_at,
                ordered_commands: row.ordered_commands.unwrap_or(ordered_commands),
                covered_checks: row.covered_checks.unwrap_or(covered_checks),
                required_checks,
                verification_input_fingerprint: row
                    .verification_input_fingerprint
                    .unwrap_or_else(|| f0.fingerprint.clone()),
                manifest_version: row.manifest_version.unwrap_or(manifest_version),
                environment_identity_digest: row
                    .environment_identity_digest
                    .unwrap_or_else(|| pre.digest.clone()),
            }),
        },
        Err(error) => error_outcome(
            attempt_id,
            &format!("final verification insert failed: {error}"),
        ),
    }
}

fn identity_json(identity: &EnvironmentIdentityV1) -> serde_json::Value {
    serde_json::from_str(&identity.canonical_json).unwrap_or_else(|_| {
        serde_json::json!({
            "canonical_json": identity.canonical_json,
            "schema_version": identity.schema_version,
            "canonicalization_version": identity.canonicalization_version,
        })
    })
}

fn format_evidence_reason(evidence: &FinalVerificationExecutionEvidence) -> String {
    evidence.eligibility_reason.as_ref().map_or_else(
        || "malformed final-verification evidence".to_owned(),
        |reason| format!("{reason:?}"),
    )
}

fn ineligible_outcome(attempt_id: &str, reason: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Ineligible {
        verification_attempt_id: attempt_id.to_owned(),
        reason: reason.to_owned(),
    }
}
fn error_outcome(attempt_id: &str, detail: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Error {
        verification_attempt_id: attempt_id.to_owned(),
        detail: detail.to_owned(),
    }
}
fn emit_ineligible(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    reason: &str,
) -> FinalVerificationRecordingOutcome {
    emit_outcome(request, ineligible_outcome(attempt_id, reason))
}
fn emit_error(
    request: &FinalVerificationCoordinatorRequest,
    attempt_id: &str,
    detail: &str,
) -> FinalVerificationRecordingOutcome {
    emit_outcome(request, error_outcome(attempt_id, detail))
}
fn emit_outcome(
    request: &FinalVerificationCoordinatorRequest,
    outcome: FinalVerificationRecordingOutcome,
) -> FinalVerificationRecordingOutcome {
    match &outcome {
        FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id,
            verify_run_id,
            evidence: _,
        } => {
            djinn_telemetry::final_verification::increment_record(
                djinn_telemetry::final_verification::RECORD_STORED,
            );
            tracing::info!(
                recording_outcome = "stored", task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id, verify_run_id = %verify_run_id,
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::Reused {
            verification_attempt_id,
            evidence,
        } => tracing::info!(
            recording_outcome = "reused", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id, verify_run_id = %evidence.persisted_run_id,
            "final verification recording completed"
        ),
        FinalVerificationRecordingOutcome::Ineligible {
            verification_attempt_id,
            reason,
        } => {
            djinn_telemetry::final_verification::increment_record(
                djinn_telemetry::final_verification::RECORD_INELIGIBLE,
            );
            tracing::info!(
                recording_outcome = "ineligible", task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id, reason = %reason,
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::Error {
            verification_attempt_id,
            detail,
        } => {
            djinn_telemetry::final_verification::increment_record(
                djinn_telemetry::final_verification::RECORD_ERROR,
            );
            tracing::error!(
                recording_outcome = "error", task_id = %request.task_id, task_run_id = %request.task_run_id,
                verification_attempt_id = %verification_attempt_id, detail = %detail,
                "final verification recording completed"
            )
        }
        FinalVerificationRecordingOutcome::NotConfigured {
            verification_attempt_id,
        } => tracing::info!(
            recording_outcome = "not_configured", task_id = %request.task_id, task_run_id = %request.task_run_id,
            verification_attempt_id = %verification_attempt_id,
            "final verification recording completed"
        ),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct TestLease(Result<(), String>);

    impl FinalVerificationInvocationLease for TestLease {
        fn release<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { self.0.clone() })
        }
    }

    fn stored() -> FinalVerificationRecordingOutcome {
        FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id: "attempt".to_owned(),
            verify_run_id: "run".to_owned(),
            evidence: Box::new(FinalVerificationSuccessEvidence {
                persisted_run_id: "persisted-run".to_owned(),
                completed_at: "2025-01-01T00:00:00Z".to_owned(),
                ordered_commands: serde_json::json!([]),
                covered_checks: serde_json::json!([]),
                required_checks: vec![],
                verification_input_fingerprint: "fingerprint".to_owned(),
                manifest_version: "manifest-v1".to_owned(),
                environment_identity_digest: "identity".to_owned(),
            }),
        }
    }

    #[tokio::test]
    async fn cancellation_at_persistence_boundary_never_starts_writer() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut lease = TestLease(Ok(()));
        let writer_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&writer_started);

        let outcome = release_then_persist(&mut lease, &cancellation, "attempt", move || {
            started.store(true, Ordering::SeqCst);
            async { stored() }
        })
        .await;

        assert!(matches!(
            outcome,
            FinalVerificationRecordingOutcome::Ineligible { .. }
        ));
        assert!(!writer_started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn lease_release_failure_never_starts_writer() {
        let cancellation = CancellationToken::new();
        let mut lease = TestLease(Err("release failed".to_owned()));
        let writer_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&writer_started);

        let outcome = release_then_persist(&mut lease, &cancellation, "attempt", move || {
            started.store(true, Ordering::SeqCst);
            async { stored() }
        })
        .await;

        assert!(matches!(
            outcome,
            FinalVerificationRecordingOutcome::Error { .. }
        ));
        assert!(!writer_started.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod recording_tests;
