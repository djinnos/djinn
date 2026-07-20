// djinn:allow-oversize — over size-guard byte threshold after arbiter decision plumbing; split when touched substantively.
//! `DirectServices` — in-process [`SupervisorServices`] impl.
//!
//! Phase 2 PR 3 replaced `djinn-supervisor`'s struct-with-callbacks
//! `SupervisorServices` with a trait.  `DirectServices` is the production
//! (and integration-test) impl: it wraps an [`AgentContext`], a
//! supervisor-wide [`CancellationToken`], and an optional test-only
//! [`LlmProvider`] override, and delegates every trait method straight into
//! the in-tree lifecycle helpers.  Behaviour is verbatim with PR 2 — this
//! file just reshapes the closure bodies that used to live on
//! `SupervisorServices` into trait-method bodies.
//!
//! The worker-side sibling impl (`djinn_supervisor::services::rpc::StubRpcServices`
//! → real RPC client in PR 4/5) lives on the other side of the crate split
//! so this code never links the bincode/Unix-socket plumbing.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::SessionRecord;
use djinn_core::models::{Task, TaskRunStatus};
use djinn_db::TaskRunRepository;
use djinn_db::repositories::llm_call_attempt::{
    CreateLlmCallAttemptParams, FinalizeLlmCallAttemptParams, LlmCallAttemptRepository,
    LlmCallOutcome,
};
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_db::repositories::task_run_outcome::TaskRunOutcomeRepository;
use djinn_db::{EffectiveCreatorProvenance, SessionRepository};
use djinn_stack::environment::EnvironmentConfig;
use djinn_supervisor::services::wire::{PlannerAttemptResult, PlannerOutcome};
use djinn_supervisor::services::{
    CostBasisHint, SerializableCreateSessionParams, SerializableCreateTaskRunParams,
    SerializableDjinnEvent,
};
use djinn_supervisor::{
    BranchPublicationResult, RoleKind, StageError, StageOutcome, SupervisorServices,
    TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::actors::slot::lifecycle::memory_intent_planner::parse_planned_queries;
use crate::context::AgentContext;
use crate::supervisor_impl::{SupervisorCallbackContext, execute_stage, supervisor_pr_open};
use djinn_provider::catalog::builtin::classify_provider;
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, LlmResponse, StreamEvent, TokenUsage, ToolChoice};
use futures::StreamExt;

/// Apply one provider event to the terminal response aggregate used by direct
/// services. Returns `true` when the event terminates the stream.
///
/// This production seam is shared by direct invocation and attributed planner
/// calls. A completed attributed thinking block is retained as content, but
/// its text is not appended to `thinking`: the matching delta already supplied
/// the display aggregate.
#[doc(hidden)]
pub fn append_direct_response_event(response: &mut LlmResponse, event: StreamEvent) -> bool {
    match event {
        StreamEvent::Delta(block) => response.content.push(block),
        StreamEvent::Thinking(thinking) => response.thinking.push_str(&thinking),
        StreamEvent::ThinkingDelta { text, .. } => response.thinking.push_str(&text),
        StreamEvent::ThinkingBlockComplete {
            thinking,
            signature,
            ..
        } => response.content.push(ContentBlock::Thinking {
            thinking,
            signature,
        }),
        StreamEvent::Usage(usage) => response.usage = usage,
        StreamEvent::Done => return true,
    }
    false
}

/// Drive the ordinary direct-invocation provider stream to its terminal
/// aggregate. This is deliberately shared by `invoke_llm` and the integration
/// test seam below so consumer tests execute the production collection loop,
/// rather than duplicating its event matching.
async fn collect_invoke_llm_stream(
    provider: &dyn LlmProvider,
    conversation: &Conversation,
    tools: &[serde_json::Value],
    tool_choice: Option<ToolChoice>,
) -> Result<LlmResponse, String> {
    let mut stream = provider
        .stream(conversation, tools, tool_choice)
        .await
        .map_err(|e| format!("provider stream init failed: {e}"))?;
    let mut response = LlmResponse {
        content: Vec::new(),
        thinking: String::new(),
        usage: TokenUsage::default(),
    };
    while let Some(ev) = stream.next().await {
        let event = ev.map_err(|e| format!("provider stream error: {e}"))?;
        if append_direct_response_event(&mut response, event) {
            break;
        }
    }
    Ok(response)
}

/// Production `invoke_llm` stream collector exposed solely for behavioral
/// integration tests that supply a scripted provider.
#[doc(hidden)]
pub async fn collect_invoke_llm_stream_for_test(
    provider: &dyn LlmProvider,
    conversation: &Conversation,
    tools: &[serde_json::Value],
    tool_choice: Option<ToolChoice>,
) -> Result<LlmResponse, String> {
    collect_invoke_llm_stream(provider, conversation, tools, tool_choice).await
}

/// In-process `SupervisorServices` impl that delegates straight to the
/// lifecycle helpers inside `djinn-agent`.
pub struct DirectServices {
    callbacks: SupervisorCallbackContext,
    /// Bound to the same `Database` carried in `callbacks.agent_context`.
    /// Phase 3 adds this so [`SupervisorServices::create_task_run`] and
    /// [`SupervisorServices::update_task_run_status`] can persist
    /// `task_run` rows in-process; until Phase 4 cuts the
    /// `TaskRunSupervisor::run` body over to the trait, these methods are
    /// dead code (the supervisor still calls `task_runs.create()` /
    /// `task_runs.update_status()` directly).
    task_runs: Arc<TaskRunRepository>,
    #[cfg(test)]
    planner_test_seam: Option<PlannerTestSeam>,
}

#[cfg(test)]
#[derive(Clone)]
struct PlannerTestSeam {
    provider: Arc<dyn LlmProvider>,
    ledger: Arc<dyn PlannerAttemptLedger>,
    model_id: String,
}

#[derive(Clone, Debug)]
struct PlannerLedgerCreate {
    id: String,
    project_id: String,
    task_id: String,
    task_run_id: String,
    session_id: String,
    created_by_user_id: String,
    operation: String,
    prompt_id: String,
    model_id: String,
    input_price: Option<f64>,
    output_price: Option<f64>,
    cache_read_price: Option<f64>,
    cache_write_price: Option<f64>,
}

#[derive(Clone, Debug)]
struct PlannerLedgerFinalize {
    id: String,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    diagnostic: Option<String>,
    outcome: LlmCallOutcome,
}

#[derive(Clone, Debug)]
struct PlannerLedgerFinalized {
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: Option<f64>,
    diagnostic: Option<String>,
}

#[async_trait]
trait PlannerAttemptLedger: Send + Sync {
    async fn create(&self, params: PlannerLedgerCreate) -> Result<(), String>;
    async fn finalize(
        &self,
        params: PlannerLedgerFinalize,
    ) -> Result<PlannerLedgerFinalized, String>;
}

struct RepositoryPlannerAttemptLedger(LlmCallAttemptRepository);

#[async_trait]
impl PlannerAttemptLedger for RepositoryPlannerAttemptLedger {
    async fn create(&self, params: PlannerLedgerCreate) -> Result<(), String> {
        self.0
            .create(CreateLlmCallAttemptParams {
                id: &params.id,
                project_id: &params.project_id,
                task_id: &params.task_id,
                task_run_id: Some(&params.task_run_id),
                session_id: Some(&params.session_id),
                created_by_user_id: Some(&params.created_by_user_id),
                operation: &params.operation,
                prompt_id: &params.prompt_id,
                model_id: &params.model_id,
                input_price_per_million_snapshot: params.input_price,
                output_price_per_million_snapshot: params.output_price,
                cache_read_price_per_million_snapshot: params.cache_read_price,
                cache_write_price_per_million_snapshot: params.cache_write_price,
            })
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn finalize(
        &self,
        params: PlannerLedgerFinalize,
    ) -> Result<PlannerLedgerFinalized, String> {
        self.0
            .finalize(FinalizeLlmCallAttemptParams {
                id: &params.id,
                tokens_in: params.tokens_in,
                tokens_out: params.tokens_out,
                cache_read_tokens: params.cache_read_tokens,
                cache_write_tokens: params.cache_write_tokens,
                diagnostic: params.diagnostic.as_deref(),
                outcome: params.outcome,
            })
            .await
            .map(|record| PlannerLedgerFinalized {
                tokens_in: record.tokens_in,
                tokens_out: record.tokens_out,
                cache_read_tokens: record.cache_read_tokens,
                cache_write_tokens: record.cache_write_tokens,
                cost_usd: record.cost_usd,
                diagnostic: record.diagnostic,
            })
            .map_err(|e| e.to_string())
    }
}

impl DirectServices {
    /// Construct a `DirectServices` bound to the given [`AgentContext`] and
    /// cancellation token.  Production path.
    pub fn new(agent_context: AgentContext, cancel: CancellationToken) -> Self {
        Self::with_provider_override(agent_context, cancel, None)
    }

    /// Same as [`DirectServices::new`] but installs a test-only
    /// [`LlmProvider`] override on the stage executor, bypassing the catalog
    /// / vault credential lookup inside `execute_stage`.  Used by
    /// `tests/phase1_supervisor.rs`.
    pub fn with_provider_override(
        agent_context: AgentContext,
        cancel: CancellationToken,
        provider_override: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        let task_runs = Arc::new(TaskRunRepository::new(agent_context.db.clone()));
        Self {
            callbacks: SupervisorCallbackContext {
                agent_context,
                cancel,
                provider_override,
            },
            task_runs,
            #[cfg(test)]
            planner_test_seam: None,
        }
    }

    #[cfg(test)]
    fn with_planner_test_seam(
        agent_context: AgentContext,
        provider: Arc<dyn LlmProvider>,
        ledger: Arc<dyn PlannerAttemptLedger>,
    ) -> Self {
        let mut services = Self::new(agent_context, CancellationToken::new());
        services.planner_test_seam = Some(PlannerTestSeam {
            provider,
            ledger,
            model_id: "planner-test-model".into(),
        });
        services
    }

    /// Execute the arbiter park transaction: persist the decision and dossier
    /// on the current unconsumed arbitration row, mark it consumed, create an
    /// autonomous **planner escalation** review task carrying the dossier
    /// (blocking the source), and emit `arbiter_decision` / `arbiter_parked`
    /// activity events.
    ///
    /// Djinn has NO human-review holds: `park` no longer strands the source on
    /// a human. Instead it creates a planner-dispatchable escalation task — the
    /// same autonomous planner-remediation shape the coordinator's intervention
    /// path uses — that a Planner SESSION resolves terminally (decompose +
    /// supersede, close as won't-fix, or re-scope + reopen the source). Closing
    /// the escalation releases the blocked source exactly like a human hold
    /// close did (blocker resolution + `human_review_resolved_at` stamp +
    /// tripwire release when applicable).
    ///
    /// Called BEFORE the `ArbiterPark` state transition so the escalation
    /// blocker exists before the source task lands at `open` (the ordering
    /// contract from 7f8u). On any arbitration-row failure, fails closed by
    /// creating the escalation with a fallback dossier rather than leaving the
    /// task stranded.
    async fn execute_arbiter_park_transaction(
        &self,
        task_id: &str,
        dossier_json: Option<&str>,
    ) -> Result<(), String> {
        use djinn_db::TaskRepository;
        use djinn_db::repositories::task_arbitration::{
            TaskArbitrationRepository, UpdateDispatchLedgerParams,
        };

        let db = self.callbacks.agent_context.db.clone();
        let event_bus = self.callbacks.agent_context.event_bus.clone();
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let arb_repo = TaskArbitrationRepository::new(db.clone());

        // Load the source task for project_id and short_id.
        let task = task_repo
            .get(task_id)
            .await
            .map_err(|e| format!("arbiter_park: failed to load task: {e}"))?
            .ok_or_else(|| format!("arbiter_park: task {task_id} not found"))?;

        // Parse the dossier JSON from the reason parameter.
        let dossier: serde_json::Value = dossier_json
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        // Resolve the current unconsumed arbitration row.
        let (hold_cycle, unconsumed_record) = arb_repo
            .resolve_current_hold_cycle(task_id)
            .await
            .map_err(|e| format!("arbiter_park: failed to resolve hold cycle: {e}"))?;

        // Dossier summary for activity events.
        let dossier_summary = dossier
            .get("hold_description")
            .and_then(|v| v.as_str())
            .unwrap_or("arbiter park decision")
            .chars()
            .take(200)
            .collect::<String>();

        // Persist the decision payload and park dossier on the arbitration
        // row, then mark it consumed. If no unconsumed row exists, create a
        // new one in consumed state as a fail-closed recovery.
        if let Some(ref record) = unconsumed_record {
            // Update the existing unconsumed row with the decision/dossier
            // and git-evidence fields from the arbitration row.
            let decision_json = serde_json::json!({
                "decision": "park",
                "dossier_summary": dossier_summary,
            });
            arb_repo
                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                    task_id,
                    hold_cycle: record.hold_cycle,
                    mirror_head_sha: record.mirror_head_sha.as_deref(),
                    github_head_sha: record.github_head_sha.as_deref(),
                    pr_url: record.pr_url.as_deref(),
                    failing_ci_job_ids: Some(&record.failing_ci_job_ids),
                    dossier: Some(&dossier),
                    directive: Some(&decision_json),
                    verification_command: None,
                    excluded_models: None,
                })
                .await
                .map_err(|e| format!("arbiter_park: failed to update arbitration row: {e}"))?;

            // Mark consumed exactly once.
            let consumed = arb_repo
                .mark_consumed(task_id, record.hold_cycle)
                .await
                .map_err(|e| format!("arbiter_park: failed to mark consumed: {e}"))?;
            if !consumed {
                tracing::warn!(
                    task_id = %task.short_id,
                    hold_cycle = record.hold_cycle,
                    "arbiter_park: arbitration row was already consumed — possible double-park"
                );
            }
        } else {
            // No unconsumed row exists. Fail closed: create a consumed row
            // with the dossier so the park transaction is recoverable.
            // Populate git-evidence from the task-level CI fields.
            tracing::warn!(
                task_id = %task.short_id,
                hold_cycle,
                "arbiter_park: no unconsumed arbitration row; creating a consumed recovery row"
            );
            use djinn_db::repositories::task_arbitration::CreateArbitrationParams;
            let failing_ci_job_ids = serde_json::json!([]);
            let excluded_models = serde_json::json!([]);
            let decision_json = serde_json::json!({
                "decision": "park",
                "dossier_summary": dossier_summary,
                "recovery": true,
            });
            arb_repo
                .try_create(CreateArbitrationParams {
                    task_id,
                    hold_cycle,
                    deadline_at: None,
                    mirror_head_sha: task.ci_mirror_head_sha.as_deref(),
                    github_head_sha: task.ci_github_head_sha.as_deref(),
                    pr_url: task.pr_url.as_deref(),
                    failing_ci_job_ids: &failing_ci_job_ids,
                    dossier: Some(&dossier),
                    directive: Some(&decision_json),
                    verification_command: None,
                    excluded_models: &excluded_models,
                })
                .await
                .map_err(|e| format!("arbiter_park: failed to create recovery row: {e}"))?;
            // Mark it consumed.
            let _ = arb_repo.mark_consumed(task_id, hold_cycle).await;
        }

        // Create an autonomous planner escalation task that blocks the source.
        // Reuses the same planner-dispatchable shape as the coordinator's
        // `create_remediation_task(RemediationKind::Planner)` intervention path —
        // NO human hold. The coordinator's normal dispatch pass routes the open
        // `review` escalation task to the Planner role.
        self.create_arbiter_planner_escalation(
            task_id,
            &task.project_id,
            &dossier,
            &dossier_summary,
        )
        .await?;

        // Extract git-evidence fields from the arbitration row when available.
        let (mirror_head, github_head, pr_url_val, failing_ci, created_at_str) =
            if let Some(ref record) = unconsumed_record {
                (
                    record.mirror_head_sha.clone(),
                    record.github_head_sha.clone(),
                    record.pr_url.clone(),
                    record.failing_ci_job_ids.clone(),
                    record.created_at.clone(),
                )
            } else {
                (None, None, None, serde_json::json!([]), String::new())
            };

        // Emit arbiter_decision activity with git-evidence fields.
        let decision_payload = serde_json::json!({
            "event": "arbiter_decision",
            "task_id": task.short_id,
            "hold_cycle": hold_cycle,
            "decision": "park",
            // Audit: park now escalates to an autonomous Planner session rather
            // than a human-review hold. No human is required to release the
            // source.
            "autonomous_escalation": true,
            "dossier_summary": dossier_summary,
            "mirror_head_sha": mirror_head,
            "github_head_sha": github_head,
            "pr_url": pr_url_val,
            "failing_ci_job_ids": failing_ci,
        });
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "system",
                "coordinator",
                "arbiter_decision",
                &decision_payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "arbiter_park: failed to log arbiter_decision activity"
            );
        }

        // Emit arbiter_parked activity with git-evidence fields.
        let parked_payload = serde_json::json!({
            "event": "arbiter_parked",
            "task_id": task.short_id,
            "hold_cycle": hold_cycle,
            "decision": "park",
            "autonomous_escalation": true,
            "dossier_summary": dossier_summary,
        });
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "system",
                "coordinator",
                "arbiter_parked",
                &parked_payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "arbiter_park: failed to log arbiter_parked activity"
            );
        }

        // Record park metric (existing).
        djinn_telemetry::task::increment_parked_labeled(0, 0, 0, task.reopen_count);

        // Emit arbiter rollout telemetry: decision + park outcome.
        djinn_telemetry::arbiter::record_decision(djinn_telemetry::arbiter::DECISION_PARK);
        djinn_telemetry::arbiter::record_park(
            djinn_telemetry::arbiter::PARK_REASON_ARBITER_DECIDED,
            djinn_telemetry::arbiter::PARK_OUTCOME_SUCCESS,
        );

        // Emit time-in-arbitration when the record creation time is available.
        if !created_at_str.is_empty()
            && let Ok(created_at) = time::OffsetDateTime::parse(
                &created_at_str,
                &time::format_description::well_known::Rfc3339,
            )
        {
            let elapsed = (time::OffsetDateTime::now_utc() - created_at).as_seconds_f64();
            if elapsed >= 0.0 {
                djinn_telemetry::arbiter::record_time_in_arbitration(elapsed);
            }
        }

        Ok(())
    }

    /// Create an autonomous **planner escalation** review task that blocks the
    /// source task. The full arbiter dossier becomes the escalation task's
    /// description so the Planner session has the structured failure analysis,
    /// attempted decisions, and recommended action as context.
    ///
    /// The task is a NORMAL open `review` task carrying the
    /// `planner-park-escalation` label (NOT `human-review-hold`): the
    /// coordinator's dispatch pass routes it to the Planner role, and the
    /// Planner owns terminal resolution. Closing it releases the blocked source
    /// (blocker resolution + `human_review_resolved_at` stamp + tripwire
    /// release) via the broadened close path (`releases_source_on_close`).
    async fn create_arbiter_planner_escalation(
        &self,
        source_task_id: &str,
        project_id: &str,
        dossier: &serde_json::Value,
        dossier_summary: &str,
    ) -> Result<(), String> {
        use djinn_db::TaskRepository;

        let db = self.callbacks.agent_context.db.clone();
        let event_bus = self.callbacks.agent_context.event_bus.clone();
        let task_repo = TaskRepository::new(db.clone(), event_bus);

        // Load the source task for naming the escalation task.
        let source_task = task_repo.get(source_task_id).await.ok().flatten();
        let source_creator = source_task
            .as_ref()
            .and_then(|t| t.created_by_user_id.clone());

        // Idempotency: if the source already has an unresolved blocker
        // (from a prior park), skip creating a duplicate escalation.
        if let Some(ref src) = source_task {
            match task_repo.list_blockers(&src.id).await {
                Ok(blockers) if blockers.iter().any(|b| b.status != "closed") => {
                    tracing::info!(
                        source_task_id = %src.short_id,
                        "arbiter_park: planner escalation skipped — source already blocked"
                    );
                    return Ok(());
                }
                _ => {}
            }
        }

        // Build the escalation body from the arbiter dossier. Keep the
        // "Arbiter park decision" lead-in so the dossier context is unmistakable
        // in the task feed.
        let dossier_text = format!(
            "Arbiter park decision — autonomous planner remediation.\n\n{}",
            serde_json::to_string_pretty(dossier).unwrap_or_else(|_| dossier.to_string())
        );

        let title = match source_task.as_ref() {
            Some(t) => {
                let name: String = t.title.chars().take(70).collect();
                format!("Planner remediation [{}]: {}", t.short_id, name)
            }
            None => format!(
                "Arbiter park escalation: {}",
                &dossier_summary[..dossier_summary.len().min(60)]
            ),
        };
        let source_label = source_task
            .as_ref()
            .map(|t| format!("{} ({})", t.title, t.short_id))
            .unwrap_or_else(|| source_task_id.to_string());
        let description = format!(
            "Escalated from task {source_label}. The arbiter decided to PARK and \
             handed you (the Planner) terminal ownership of this task.\n\nYou OWN \
             the resolution: decompose the source into replacement subtasks and \
             supersede it, close it as won't-fix with a reason, or re-scope and \
             reopen it. Do NOT create another escalation and do NOT wait for a \
             human — closing THIS task releases the blocked source.\n\nReason: {}",
            dossier_text
        );
        let instructions = "The arbiter parked this task and handed you terminal ownership. \
             Resolve it autonomously (decompose + supersede, close as won't-fix, or re-scope + \
             reopen the source); closing this task releases the blocked source. Do NOT escalate \
             again and do NOT wait for a human.";

        // Create the escalation review task.
        let review_task = match djinn_core::auth_context::SESSION_USER_ID
            .scope(
                source_creator.clone(),
                task_repo.create_in_project_with_provenance(
                    project_id,
                    None,
                    EffectiveCreatorProvenance {
                        explicit_user_id: source_creator.as_deref(),
                        source_task_id: source_task.as_ref().map(|task| task.id.as_str()),
                        proposal_id: None,
                    },
                    &title,
                    &description,
                    instructions,
                    "review",
                    0,
                    "system",
                    Some("open"),
                    None,
                ),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return Err(format!(
                    "arbiter_park: failed to create planner escalation task: {e}"
                ));
            }
        };

        // Block the source on the escalation task.
        if let Some(ref src) = source_task
            && let Err(e) = task_repo.add_blocker(&src.id, &review_task.id).await
        {
            tracing::warn!(
                error = %e,
                source_task_id = %src.short_id,
                review_task_id = %review_task.short_id,
                "arbiter_park: failed to block source on planner escalation task"
            );
        }

        // Label the escalation so the close path runs the source-release
        // semantics (`releases_source_on_close`). This is deliberately NOT
        // `human-review-hold`: the task must stay planner-dispatchable and
        // planner-closable.
        if let Err(e) = task_repo
            .update_labels(&review_task.id, r#"["planner-park-escalation"]"#)
            .await
        {
            tracing::warn!(
                error = %e,
                review_task_id = %review_task.short_id,
                "arbiter_park: failed to label planner escalation task"
            );
        }

        Ok(())
    }

    /// Execute the arbiter supersede transaction: persist the decision on the
    /// current unconsumed arbitration row and mark it consumed, emit an
    /// `arbiter_decision` activity with decision `"supersede"` and the
    /// replacement ids, transfer the source task's downstream blockers to the
    /// last replacement subtask, and clean up the task branch/PR. Unlike the
    /// park path this creates NO human-review hold — the replacement subtasks
    /// created by the arbiter already carry the work forward, so the caller
    /// force-closes the source (via the `arbiter_supersede → closed`
    /// transition that runs after this method returns).
    ///
    /// `payload_json` is the JSON payload carried on the transition:
    /// `{"reason": "...", "replacement_task_ids": ["..."]}`. Returns the
    /// human-readable reason (referencing the replacement short_ids) that the
    /// caller logs with the force-close transition.
    async fn execute_arbiter_supersede_transaction(
        &self,
        task_id: &str,
        payload_json: Option<&str>,
    ) -> Result<String, String> {
        use djinn_db::TaskRepository;
        use djinn_db::repositories::task_arbitration::{
            CreateArbitrationParams, TaskArbitrationRepository, UpdateDispatchLedgerParams,
        };

        let db = self.callbacks.agent_context.db.clone();
        let event_bus = self.callbacks.agent_context.event_bus.clone();
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let arb_repo = TaskArbitrationRepository::new(db.clone());

        // Load the source task for project_id / short_id.
        let task = task_repo
            .get(task_id)
            .await
            .map_err(|e| format!("arbiter_supersede: failed to load task: {e}"))?
            .ok_or_else(|| format!("arbiter_supersede: task {task_id} not found"))?;

        // Parse the payload: reason + replacement task ids.
        let payload: serde_json::Value = payload_json
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let raw_reason = payload
            .get("reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let replacement_ids: Vec<String> = payload
            .get("replacement_task_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Resolve replacement ids → short_ids for the reason/activity, and
        // capture the last replacement's canonical id for blocker transfer.
        let mut replacement_short_ids: Vec<String> = Vec::new();
        let mut last_replacement_id: Option<String> = None;
        for id in &replacement_ids {
            match task_repo.resolve(id).await {
                Ok(Some(t)) => {
                    replacement_short_ids.push(t.short_id.clone());
                    last_replacement_id = Some(t.id.clone());
                }
                _ => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        replacement = %id,
                        "arbiter_supersede: replacement task did not resolve; skipping"
                    );
                    replacement_short_ids.push(id.clone());
                }
            }
        }

        let human_reason = match raw_reason {
            Some(r) => r,
            None if replacement_short_ids.is_empty() => "arbiter superseded task".to_string(),
            None => format!(
                "Superseded by replacement subtasks: {}",
                replacement_short_ids.join(", ")
            ),
        };

        // Resolve the current unconsumed arbitration row and persist the
        // supersede decision, then mark it consumed. Mirrors the park path so
        // the hold cycle is closed out exactly once.
        let (hold_cycle, unconsumed_record) = arb_repo
            .resolve_current_hold_cycle(task_id)
            .await
            .map_err(|e| format!("arbiter_supersede: failed to resolve hold cycle: {e}"))?;

        let decision_json = serde_json::json!({
            "decision": "supersede",
            "replacement_task_ids": replacement_short_ids,
        });

        if let Some(ref record) = unconsumed_record {
            arb_repo
                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                    task_id,
                    hold_cycle: record.hold_cycle,
                    mirror_head_sha: None,
                    github_head_sha: None,
                    pr_url: None,
                    failing_ci_job_ids: None,
                    dossier: None,
                    directive: Some(&decision_json),
                    verification_command: None,
                    excluded_models: None,
                })
                .await
                .map_err(|e| format!("arbiter_supersede: failed to update arbitration row: {e}"))?;
            let consumed = arb_repo
                .mark_consumed(task_id, record.hold_cycle)
                .await
                .map_err(|e| format!("arbiter_supersede: failed to mark consumed: {e}"))?;
            if !consumed {
                tracing::warn!(
                    task_id = %task.short_id,
                    hold_cycle = record.hold_cycle,
                    "arbiter_supersede: arbitration row was already consumed"
                );
            }
        } else {
            // Fail closed: create a consumed recovery row so the decision is
            // durable even when no unconsumed row exists.
            tracing::warn!(
                task_id = %task.short_id,
                hold_cycle,
                "arbiter_supersede: no unconsumed arbitration row; creating a consumed recovery row"
            );
            let failing_ci_job_ids = serde_json::json!([]);
            let excluded_models = serde_json::json!([]);
            arb_repo
                .try_create(CreateArbitrationParams {
                    task_id,
                    hold_cycle,
                    deadline_at: None,
                    mirror_head_sha: None,
                    github_head_sha: None,
                    pr_url: None,
                    failing_ci_job_ids: &failing_ci_job_ids,
                    dossier: None,
                    directive: Some(&decision_json),
                    verification_command: None,
                    excluded_models: &excluded_models,
                })
                .await
                .map_err(|e| format!("arbiter_supersede: failed to create recovery row: {e}"))?;
            let _ = arb_repo.mark_consumed(task_id, hold_cycle).await;
        }

        // Transfer downstream blocker edges: any task blocked by the closing
        // source is re-pointed at the last replacement so it is not prematurely
        // dispatched when the force-close resolves the source blocker. Mirrors
        // the tool-path `force_close` replacement_task_ids mechanism.
        if let Some(ref last_id) = last_replacement_id {
            let downstream = task_repo
                .list_blocked_by(&task.id)
                .await
                .unwrap_or_default();
            for blocked_ref in &downstream {
                if let Err(e) = task_repo.add_blocker(&blocked_ref.task_id, last_id).await {
                    tracing::warn!(
                        task_id = %task.short_id,
                        blocked = %blocked_ref.task_id,
                        error = %e,
                        "arbiter_supersede: failed to transfer downstream blocker to replacement"
                    );
                }
            }
        }

        // Emit arbiter_decision activity.
        let decision_payload = serde_json::json!({
            "event": "arbiter_decision",
            "task_id": task.short_id,
            "hold_cycle": hold_cycle,
            "decision": "supersede",
            "replacement_task_ids": replacement_short_ids,
        });
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "system",
                "coordinator",
                "arbiter_decision",
                &decision_payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "arbiter_supersede: failed to log arbiter_decision activity"
            );
        }

        // Branch hygiene: delete the task branch on the local mirror and the
        // GitHub remote so it doesn't linger as a dead ref / open PR. Deleting
        // the remote ref closes any PR still open on that head — exactly the
        // superseded-PR cleanup this decision exists to automate. Best-effort.
        crate::task_merge::cleanup_task_branches_post_close(
            &task.id,
            &db,
            &event_bus,
            self.callbacks.agent_context.mirror.as_deref(),
        )
        .await;

        Ok(human_reason)
    }
}

#[async_trait]
impl SupervisorServices for DirectServices {
    fn cancel(&self) -> &CancellationToken {
        &self.callbacks.cancel
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        crate::actors::slot::helpers::load_task(&task_id, &self.callbacks.agent_context)
            .await
            .map_err(|e| e.to_string())
    }

    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        execute_stage(
            task,
            workspace,
            role_kind,
            task_run_id,
            spec,
            &self.callbacks,
            self,
        )
        .await
    }

    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome {
        supervisor_pr_open(spec, task, &self.callbacks).await
    }

    async fn create_task_run(&self, params: SerializableCreateTaskRunParams) -> Result<(), String> {
        let attempt_id = params
            .task_attempt_id
            .as_deref()
            .ok_or_else(|| "task-run creation requires an exact task attempt ID".to_owned())?;
        TaskRunOutcomeRepository::new(self.callbacks.agent_context.db.clone())
            .create_run_for_attempt(
                CreateTaskRunParams {
                    id: params.id.as_str(),
                    project_id: params.project_id.as_str(),
                    task_id: params.task_id.as_str(),
                    trigger_type: params.trigger_type.as_str(),
                    status: params.status.as_deref(),
                    workspace_path: params.workspace_path.as_deref(),
                    mirror_ref: params.mirror_ref.as_deref(),
                },
                attempt_id,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String> {
        self.task_runs
            .update_status(&run_id, status)
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String> {
        self.callbacks
            .agent_context
            .catalog
            .find_model(&model_id)
            .map(|m| m.context_window)
            .ok_or_else(|| format!("model not found in catalog: {model_id}"))
    }

    async fn get_provider_base_url(&self, catalog_provider_id: String) -> Result<String, String> {
        let base_url = self
            .callbacks
            .agent_context
            .catalog
            .list_providers()
            .iter()
            .find(|p| p.id == catalog_provider_id)
            .map(|p| p.base_url.clone())
            .ok_or_else(|| format!("provider not found in catalog: {catalog_provider_id}"))?;
        if base_url.is_empty() {
            return Err(format!(
                "provider has empty base_url in catalog: {catalog_provider_id}"
            ));
        }
        Ok(base_url)
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        let catalog = &self.callbacks.agent_context.catalog;
        for provider in catalog.list_providers() {
            if let Some(model) = catalog.list_models(&provider.id).first() {
                return Ok(Some(format!("{}/{}", provider.id, model.id)));
            }
        }
        Ok(None)
    }

    async fn create_session(
        &self,
        params: SerializableCreateSessionParams,
    ) -> Result<SessionRecord, String> {
        let ctx = &self.callbacks.agent_context;
        let repo = SessionRepository::new(ctx.db.clone(), ctx.event_bus.clone());
        // Resolve the current catalog pricing at session start so later cost
        // calculations don't require catalog access. Uncatalogued models
        // produce `None` — all snapshot columns and `cost_usd` stay NULL.
        let catalog_model = ctx.catalog.find_model(params.model.as_str());
        let pricing = catalog_model.as_ref().map(|m| m.pricing.clone());
        // Derive cost basis using the explicit credential billing hint when
        // available (forward-fix for Codex OAuth + coding-plan credentials
        // surfacing under plain API-key provider namespaces), otherwise fall
        // back to provider/model subscription rules + pricing availability.
        let cost_basis = determine_cost_basis(
            params.cost_basis_hint,
            pricing.as_ref(),
            catalog_model.as_ref().map(|m| m.provider_id.as_str()),
        );
        let created = repo
            .create(CreateSessionParams {
                project_id: params.project_id.as_str(),
                task_id: params.task_id.as_deref(),
                model: params.model.as_str(),
                agent_type: params.agent_type.as_str(),
                metadata_json: params.metadata_json.as_deref(),
                task_run_id: params.task_run_id.as_deref(),
                pricing: pricing.as_ref(),
                cost_basis: Some(cost_basis),
            })
            .await
            .map_err(|e| e.to_string())?;
        // Persist the credential kind (`plan_oauth` / `api_key`) derived from the
        // resolved credential at model-resolution time, so plan-vs-API-key usage
        // is queryable after the fact. A dedicated post-insert write rather than
        // a `CreateSessionParams` field: only this dispatch host path carries the
        // signal, and the ~90 other `create()` call sites legitimately leave the
        // column `NULL`.
        match params.billing_source {
            Some(source) => repo
                .set_billing_source(&created.id, source.as_db_str())
                .await
                .map_err(|e| e.to_string()),
            None => Ok(created),
        }
    }

    async fn publish_session_message(
        &self,
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    ) -> Result<(), String> {
        self.callbacks
            .agent_context
            .event_bus
            .send(DjinnEventEnvelope::session_message(
                &session_id,
                &task_id,
                &agent_type,
                &message,
            ));
        Ok(())
    }

    async fn get_environment_config(
        &self,
        project_id: String,
    ) -> Result<EnvironmentConfig, String> {
        let cfg = crate::environment::environment_config_for_project_id(
            &self.callbacks.agent_context.db,
            &project_id,
        )
        .await;
        Ok(cfg)
    }

    async fn invoke_llm(
        &self,
        model_id: String,
        conversation: Conversation,
        tools: Vec<serde_json::Value>,
        tool_choice: Option<ToolChoice>,
    ) -> Result<LlmResponse, String> {
        // Resolve model + credential from the catalog + vault. The
        // task_id slot is a synthetic identifier used only for telemetry /
        // event-bus correlation; there is no real Task row backing this
        // host-side invocation.
        let synthetic_task_id = format!("invoke_llm:{model_id}");
        let resolved =
            crate::actors::slot::lifecycle::model_resolution::resolve_model_and_credential(
                &model_id,
                &synthetic_task_id,
                &self.callbacks.agent_context,
            )
            .await
            .map_err(|e| e.reason)?;

        // Build the provider from the resolved credential. Mirrors the
        // construction in `supervisor_impl::stage` — minus the session
        // affinity key (we have no session here).
        let context_window = self
            .get_model_context_window(model_id.clone())
            .await
            .unwrap_or(0)
            .max(0) as u32;
        let telemetry_meta = crate::actors::slot::helpers::build_telemetry_meta_with_attribution(
            "invoke_llm",
            &synthetic_task_id,
            None,
            None,
        );
        // Look up the API base URL only for API-key providers (OAuth configs
        // carry their own); then build the provider via the shared helper.
        let base_url = if crate::actors::slot::helpers::resolved_needs_base_url(&resolved) {
            self.get_provider_base_url(resolved.catalog_provider_id.clone())
                .await
                .unwrap_or_else(|_| {
                    crate::actors::slot::helpers::default_base_url(&resolved.catalog_provider_id)
                })
        } else {
            String::new()
        };
        // Build a RestampTarget from catalog metadata so model-dependent
        // defaults (reasoning_effort, max_tokens_default, format_family,
        // tool_schema_compat) reflect the target model.
        let restamp_target = crate::actors::slot::helpers::build_restamp_target(
            &resolved.catalog_provider_id,
            &resolved.model_name,
            context_window,
            &self.callbacks.agent_context.catalog,
        );
        let provider: Box<dyn LlmProvider> =
            crate::actors::slot::helpers::build_provider_from_resolved(
                resolved,
                context_window,
                Some(telemetry_meta),
                None,
                base_url,
                &restamp_target,
            )
            .ok_or_else(|| "no provider credential resolved for model".to_string())?;

        // Drive the stream to completion and collect the terminal aggregate.
        collect_invoke_llm_stream(provider.as_ref(), &conversation, &tools, tool_choice).await
    }

    async fn plan_memory_intents(
        &self,
        request: djinn_supervisor::services::wire::AttributedPlannerRequest,
    ) -> Result<PlannerAttemptResult, String> {
        use djinn_provider::completion::resolve_memory_provider_config_for_user_db;
        use djinn_provider::provider::create_provider;
        use uuid::Uuid;

        let ctx = &self.callbacks.agent_context;
        let db = &ctx.db;
        let repository_ledger: Arc<dyn PlannerAttemptLedger> =
            Arc::new(RepositoryPlannerAttemptLedger(
                LlmCallAttemptRepository::new(db.clone(), ctx.event_bus.clone()),
            ));
        #[cfg(test)]
        let ledger = self
            .planner_test_seam
            .as_ref()
            .map(|seam| seam.ledger.clone())
            .unwrap_or(repository_ledger);
        #[cfg(not(test))]
        let ledger = repository_ledger;

        // This primitive is deliberately attributed-only. Reject empty values
        // before credential resolution or ledger insertion so an RPC caller
        // cannot downgrade an enabled planner call into an anonymous attempt.
        for (name, value) in [
            ("project_id", request.project_id.as_str()),
            ("task_id", request.task_id.as_str()),
            ("task_run_id", request.task_run_id.as_str()),
            ("session_id", request.session_id.as_str()),
            ("created_by_user_id", request.created_by_user_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "attributed planner request requires non-empty {name}"
                ));
            }
        }

        let conversation: Conversation = serde_json::from_str(&request.conversation)
            .map_err(|e| format!("decode planner conversation: {e}"))?;
        let tools: Vec<serde_json::Value> = serde_json::from_str(&request.tools)
            .map_err(|e| format!("decode planner tools: {e}"))?;

        let call_id = Uuid::now_v7().to_string();

        // Production always resolves model + credential under the caller-scoped
        // policy. Tests replace only this boundary with a deterministic provider.
        #[cfg(test)]
        let provider_and_model = self
            .planner_test_seam
            .as_ref()
            .map(|seam| (seam.provider.clone(), seam.model_id.clone()));
        #[cfg(not(test))]
        let provider_and_model: Option<(Arc<dyn LlmProvider>, String)> = None;

        let (provider, model_id) = if let Some(pair) = provider_and_model {
            pair
        } else {
            let (provider_config, model_id) =
                resolve_memory_provider_config_for_user_db(db, Some(&request.created_by_user_id))
                    .await
                    .map_err(|e| format!("resolve memory provider for planner: {e}"))?;
            (Arc::from(create_provider(provider_config)), model_id)
        };

        // Snapshot catalog pricing at the time of the call.
        let catalog_model = ctx.catalog.find_model(&model_id);
        let input_price = catalog_model.as_ref().map(|m| m.pricing.input_per_million);
        let output_price = catalog_model.as_ref().map(|m| m.pricing.output_per_million);
        let cache_read_price = catalog_model
            .as_ref()
            .map(|m| m.pricing.cache_read_per_million);
        let cache_write_price = catalog_model
            .as_ref()
            .map(|m| m.pricing.cache_write_per_million);

        // Insert pending attempt before provider I/O.
        ledger
            .create(PlannerLedgerCreate {
                id: call_id.clone(),
                project_id: request.project_id.clone(),
                task_id: request.task_id.clone(),
                task_run_id: request.task_run_id.clone(),
                session_id: request.session_id.clone(),
                created_by_user_id: request.created_by_user_id.clone(),
                operation: request.operation.clone(),
                prompt_id: request.prompt_id.clone(),
                model_id,
                input_price,
                output_price,
                cache_read_price,
                cache_write_price,
            })
            .await
            .map_err(|e| format!("persist planner attempt: {e}"))?;

        let collected = collect_planner_stream(
            provider.as_ref(),
            &conversation,
            &tools,
            request.tool_choice,
            request.timeout_ms,
        )
        .await;
        let response = collected.response;
        let mut outcome = match collected.outcome {
            PlannerOutcome::Success => LlmCallOutcome::Success,
            PlannerOutcome::Timeout => LlmCallOutcome::Timeout,
            PlannerOutcome::InvalidPayload => LlmCallOutcome::InvalidPayload,
            PlannerOutcome::ProviderError => LlmCallOutcome::ProviderError,
        };
        let mut diagnostic = collected.diagnostic;

        let content_text = response
            .content
            .iter()
            .map(|b| b.as_text().unwrap_or(""))
            .collect::<String>();

        if collected.completed {
            // Validate the complete typed planner contract before terminal
            // success persistence. This includes JSON shape, the closed note
            // type set, query count, and the Phase-1 query style rules. This
            // is a dedicated planner operation: wire attribution fields are
            // not a caller-controlled opt-out from payload validation.
            let valid =
                !content_text.trim().is_empty() && parse_planned_queries(&content_text).is_ok();
            if valid {
                outcome = LlmCallOutcome::Success;
            } else {
                outcome = LlmCallOutcome::InvalidPayload;
                diagnostic = Some("planner output failed payload validation".into());
            }
        }

        // Finalize the ledger row with the latest usage.
        let finalized = ledger
            .finalize(PlannerLedgerFinalize {
                id: call_id,
                tokens_in: response.usage.input as i64,
                tokens_out: response.usage.output as i64,
                cache_read_tokens: response.usage.cache_read as i64,
                cache_write_tokens: response.usage.cache_write as i64,
                diagnostic: diagnostic.clone(),
                outcome,
            })
            .await;

        // If finalization itself fails, the pending row remains reconcilable
        // and we must fail open (no injectable planner output).
        let finalized = match finalized {
            Ok(r) => r,
            Err(e) => {
                return Ok(PlannerAttemptResult {
                    outcome: PlannerOutcome::ProviderError,
                    content: None,
                    tokens_in: response.usage.input as i64,
                    tokens_out: response.usage.output as i64,
                    cache_read_tokens: response.usage.cache_read as i64,
                    cache_write_tokens: response.usage.cache_write as i64,
                    cost_usd: None,
                    diagnostic: Some(format!("ledger finalization failed: {e}")),
                });
            }
        };

        let planner_outcome = match outcome {
            LlmCallOutcome::Success => PlannerOutcome::Success,
            LlmCallOutcome::Timeout => PlannerOutcome::Timeout,
            LlmCallOutcome::InvalidPayload => PlannerOutcome::InvalidPayload,
            LlmCallOutcome::ProviderError => PlannerOutcome::ProviderError,
        };

        Ok(PlannerAttemptResult {
            outcome: planner_outcome,
            content: if planner_outcome == PlannerOutcome::Success {
                Some(content_text)
            } else {
                None
            },
            tokens_in: finalized.tokens_in,
            tokens_out: finalized.tokens_out,
            cache_read_tokens: finalized.cache_read_tokens,
            cache_write_tokens: finalized.cache_write_tokens,
            cost_usd: finalized.cost_usd,
            diagnostic: finalized.diagnostic.clone().or(diagnostic),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_session_status(
        &self,
        session_id: String,
        status: djinn_core::models::SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
        parked_reason: Option<String>,
    ) -> Result<(), String> {
        let ctx = &self.callbacks.agent_context;
        let repo = SessionRepository::new(ctx.db.clone(), ctx.event_bus.clone());
        repo.update(
            &session_id,
            status,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            parked_reason,
        )
        .await
        .map(|_record| ())
        .map_err(|e| e.to_string())
    }

    async fn flush_session_tokens(
        &self,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Result<(), String> {
        let ctx = &self.callbacks.agent_context;
        let repo = SessionRepository::new(ctx.db.clone(), ctx.event_bus.clone());
        repo.flush_tokens(&session_id, tokens_in, tokens_out, cache_read, cache_write)
            .await
            .map_err(|e| e.to_string())
    }

    async fn tool_github_search(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        crate::extension::handlers::call_github_search(
            &self.callbacks.agent_context,
            &Some(arguments),
            project_id.as_deref(),
        )
        .await
    }

    async fn tool_github_fetch_file(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        crate::extension::handlers::call_github_fetch_file(
            &self.callbacks.agent_context,
            &Some(arguments),
            project_id.as_deref(),
        )
        .await
    }

    async fn tool_ci_job_log(
        &self,
        session_task_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        crate::extension::handlers::call_ci_job_log(
            &self.callbacks.agent_context,
            &Some(arguments),
            session_task_id.as_deref(),
        )
        .await
    }

    async fn tool_ci_artifact(
        &self,
        session_task_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        crate::extension::handlers::call_ci_artifact(
            &self.callbacks.agent_context,
            &Some(arguments),
            session_task_id.as_deref(),
        )
        .await
    }

    async fn touch_activity(&self, task_id: String) -> Result<(), String> {
        self.callbacks.agent_context.touch_activity(&task_id);
        Ok(())
    }

    async fn transition_task(
        &self,
        task_id: String,
        action: String,
        reason: Option<String>,
    ) -> Result<(), String> {
        use djinn_core::models::TransitionAction;
        use djinn_db::TaskRepository;
        let parsed = TransitionAction::parse(&action).map_err(|e| e.to_string())?;

        // The reason string threaded into the state-machine transition (and
        // hence the activity log). Overridden below for `arbiter_supersede`,
        // whose wire `reason` is a JSON payload rather than human-readable text.
        let mut effective_reason = reason.clone();

        // Arbiter park: execute the full park transaction before the state
        // transition so the planner-escalation blocker exists before the task
        // lands at `open` (the ordering contract from 7f8u).
        if matches!(parsed, TransitionAction::ArbiterPark) {
            self.execute_arbiter_park_transaction(&task_id, reason.as_deref())
                .await?;
        }

        // Arbiter supersede: execute the supersede transaction before the
        // terminal force-close so the arbitration row is consumed, the
        // `arbiter_decision` activity is emitted, downstream blockers are
        // transferred to the last replacement, and the task branch/PR are
        // cleaned up. No human-review hold is created. Returns the
        // human-readable reason (referencing the replacement short_ids) that
        // is logged with the force-close transition.
        if matches!(parsed, TransitionAction::ArbiterSupersede) {
            let human_reason = self
                .execute_arbiter_supersede_transaction(&task_id, reason.as_deref())
                .await?;
            effective_reason = Some(human_reason);
        }

        let repo = TaskRepository::new(
            self.callbacks.agent_context.db.clone(),
            self.callbacks.agent_context.event_bus.clone(),
        );
        repo.transition(
            &task_id,
            parsed.clone(),
            "supervisor",
            "system",
            effective_reason.as_deref(),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())?;

        // Supervisor-driven rework reopens (task_review_reject* /
        // lead_approve_conflict) must terminalize the worker's in-flight
        // attempt to `reopened` and record a durable rework marker — otherwise
        // the reviewer's rejection leaves an orphaned `submitted` attempt that
        // wedges the respawn guard's step-2 dedup forever (the ylme bug). The
        // PR poller's apply_pr_transition already owns the PrCiFailed /
        // PrChangesRequested / PrConflict reopens; this covers the transitions
        // it does not. A no-op for every non-rework action.
        djinn_coordinator::record_supervisor_rework_reopen(
            &self.callbacks.agent_context.db,
            &task_id,
            &parsed,
            reason.as_deref(),
        )
        .await;

        Ok(())
    }

    async fn emit_djinn_event(&self, event: SerializableDjinnEvent) -> Result<(), String> {
        match intern_envelope(event) {
            Ok(envelope) => {
                self.callbacks.agent_context.event_bus.send(envelope);
                Ok(())
            }
            Err(unknown_pair) => {
                // Unknown (entity_type, action) — drop with a log instead of
                // leaking the strings. New worker-emitted events need a row
                // in the `intern_envelope` match before they can cross the
                // wire. This keeps the host immune to drift from
                // future worker additions without a coordinated update.
                tracing::warn!(
                    entity_type = %unknown_pair.0,
                    action = %unknown_pair.1,
                    "emit_djinn_event: dropping unknown (entity_type, action) pair from worker"
                );
                Ok(())
            }
        }
    }

    /// Persist the arbiter decision (approve / approve_conflict) and its
    /// evidence on the current unconsumed arbitration row, then emit an
    /// `arbiter_decision` activity event.  Non-fatal on any individual
    /// failure — the caller logs and proceeds with the board transition.
    async fn record_arbiter_decision(
        &self,
        task_id: String,
        decision: String,
        evidence_json: String,
    ) -> Result<(), String> {
        use djinn_db::TaskRepository;
        use djinn_db::repositories::task_arbitration::{
            TaskArbitrationRepository, UpdateDispatchLedgerParams,
        };

        let db = self.callbacks.agent_context.db.clone();
        let event_bus = self.callbacks.agent_context.event_bus.clone();
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let arb_repo = TaskArbitrationRepository::new(db.clone());

        // Load the source task for short_id (activity logging).
        let task = task_repo
            .get(&task_id)
            .await
            .map_err(|e| format!("record_arbiter_decision: failed to load task: {e}"))?
            .ok_or_else(|| format!("record_arbiter_decision: task {task_id} not found"))?;

        // Parse evidence.
        let evidence: serde_json::Value =
            serde_json::from_str(&evidence_json).unwrap_or_else(|_| serde_json::json!({}));
        let evidence_summary = evidence
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("arbiter approval")
            .chars()
            .take(200)
            .collect::<String>();

        // Resolve the current unconsumed arbitration row.
        let (_hold_cycle, unconsumed_record) = arb_repo
            .resolve_current_hold_cycle(&task_id)
            .await
            .map_err(|e| format!("record_arbiter_decision: failed to resolve hold cycle: {e}"))?;

        // Build the decision payload.
        let decision_json = serde_json::json!({
            "decision": decision,
            "evidence_summary": evidence_summary,
        });

        if let Some(ref record) = unconsumed_record {
            // Update the existing unconsumed row with the decision, evidence,
            // and git-evidence fields from the arbitration row.
            arb_repo
                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                    task_id: &task_id,
                    hold_cycle: record.hold_cycle,
                    mirror_head_sha: record.mirror_head_sha.as_deref(),
                    github_head_sha: record.github_head_sha.as_deref(),
                    pr_url: record.pr_url.as_deref(),
                    failing_ci_job_ids: Some(&record.failing_ci_job_ids),
                    dossier: None,
                    directive: Some(&decision_json),
                    verification_command: None,
                    excluded_models: None,
                })
                .await
                .map_err(|e| {
                    format!("record_arbiter_decision: failed to update arbitration row: {e}")
                })?;

            // Mark consumed exactly once, mirroring the park/supersede paths.
            // An approve that leaves the row unconsumed wedges the task if it
            // later re-enters the second-strike path (e.g. a merge-conflict
            // reopen): the coordinator sees "arbiter already in flight" every
            // tick and dispatches nothing until the arbitration deadline
            // (incident lre2, 2026-07-16).
            let consumed = arb_repo
                .mark_consumed(&task_id, record.hold_cycle)
                .await
                .map_err(|e| format!("record_arbiter_decision: failed to mark consumed: {e}"))?;
            if !consumed {
                tracing::warn!(
                    task_id = %task.short_id,
                    hold_cycle = record.hold_cycle,
                    "record_arbiter_decision: arbitration row was already consumed"
                );
            }
        } else {
            // No unconsumed row — log and continue. The park transaction
            // creates its own row, but approve/approve_conflict are expected
            // to always have an unconsumed row from the dispatch.
            tracing::warn!(
                task_id = %task.short_id,
                "record_arbiter_decision: no unconsumed arbitration row found — decision logged as activity only"
            );
        }

        // Extract git-evidence fields from the arbitration row when available.
        let (mirror_head, github_head, pr_url_val, failing_ci) =
            if let Some(ref record) = unconsumed_record {
                (
                    record.mirror_head_sha.clone(),
                    record.github_head_sha.clone(),
                    record.pr_url.clone(),
                    record.failing_ci_job_ids.clone(),
                )
            } else {
                (None, None, None, serde_json::json!([]))
            };

        // Emit arbiter_decision activity event with git-evidence fields.
        let activity_payload = serde_json::json!({
            "event": "arbiter_decision",
            "task_id": task.short_id,
            "decision": decision,
            "evidence_summary": evidence_summary,
            "mirror_head_sha": mirror_head,
            "github_head_sha": github_head,
            "pr_url": pr_url_val,
            "failing_ci_job_ids": failing_ci,
        });
        if let Err(e) = task_repo
            .log_activity(
                Some(&task_id),
                "system",
                "coordinator",
                "arbiter_decision",
                &activity_payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "record_arbiter_decision: failed to log arbiter_decision activity"
            );
        }

        // Emit arbiter rollout telemetry: decision distribution.
        let telemetry_decision = match decision.as_str() {
            "approve" => djinn_telemetry::arbiter::DECISION_APPROVE,
            "approve_conflict" => djinn_telemetry::arbiter::DECISION_APPROVE_CONFLICT,
            _ => djinn_telemetry::arbiter::DECISION_APPROVE,
        };
        djinn_telemetry::arbiter::record_decision(telemetry_decision);

        // Emit time-in-arbitration when the record creation time is available.
        if let Some(ref record) = unconsumed_record
            && let Ok(created_at) = time::OffsetDateTime::parse(
                &record.created_at,
                &time::format_description::well_known::Rfc3339,
            )
        {
            let elapsed = (time::OffsetDateTime::now_utc() - created_at).as_seconds_f64();
            if elapsed >= 0.0 {
                djinn_telemetry::arbiter::record_time_in_arbitration(elapsed);
            }
        }

        Ok(())
    }

    /// Start a monitored-reopen worker attempt.  Persists the directive,
    /// verification command, and excluded models on the current unconsumed
    /// arbitration row, then atomically marks the attempt start via
    /// `record_monitored_reopen` so re-entry cannot inject the directive
    /// twice.  Emits an `arbiter_decision` activity event.
    async fn start_monitored_reopen(
        &self,
        task_id: String,
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    ) -> Result<(), String> {
        use djinn_db::TaskRepository;
        use djinn_db::repositories::task_arbitration::{
            TaskArbitrationRepository, UpdateDispatchLedgerParams,
        };

        let db = self.callbacks.agent_context.db.clone();
        let event_bus = self.callbacks.agent_context.event_bus.clone();
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let arb_repo = TaskArbitrationRepository::new(db.clone());

        // Load the source task for short_id (activity logging).
        let task = task_repo
            .get(&task_id)
            .await
            .map_err(|e| format!("start_monitored_reopen: failed to load task: {e}"))?
            .ok_or_else(|| format!("start_monitored_reopen: task {task_id} not found"))?;

        // Resolve the current unconsumed arbitration row.
        let (_hold_cycle, unconsumed_record) = arb_repo
            .resolve_current_hold_cycle(&task_id)
            .await
            .map_err(|e| format!("start_monitored_reopen: failed to resolve hold cycle: {e}"))?;

        // Build the structured payloads for the dispatch ledger update.
        let directive_json = serde_json::json!({
            "decision": "reopen",
            "directive": directive,
        });
        let excluded_json = serde_json::Value::Array(
            exclude_models
                .iter()
                .map(|m| serde_json::Value::String(m.clone()))
                .collect(),
        );

        if let Some(ref record) = unconsumed_record {
            // Persist directive / verification command / excluded models.
            arb_repo
                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                    task_id: &task_id,
                    hold_cycle: record.hold_cycle,
                    mirror_head_sha: None,
                    github_head_sha: None,
                    pr_url: None,
                    failing_ci_job_ids: None,
                    dossier: None,
                    directive: Some(&directive_json),
                    verification_command: Some(&verification_command),
                    excluded_models: Some(&excluded_json),
                })
                .await
                .map_err(|e| {
                    format!("start_monitored_reopen: failed to update arbitration row: {e}")
                })?;

            // Atomically mark the attempt start.  This increments
            // `monitored_reopen_count` and sets `monitored_reopen_at`.
            // Re-entry (a second worker dispatch for the same reopen)
            // will see `monitored_reopen_count >= 1` and NOT inject the
            // directive again.
            arb_repo
                .record_monitored_reopen(&task_id, record.hold_cycle)
                .await
                .map_err(|e| {
                    format!("start_monitored_reopen: failed to mark attempt start: {e}")
                })?;
        } else {
            // No unconsumed row — log and continue.  The directive will
            // not be injected since no arbitration row carries it.
            tracing::warn!(
                task_id = %task.short_id,
                "start_monitored_reopen: no unconsumed arbitration row found — directive not persisted"
            );
        }

        // Extract git-evidence fields from the arbitration row when available.
        let (mirror_head, github_head, pr_url_val, failing_ci, created_at_str, reopen_outcome) =
            if let Some(ref record) = unconsumed_record {
                (
                    record.mirror_head_sha.clone(),
                    record.github_head_sha.clone(),
                    record.pr_url.clone(),
                    record.failing_ci_job_ids.clone(),
                    record.created_at.clone(),
                    djinn_telemetry::arbiter::REOPEN_OUTCOME_STARTED,
                )
            } else {
                (
                    None,
                    None,
                    None,
                    serde_json::json!([]),
                    String::new(),
                    djinn_telemetry::arbiter::REOPEN_OUTCOME_NO_UNCONSUMED,
                )
            };

        // Emit arbiter_decision activity event with git-evidence fields.
        let activity_payload = serde_json::json!({
            "event": "arbiter_decision",
            "task_id": task.short_id,
            "decision": "reopen",
            "directive": directive,
            "mirror_head_sha": mirror_head,
            "github_head_sha": github_head,
            "pr_url": pr_url_val,
            "failing_ci_job_ids": failing_ci,
        });
        if let Err(e) = task_repo
            .log_activity(
                Some(&task_id),
                "system",
                "coordinator",
                "arbiter_decision",
                &activity_payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "start_monitored_reopen: failed to log arbiter_decision activity"
            );
        }

        // Emit arbiter rollout telemetry: decision + monitored reopen outcome.
        djinn_telemetry::arbiter::record_decision(djinn_telemetry::arbiter::DECISION_REOPEN);
        djinn_telemetry::arbiter::record_monitored_reopen(reopen_outcome);

        // Emit time-in-arbitration when the record creation time is available.
        if !created_at_str.is_empty()
            && let Ok(created_at) = time::OffsetDateTime::parse(
                &created_at_str,
                &time::format_description::well_known::Rfc3339,
            )
        {
            let elapsed = (time::OffsetDateTime::now_utc() - created_at).as_seconds_f64();
            if elapsed >= 0.0 {
                djinn_telemetry::arbiter::record_time_in_arbitration(elapsed);
            }
        }

        Ok(())
    }

    /// Mark the monitored-reopen attempt as complete.  Resolves the latest
    /// arbitration row for the task and, if it is unconsumed with a monitored
    /// reopen in progress (`monitored_reopen_count >= 1`), transitions it to
    /// `consumed`.  This is idempotent: a row already consumed/failed is a
    /// no-op.
    async fn complete_monitored_reopen(&self, task_id: String) -> Result<(), String> {
        use djinn_db::repositories::task_arbitration::TaskArbitrationRepository;

        let db = self.callbacks.agent_context.db.clone();
        let arb_repo = TaskArbitrationRepository::new(db);

        let latest = arb_repo.get_latest_for_task(&task_id).await.map_err(|e| {
            format!("complete_monitored_reopen: failed to load latest arbitration: {e}")
        })?;

        let Some(record) = latest else {
            return Ok(());
        };

        // Only complete when a monitored reopen is actually in progress.
        if record.monitored_reopen_count < 1 {
            return Ok(());
        }

        arb_repo
            .complete_monitored_reopen(&task_id, record.hold_cycle)
            .await
            .map_err(|e| format!("complete_monitored_reopen: failed to complete: {e}"))?;

        tracing::info!(
            task_id = %task_id,
            hold_cycle = record.hold_cycle,
            "complete_monitored_reopen: marked monitored reopen attempt complete"
        );

        Ok(())
    }

    async fn record_arbiter_session_termination(
        &self,
        task_id: String,
        is_infra_failure: bool,
    ) -> Result<bool, String> {
        use djinn_db::TaskRepository;
        use djinn_db::repositories::task_arbitration::TaskArbitrationRepository;

        let db = self.callbacks.agent_context.db.clone();
        let event_bus = self.callbacks.agent_context.event_bus.clone();
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let arb_repo = TaskArbitrationRepository::new(db.clone());

        // Load the source task for short_id (activity logging).
        let task = task_repo
            .get(&task_id)
            .await
            .map_err(|e| format!("record_arbiter_session_termination: failed to load task: {e}"))?
            .ok_or_else(|| {
                format!("record_arbiter_session_termination: task {task_id} not found")
            })?;

        // Load the latest arbitration for this task.
        let latest = arb_repo.get_latest_for_task(&task_id).await.map_err(|e| {
            format!("record_arbiter_session_termination: failed to load latest arbitration: {e}")
        })?;

        let Some(record) = latest else {
            // No arbitration row — nothing to account.
            tracing::debug!(
                task_id = %task.short_id,
                "record_arbiter_session_termination: no arbitration row found; skipping"
            );
            return Ok(false);
        };

        // Do not mutate accounting for consumed (decision already accepted)
        // or failed (terminal) arbitrations.
        if record.state != "unconsumed" {
            tracing::debug!(
                task_id = %task.short_id,
                hold_cycle = record.hold_cycle,
                state = %record.state,
                "record_arbiter_session_termination: arbitration is not unconsumed; skipping accounting"
            );
            return Ok(false);
        }

        if is_infra_failure {
            // Infra-class failures before a decision increment only infra
            // observability — they do not count as bad arbiter decisions.
            let _ = arb_repo
                .increment_infra_retry(&task_id, record.hold_cycle)
                .await
                .map_err(|e| {
                    format!(
                        "record_arbiter_session_termination: failed to increment infra retry: {e}"
                    )
                });
            tracing::info!(
                task_id = %task.short_id,
                hold_cycle = record.hold_cycle,
                infra_retry_count = record.infra_retry_count + 1,
                "record_arbiter_session_termination: infra failure recorded"
            );
            djinn_telemetry::arbiter::record_termination(
                djinn_telemetry::arbiter::TERMINATION_INFRA,
            );
            return Ok(false);
        }

        // No-valid-decision failure: the session ran and ended without
        // calling submit_decision.  Increment decision_failure_count.
        let _ = arb_repo
            .increment_decision_failure(&task_id, record.hold_cycle)
            .await
            .map_err(|e| {
                format!(
                    "record_arbiter_session_termination: failed to increment decision failure: {e}"
                )
            });

        let new_count = record.decision_failure_count + 1;
        tracing::info!(
            task_id = %task.short_id,
            hold_cycle = record.hold_cycle,
            decision_failure_count = new_count,
            "record_arbiter_session_termination: no-decision failure recorded"
        );

        // Decision-failure cap: at 2, mark the arbitration as failed and
        // park the task behind HumanReview with a generated dossier.
        const DECISION_FAILURE_CAP: i32 = 2;
        if new_count >= DECISION_FAILURE_CAP {
            // Mark the arbitration as failed (terminal for this hold cycle).
            let _ = arb_repo
                .mark_failed(&task_id, record.hold_cycle)
                .await
                .map_err(|e| {
                    format!(
                        "record_arbiter_session_termination: failed to mark arbitration failed: {e}"
                    )
                });

            // Generate an arbiter-failure dossier with explicit git-evidence
            // fields sourced from the existing arbitration row.
            let dossier = serde_json::json!({
                "kind": "arbiter_decision_failure_cap",
                "summary": format!(
                    "Arbiter session terminated without a valid decision {} times for hold cycle {}; \
                     parking behind HumanReview.  No further arbiter dispatch for this hold cycle.",
                    new_count, record.hold_cycle,
                ),
                "task_id": task.short_id,
                "hold_cycle": record.hold_cycle,
                "decision_failure_count": new_count,
                "infra_retry_count": record.infra_retry_count,
                "deadline_at": record.deadline_at,
                "mirror_head_sha": record.mirror_head_sha,
                "github_head_sha": record.github_head_sha,
                "pr_url": record.pr_url,
                "failing_ci_job_ids": record.failing_ci_job_ids,
            });

            // Update the arbitration row with the dossier and git-evidence.
            use djinn_db::repositories::task_arbitration::UpdateDispatchLedgerParams;
            let _ = arb_repo
                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                    task_id: &task_id,
                    hold_cycle: record.hold_cycle,
                    mirror_head_sha: record.mirror_head_sha.as_deref(),
                    github_head_sha: record.github_head_sha.as_deref(),
                    pr_url: record.pr_url.as_deref(),
                    failing_ci_job_ids: Some(&record.failing_ci_job_ids),
                    dossier: Some(&dossier),
                    directive: None,
                    verification_command: None,
                    excluded_models: None,
                })
                .await
                .map_err(|e| {
                    format!("record_arbiter_session_termination: failed to update dossier: {e}")
                });

            // Emit arbiter_decision activity with git-evidence fields for
            // the decision-failure cap path.
            let failure_payload = serde_json::json!({
                "event": "arbiter_decision",
                "task_id": task.short_id,
                "decision": "park",
                "autonomous_escalation": true,
                "reason": "decision_failure_cap",
                "hold_cycle": record.hold_cycle,
                "decision_failure_count": new_count,
                "mirror_head_sha": record.mirror_head_sha,
                "github_head_sha": record.github_head_sha,
                "pr_url": record.pr_url,
                "failing_ci_job_ids": record.failing_ci_job_ids,
            });
            if let Err(e) = task_repo
                .log_activity(
                    Some(&task_id),
                    "system",
                    "coordinator",
                    "arbiter_decision",
                    &failure_payload.to_string(),
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "record_arbiter_session_termination: failed to log arbiter_decision activity"
                );
            }

            // Create the autonomous planner escalation (reuses the arbiter-park
            // escalation creation logic so the source is blocked before it
            // lands at `open`). The arbiter capping out on decision failures is
            // NOT a reason to strand the task on a human — the Planner takes
            // terminal ownership exactly as it does for an explicit park.
            let dossier_summary = dossier
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("arbiter decision-failure cap reached");
            if let Err(e) = self
                .create_arbiter_planner_escalation(
                    &task_id,
                    &task.project_id,
                    &dossier,
                    dossier_summary,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "record_arbiter_session_termination: failed to create planner escalation"
                );
            }

            // Transition the task: in_lead_intervention → open.
            if let Err(e) = task_repo
                .transition(
                    &task_id,
                    djinn_core::models::TransitionAction::ArbiterPark,
                    "system",
                    "coordinator",
                    Some(&dossier.to_string()),
                    None,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "record_arbiter_session_termination: arbiter_park transition failed — \
                     task remains in_lead_intervention"
                );
            }

            // Log the parking activity.
            let activity_payload = serde_json::json!({
                "event": "arbiter_decision_failure_parked",
                "task_id": task.short_id,
                "hold_cycle": record.hold_cycle,
                "decision_failure_count": new_count,
            });
            if let Err(e) = task_repo
                .log_activity(
                    Some(&task_id),
                    "system",
                    "coordinator",
                    "arbiter_decision_failure_parked",
                    &activity_payload.to_string(),
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "record_arbiter_session_termination: failed to log parking activity"
                );
            }

            tracing::warn!(
                task_id = %task.short_id,
                hold_cycle = record.hold_cycle,
                decision_failure_count = new_count,
                "record_arbiter_session_termination: decision-failure cap reached; arbiter parked"
            );
            djinn_telemetry::arbiter::record_termination(
                djinn_telemetry::arbiter::TERMINATION_DECISION_FAILURE,
            );
            djinn_telemetry::arbiter::record_park(
                djinn_telemetry::arbiter::PARK_REASON_DECISION_FAILURE_CAP,
                djinn_telemetry::arbiter::PARK_OUTCOME_SUCCESS,
            );
            return Ok(true);
        }

        Ok(false)
    }

    async fn publish_branch_to_github(
        &self,
        spec: &TaskRunSpec,
        _task: &Task,
    ) -> BranchPublicationResult {
        // Push the task branch to GitHub for a task with an existing open PR,
        // so that GitHub Actions evaluates the worker's latest commit instead
        // of a stale PR head (the aah4 stale-head condition).
        //
        // This explicitly reuses `push_task_branch_to_github` and its
        // concurrent-push race guard (`is_concurrent_push_race`) rather than
        // creating a second GitHub writer — consistent push semantics and
        // race handling across the codebase.
        //
        // Cross-references: epic vy47, proposal icoe acceptance criteria 4
        // (open-PR WorkerDone mirror commits pushed to GitHub immediately),
        // 5 (GitHub push failure structured activity), 7 (supervisor-level
        // junk-free alignment regression), 8 (branch-publication policy
        // approval + helper reuse evidence).
        use crate::supervisor_impl::pr::push_task_branch_to_github;
        use crate::task_merge::build_app_push_url;
        use djinn_db::ProjectRepository;
        use djinn_git::run_git_command;
        use djinn_provider::github_app::{
            app_id as github_app_id, installations::get_installation_token,
        };

        // Pre-populate the fields we'll always fill in.
        let empty_result =
            |success: bool, error_class: Option<String>, error_message: Option<String>| {
                BranchPublicationResult {
                    success,
                    pushed_sha: None,
                    mirror_head: String::new(),
                    attempted_github_head: String::new(),
                    pr_branch_existed: false,
                    error_class,
                    error_message,
                }
            };

        // GitHub App must be configured.
        if github_app_id().is_err() {
            return empty_result(
                false,
                Some("no_github_app".into()),
                Some("GitHub App is not configured on this deployment".into()),
            );
        }

        let app_state = &self.callbacks.agent_context;
        let mirror = match app_state.mirror.as_ref() {
            Some(m) => m.clone(),
            None => {
                return empty_result(
                    false,
                    Some("no_mirror".into()),
                    Some("AgentContext has no MirrorManager".into()),
                );
            }
        };

        let project_repo =
            ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());

        // Resolve GitHub coords.
        let (owner, repo_name) = match project_repo.get_github_coords(&spec.project_id).await {
            Ok(Some(coords)) => coords,
            Ok(None) => {
                return empty_result(
                    false,
                    Some("no_github_coords".into()),
                    Some(format!(
                        "project {} has no github_owner/github_repo persisted",
                        spec.project_id
                    )),
                );
            }
            Err(e) => {
                return empty_result(
                    false,
                    Some("db_error".into()),
                    Some(format!(
                        "failed to read github coords for project {}: {e}",
                        spec.project_id
                    )),
                );
            }
        };

        // Resolve installation token.
        let installation_id = match project_repo.get_installation_id(&spec.project_id).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return empty_result(
                    false,
                    Some("no_installation_id".into()),
                    Some(format!(
                        "project {} ({}/{}) has no cached installation_id",
                        spec.project_id, owner, repo_name
                    )),
                );
            }
            Err(e) => {
                return empty_result(
                    false,
                    Some("db_error".into()),
                    Some(format!(
                        "failed to read installation_id for project {}: {e}",
                        spec.project_id
                    )),
                );
            }
        };

        let install_token = match get_installation_token(installation_id).await {
            Ok(t) => t,
            Err(e) => {
                return empty_result(
                    false,
                    Some("auth".into()),
                    Some(format!("could not mint installation token: {e}")),
                );
            }
        };
        let push_url = build_app_push_url(&owner, &repo_name, &install_token.token);

        // Capture mirror HEAD from the bare mirror for structured failure
        // reporting.  Use the same rev-parse pattern as
        // `MirrorManager::branch_ahead_of_base`.
        let mirror_path = mirror.mirror_path(&spec.project_id);
        let mirror_head = match run_git_command(
            mirror_path.clone(),
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/heads/{}", spec.task_branch),
            ],
        )
        .await
        {
            Ok(out) => out.stdout.trim().to_string(),
            Err(e) => {
                return empty_result(
                    false,
                    Some("mirror_error".into()),
                    Some(format!(
                        "failed to resolve mirror HEAD for {}: {e}",
                        spec.task_branch
                    )),
                );
            }
        };

        // Check if the PR branch already exists on GitHub via ls-remote
        // against the push URL.
        let pr_branch_existed = match run_git_command(
            mirror_path,
            vec![
                "ls-remote".into(),
                push_url.clone(),
                format!("refs/heads/{}", spec.task_branch),
            ],
        )
        .await
        {
            Ok(out) => !out.stdout.trim().is_empty(),
            Err(_) => false,
        };

        // Delegate to the existing push helper — reuses the concurrent-push
        // race guard, ephemeral clone, and force-push logic.
        match push_task_branch_to_github(
            mirror.as_ref(),
            &spec.project_id,
            &spec.task_branch,
            &push_url,
        )
        .await
        {
            Ok(sha) => BranchPublicationResult {
                success: true,
                pushed_sha: Some(sha.clone()),
                mirror_head,
                attempted_github_head: sha,
                pr_branch_existed,
                error_class: None,
                error_message: None,
            },
            Err(e) => {
                let error_class = if e.to_string().contains("rejected") {
                    "push_rejected"
                } else {
                    "push_error"
                };
                tracing::warn!(
                    task_branch = %spec.task_branch,
                    project_id = %spec.project_id,
                    error = %e,
                    "publish_branch_to_github: push failed"
                );
                BranchPublicationResult {
                    success: false,
                    pushed_sha: None,
                    mirror_head: mirror_head.clone(),
                    attempted_github_head: mirror_head,
                    pr_branch_existed,
                    error_class: Some(error_class.into()),
                    error_message: Some(e.to_string()),
                }
            }
        }
    }
}

/// Result of collecting one attributed planner provider stream.  Keeping this
/// boundary separate makes the host timeout contract testable without a live
/// credential or network provider.
struct CollectedPlannerStream {
    response: LlmResponse,
    outcome: PlannerOutcome,
    diagnostic: Option<String>,
    completed: bool,
}

/// Collect the entire provider stream under one deadline.  Usage is stored in
/// the response as each event arrives, deliberately before awaiting the next
/// event, so late errors and collection timeouts retain attempted usage.
async fn collect_planner_stream(
    provider: &dyn LlmProvider,
    conversation: &Conversation,
    tools: &[serde_json::Value],
    tool_choice: Option<ToolChoice>,
    timeout_ms: u64,
) -> CollectedPlannerStream {
    let mut response = LlmResponse {
        content: Vec::new(),
        thinking: String::new(),
        usage: TokenUsage::default(),
    };
    let collection = async {
        let mut stream = provider
            .stream(conversation, tools, tool_choice)
            .await
            .map_err(|e| format!("provider stream init failed: {e}"))?;
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    if append_direct_response_event(&mut response, event) {
                        break;
                    }
                }
                Err(error) => return Err(format!("provider stream error: {error}")),
            }
        }
        Ok::<(), String>(())
    };

    match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms.max(1)),
        collection,
    )
    .await
    {
        Ok(Ok(())) => CollectedPlannerStream {
            response,
            outcome: PlannerOutcome::Success,
            diagnostic: None,
            completed: true,
        },
        Ok(Err(error)) => CollectedPlannerStream {
            response,
            outcome: PlannerOutcome::ProviderError,
            diagnostic: Some(error),
            completed: false,
        },
        Err(_) => CollectedPlannerStream {
            response,
            outcome: PlannerOutcome::Timeout,
            diagnostic: Some(format!("planner call timed out after {timeout_ms}ms")),
            completed: false,
        },
    }
}

/// Production attributed-planner stream collector exposed solely for
/// behavioral integration tests with a scripted provider. The tuple preserves
/// the collector's observable response and terminal classification without
/// exposing its private bookkeeping struct as public API.
#[doc(hidden)]
pub async fn collect_planner_stream_for_test(
    provider: &dyn LlmProvider,
    conversation: &Conversation,
    tools: &[serde_json::Value],
    tool_choice: Option<ToolChoice>,
    timeout_ms: u64,
) -> (LlmResponse, PlannerOutcome, Option<String>, bool) {
    let collected =
        collect_planner_stream(provider, conversation, tools, tool_choice, timeout_ms).await;
    (
        collected.response,
        collected.outcome,
        collected.diagnostic,
        collected.completed,
    )
}

/// Intern the wire form's `(entity_type, action)` back into the static-str
/// shape `DjinnEventEnvelope` expects.
///
/// The set of distinct pairs the worker can emit is bounded (see
/// `event_bus.send(..)` call sites in `actors::slot::reply_loop` /
/// `streaming` / `lifecycle/setup` /
/// `lifecycle/model_resolution`). Unknown pairs return `Err((entity_type,
/// action))` so the caller can log + drop rather than leaking strings into
/// the static lifetime.
fn intern_envelope(wire: SerializableDjinnEvent) -> Result<DjinnEventEnvelope, (String, String)> {
    let SerializableDjinnEvent {
        entity_type,
        action,
        payload,
        id,
        project_id,
    } = wire;
    let (et, ac): (&'static str, &'static str) = match (entity_type.as_str(), action.as_str()) {
        ("session", "message") => ("session", "message"),
        ("session", "token_update") => ("session", "token_update"),
        ("session", "dispatched") => ("session", "dispatched"),
        ("lifecycle", "step") => ("lifecycle", "step"),
        ("activity", "logged") => ("activity", "logged"),
        ("task", "updated") => ("task", "updated"),
        ("task", "created") => ("task", "created"),
        ("task", "deleted") => ("task", "deleted"),
        // Epics created/updated by a worker-pod agent (the proposal-decomposition
        // planner, Mode D) must reach the host coordinator so wave-1 breakdown
        // fires immediately. Without these arms the events were dropped at the
        // RPC boundary and epics only broke down on the 15-min stale sweep.
        ("epic", "created") => ("epic", "created"),
        ("epic", "updated") => ("epic", "updated"),
        // Proposal events emitted by worker-pod agents must reach the host so
        // live proposal-detail UI stays fresh.  Only the exact pairs listed
        // here are allowlisted; debate-trail events remain unregistered and
        // continue to produce the unknown-pair drift signal.
        ("proposal", "created") => ("proposal", "created"),
        ("proposal", "updated") => ("proposal", "updated"),
        ("proposal", "deleted") => ("proposal", "deleted"),
        ("proposal_feedback", "created") => ("proposal_feedback", "created"),
        _ => return Err((entity_type, action)),
    };
    // `payload` crosses the wire as an opaque JSON string — bincode can't
    // round-trip `serde_json::Value`'s untagged-enum representation. Re-parse
    // here so downstream SSE subscribers see a real `Value` again. A
    // malformed string (shouldn't happen in practice — the producer uses
    // `serde_json::to_string` on a `Value`) degrades to `Value::Null` rather
    // than dropping the whole event.
    let payload = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
    Ok(DjinnEventEnvelope {
        entity_type: et,
        action: ac,
        payload,
        id,
        project_id,
        from_sync: false,
    })
}

/// Determine the session `cost_basis` value from the explicit credential
/// billing hint, catalog pricing availability, and (for the legacy fallback
/// path) provider classification.
///
/// Precedence:
/// 1. `SubscriptionPlan` hint → `"projected"`
/// 2. `MeteredApi` hint → priced `"actual"` / unpriced `"unpriced"`
/// 3. No hint → legacy `classify_provider` + pricing availability
///
/// This is extracted as a pure function so the decision logic can be tested
/// without instantiating a `DirectServices` / database.
pub(crate) fn determine_cost_basis(
    hint: Option<CostBasisHint>,
    pricing: Option<&djinn_core::models::Pricing>,
    provider_id: Option<&str>,
) -> &'static str {
    match hint {
        Some(CostBasisHint::SubscriptionPlan) => "projected",
        Some(CostBasisHint::MeteredApi) => {
            if pricing.is_some() {
                "actual"
            } else {
                "unpriced"
            }
        }
        None => {
            // Legacy path: classify by provider id alone.
            match (provider_id, pricing) {
                (Some(pid), Some(_)) => {
                    if classify_provider(pid).is_subscription() {
                        "projected"
                    } else {
                        "actual"
                    }
                }
                _ => "unpriced",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CostBasisHint, DirectServices, PlannerAttemptLedger, PlannerLedgerCreate,
        PlannerLedgerFinalize, PlannerLedgerFinalized, SerializableDjinnEvent,
        determine_cost_basis, intern_envelope,
    };
    use async_trait::async_trait;
    use djinn_core::models::Pricing;
    use djinn_provider::message::{ContentBlock, Conversation};
    use djinn_provider::provider::{LlmProvider, StreamEvent, TokenUsage, ToolChoice};
    use futures::StreamExt;
    use std::pin::Pin;

    /// Helper: a non-zero pricing snapshot (used to represent a priced model).
    fn priced() -> Pricing {
        Pricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: 0.5,
            cache_write_per_million: 0.0,
        }
    }

    // ── CostBasisHint precedence tests ──────────────────────────────────

    /// Codex OAuth model on the openai namespace → `SubscriptionPlan` hint →
    /// `"projected"` even though `openai` is an API-key provider.
    /// This is the core forward-fix regression.
    #[test]
    fn cost_basis_codex_oauth_on_openai_gives_projected() {
        let hint = Some(CostBasisHint::SubscriptionPlan);
        assert_eq!(
            determine_cost_basis(hint, Some(&priced()), Some("openai")),
            "projected"
        );
    }

    /// Coding-plan / token-plan provider id pattern → `SubscriptionPlan` →
    /// `"projected"`.
    #[test]
    fn cost_basis_coding_plan_provider_gives_projected() {
        let hint = Some(CostBasisHint::SubscriptionPlan);
        assert_eq!(
            determine_cost_basis(hint, Some(&priced()), Some("xiaomi-token-plan-sgp")),
            "projected"
        );
    }

    /// Priced metered API-key provider → `MeteredApi` + pricing present →
    /// `"actual"`.
    #[test]
    fn cost_basis_metered_api_key_priced_gives_actual() {
        let hint = Some(CostBasisHint::MeteredApi);
        assert_eq!(
            determine_cost_basis(hint, Some(&priced()), Some("anthropic")),
            "actual"
        );
    }

    /// Uncatalogued or missing-pricing model with metered hint → `"unpriced"`.
    #[test]
    fn cost_basis_metered_unpriced_gives_unpriced() {
        let hint = Some(CostBasisHint::MeteredApi);
        assert_eq!(
            determine_cost_basis(hint, None, Some("unknown-provider")),
            "unpriced"
        );
        // Also covers the case where no provider_id is present at all.
        assert_eq!(determine_cost_basis(hint, None, None), "unpriced");
    }

    /// OAuth transport alone (without subscription evidence) must NOT blanket
    /// produce `"projected"`.  When stage.rs sees a non-subscription provider
    /// and no Codex marker, it emits `MeteredApi`.  A priced metered session
    /// stays `"actual"`.
    #[test]
    fn cost_basis_oauth_transport_alone_does_not_cause_projected() {
        // Simulates a hypothetical OAuth-backed metered provider that is NOT
        // Codex / subscription / coding-plan. The stage hint is MeteredApi.
        let hint = Some(CostBasisHint::MeteredApi);
        assert_eq!(
            determine_cost_basis(hint, Some(&priced()), Some("openai")),
            "actual"
        );
        // Uncatalogued variant — still not projected.
        assert_eq!(
            determine_cost_basis(hint, None, Some("custom-oauth-provider")),
            "unpriced"
        );
    }

    // ── Legacy fallback (no hint) ───────────────────────────────────────

    /// No hint + subscription provider + pricing → `"projected"`.
    #[test]
    fn cost_basis_legacy_subscription_provider_with_pricing() {
        assert_eq!(
            determine_cost_basis(None, Some(&priced()), Some("minimax-coding-plan")),
            "projected"
        );
    }

    /// No hint + API-key provider + pricing → `"actual"`.
    #[test]
    fn cost_basis_legacy_api_key_provider_with_pricing() {
        assert_eq!(
            determine_cost_basis(None, Some(&priced()), Some("openai")),
            "actual"
        );
    }

    /// No hint + no model/pricing → `"unpriced"`.
    #[test]
    fn cost_basis_legacy_no_model_is_unpriced() {
        assert_eq!(determine_cost_basis(None, None, None), "unpriced");
        assert_eq!(determine_cost_basis(None, None, Some("openai")), "unpriced");
    }

    // ── Existing intern_envelope tests (unchanged) ─────────────────────

    #[test]
    fn intern_envelope_forwards_worker_epic_events() {
        // Regression: worker-pod (Mode D) epic creates/updates must cross the
        // RPC boundary so the host coordinator fires wave-1 breakdown.
        for action in ["created", "updated"] {
            let wire = SerializableDjinnEvent {
                entity_type: "epic".into(),
                action: action.into(),
                payload: serde_json::json!({"id": "e1", "status": "open"}).to_string(),
                id: Some("e1".into()),
                project_id: Some("p1".into()),
            };
            let env = intern_envelope(wire).expect("epic events must be whitelisted");
            assert_eq!(env.entity_type, "epic");
            assert_eq!(env.action, action);
            assert_eq!(env.payload["status"], "open");
        }
    }

    #[test]
    fn intern_envelope_session_message_keeps_static_strs() {
        let wire = SerializableDjinnEvent {
            entity_type: "session".into(),
            action: "message".into(),
            payload: serde_json::json!({"role": "assistant"}).to_string(),
            id: None,
            project_id: Some("p1".into()),
        };
        let env = intern_envelope(wire).expect("known pair");
        assert_eq!(env.entity_type, "session");
        assert_eq!(env.action, "message");
        assert_eq!(env.payload["role"], "assistant");
        // `entity_type` / `action` are `&'static str` from the match arms
        // by type (see `DjinnEventEnvelope`), so the conversion away from
        // owned `String` is enforced by the type system — no runtime
        // pointer-eq check needed. (We used to assert
        // `std::ptr::eq(env.entity_type.as_ptr(), "session".as_ptr())` here
        // but cross-crate string-literal deduplication isn't a language
        // guarantee, only an LTO heuristic.)
    }

    #[test]
    fn intern_envelope_unknown_pair_errors() {
        let wire = SerializableDjinnEvent {
            entity_type: "unknown_entity".into(),
            action: "weird_action".into(),
            payload: serde_json::Value::Null.to_string(),
            id: None,
            project_id: None,
        };
        let err = intern_envelope(wire).expect_err("unknown pair must error");
        assert_eq!(err, ("unknown_entity".into(), "weird_action".into()));
    }

    // ── Proposal / proposal_feedback allowlist tests ────────────────────

    #[test]
    fn intern_envelope_forwards_proposal_created() {
        let wire = SerializableDjinnEvent {
            entity_type: "proposal".into(),
            action: "created".into(),
            payload: serde_json::json!({"id": "p1", "title": "Add feature"}).to_string(),
            id: Some("p1".into()),
            project_id: Some("proj1".into()),
        };
        let env = intern_envelope(wire).expect("proposal.created must be allowlisted");
        assert_eq!(env.entity_type, "proposal");
        assert_eq!(env.action, "created");
        assert_eq!(env.payload["title"], "Add feature");
        assert_eq!(env.id, Some("p1".into()));
        assert_eq!(env.project_id, Some("proj1".into()));
    }

    #[test]
    fn intern_envelope_forwards_proposal_updated() {
        let wire = SerializableDjinnEvent {
            entity_type: "proposal".into(),
            action: "updated".into(),
            payload: serde_json::json!({"id": "p1", "status": "review"}).to_string(),
            id: Some("p1".into()),
            project_id: Some("proj1".into()),
        };
        let env = intern_envelope(wire).expect("proposal.updated must be allowlisted");
        assert_eq!(env.entity_type, "proposal");
        assert_eq!(env.action, "updated");
        assert_eq!(env.payload["status"], "review");
        assert_eq!(env.id, Some("p1".into()));
        assert_eq!(env.project_id, Some("proj1".into()));
    }

    #[test]
    fn intern_envelope_forwards_proposal_deleted() {
        let wire = SerializableDjinnEvent {
            entity_type: "proposal".into(),
            action: "deleted".into(),
            payload: serde_json::json!({"id": "p1"}).to_string(),
            id: Some("p1".into()),
            project_id: Some("proj1".into()),
        };
        let env = intern_envelope(wire).expect("proposal.deleted must be allowlisted");
        assert_eq!(env.entity_type, "proposal");
        assert_eq!(env.action, "deleted");
        assert_eq!(env.id, Some("p1".into()));
        assert_eq!(env.project_id, Some("proj1".into()));
    }

    #[test]
    fn intern_envelope_forwards_proposal_feedback_created() {
        let wire = SerializableDjinnEvent {
            entity_type: "proposal_feedback".into(),
            action: "created".into(),
            payload: serde_json::json!({"id": "f1", "proposal_id": "p1", "body": "LGTM"})
                .to_string(),
            id: Some("f1".into()),
            project_id: Some("proj1".into()),
        };
        let env = intern_envelope(wire).expect("proposal_feedback.created must be allowlisted");
        assert_eq!(env.entity_type, "proposal_feedback");
        assert_eq!(env.action, "created");
        assert_eq!(env.payload["body"], "LGTM");
        assert_eq!(env.id, Some("f1".into()));
        assert_eq!(env.project_id, Some("proj1".into()));
    }

    #[test]
    fn intern_envelope_rejects_proposal_debate_trail_created() {
        let wire = SerializableDjinnEvent {
            entity_type: "proposal_debate_trail".into(),
            action: "created".into(),
            payload: serde_json::Value::Null.to_string(),
            id: None,
            project_id: None,
        };
        let err = intern_envelope(wire)
            .expect_err("proposal_debate_trail.created must not be registered");
        assert_eq!(err, ("proposal_debate_trail".into(), "created".into()));
    }

    #[test]
    fn intern_envelope_rejects_proposal_debate_trail_updated() {
        let wire = SerializableDjinnEvent {
            entity_type: "proposal_debate_trail".into(),
            action: "updated".into(),
            payload: serde_json::Value::Null.to_string(),
            id: None,
            project_id: None,
        };
        let err = intern_envelope(wire)
            .expect_err("proposal_debate_trail.updated must not be registered");
        assert_eq!(err, ("proposal_debate_trail".into(), "updated".into()));
    }

    // ── Representative combined routing regression (i2ef Task D) ───────
    //
    // Proves the worker-side ignored-pair filter and host-side
    // `intern_envelope` allowlist work together correctly across the three
    // event routing groups.  The canonical `worker_bridge_ignores_pair`
    // lives in `djinn-agent-worker`; a local mirror captures the intended
    // contract for regression testing at the host seam without creating a
    // cross-crate test dependency.

    /// Mirror of `djinn-agent-worker::worker_bridge_ignores_pair`.
    /// Keeps the canonical set in sync with the worker crate.
    fn host_mirror_worker_ignores_pair(entity_type: &str, action: &str) -> bool {
        matches!(
            (entity_type, action),
            ("session_message", "inserted")
                | ("note", "created")
                | ("note", "updated")
                | ("note", "contradiction_candidates")
        )
    }

    /// Combined routing regression: ignored pairs are filtered before the
    /// host unknown-pair warning path, proposal/proposal_feedback pairs are
    /// accepted by `intern_envelope`, and unregistered non-filtered pairs
    /// produce the host drift signal.
    ///
    /// Covers i2ef acceptance criteria A (ignored), B (accepted), and C
    /// (visible drift) in a single table-driven test.
    #[test]
    fn worker_to_host_event_routing_regression() {
        // ── Group 1: Ignored before host ──────────────────────────────
        // These pairs are filtered worker-side and should never reach
        // `intern_envelope` / the unknown-pair warning path.
        let ignored_pairs: &[(&str, &str)] = &[
            ("session_message", "inserted"),
            ("note", "created"),
            ("note", "updated"),
            ("note", "contradiction_candidates"),
        ];
        for &(et, ac) in ignored_pairs {
            assert!(
                host_mirror_worker_ignores_pair(et, ac),
                "{et}.{ac} must be filtered by worker bridge before reaching host"
            );
        }

        // ── Group 2: Accepted by host without unknown-pair warning ────
        // These pairs pass the worker filter and are allowlisted in
        // `intern_envelope`, so no drift warning is emitted.
        let accepted_pairs: &[(&str, &str)] = &[
            ("proposal", "created"),
            ("proposal", "updated"),
            ("proposal", "deleted"),
            ("proposal_feedback", "created"),
        ];
        for &(et, ac) in accepted_pairs {
            assert!(
                !host_mirror_worker_ignores_pair(et, ac),
                "{et}.{ac} must NOT be filtered by worker bridge"
            );
            let wire = SerializableDjinnEvent {
                entity_type: et.into(),
                action: ac.into(),
                payload: serde_json::json!({}).to_string(),
                id: Some("test-id".into()),
                project_id: Some("test-proj".into()),
            };
            let env = intern_envelope(wire).unwrap_or_else(|_| {
                panic!("{et}.{ac} must be accepted by host (no drift warning)")
            });
            assert_eq!(env.entity_type, et);
            assert_eq!(env.action, ac);
        }

        // ── Group 3: Visible drift — unexpected non-filtered pair ─────
        // These pass the worker filter (not ignored) but are NOT
        // allowlisted at the host, so `intern_envelope` returns `Err`
        // and the caller emits the drift warning.
        let drift_pairs: &[(&str, &str)] = &[
            ("proposal_debate_trail", "created"),
            ("proposal_debate_trail", "updated"),
        ];
        for &(et, ac) in drift_pairs {
            assert!(
                !host_mirror_worker_ignores_pair(et, ac),
                "{et}.{ac} must pass worker filter to surface as host drift"
            );
            let wire = SerializableDjinnEvent {
                entity_type: et.into(),
                action: ac.into(),
                payload: serde_json::Value::Null.to_string(),
                id: None,
                project_id: None,
            };
            let (err_et, err_ac) =
                intern_envelope(wire).expect_err("{et}.{ac} must produce host drift signal (Err)");
            assert_eq!(err_et, et, "drift error entity_type must match input");
            assert_eq!(err_ac, ac, "drift error action must match input");
        }
    }

    struct PlannerStreamProvider {
        events: Vec<Result<StreamEvent, String>>,
        hang_after: bool,
    }

    impl LlmProvider for PlannerStreamProvider {
        fn name(&self) -> &str {
            "planner-stream-test"
        }
        fn stream<'a>(
            &'a self,
            _: &'a Conversation,
            _: &'a [serde_json::Value],
            _: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let events = self
                .events
                .clone()
                .into_iter()
                .map(|event| event.map_err(anyhow::Error::msg))
                .collect::<Vec<_>>();
            let hang_after = self.hang_after;
            Box::pin(async move {
                let stream: Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                > = if hang_after {
                    Box::pin(futures::stream::iter(events).chain(futures::stream::pending()))
                } else {
                    Box::pin(futures::stream::iter(events))
                };
                Ok(stream)
            })
        }
    }

    #[derive(Default)]
    struct TestPlannerLedger {
        created: std::sync::Mutex<Vec<PlannerLedgerCreate>>,
        finalized: std::sync::Mutex<Vec<PlannerLedgerFinalize>>,
        fail_finalize: bool,
    }

    #[async_trait]
    impl PlannerAttemptLedger for TestPlannerLedger {
        async fn create(&self, params: PlannerLedgerCreate) -> Result<(), String> {
            self.created.lock().unwrap().push(params);
            Ok(())
        }
        async fn finalize(
            &self,
            params: PlannerLedgerFinalize,
        ) -> Result<PlannerLedgerFinalized, String> {
            if self.fail_finalize {
                return Err("forced finalization failure".into());
            }
            self.finalized.lock().unwrap().push(params.clone());
            Ok(PlannerLedgerFinalized {
                tokens_in: params.tokens_in,
                tokens_out: params.tokens_out,
                cache_read_tokens: params.cache_read_tokens,
                cache_write_tokens: params.cache_write_tokens,
                cost_usd: Some(0.001),
                diagnostic: params.diagnostic,
            })
        }
    }
    fn request(timeout_ms: u64) -> djinn_supervisor::services::wire::AttributedPlannerRequest {
        djinn_supervisor::services::wire::AttributedPlannerRequest {
            project_id: "project-1".into(),
            task_id: "task-1".into(),
            task_run_id: "run-1".into(),
            session_id: "session-1".into(),
            created_by_user_id: "creator-1".into(),
            operation: "memory_intent_planner".into(),
            prompt_id: "memory-intent-planner-v1".into(),
            conversation: serde_json::to_string(&Conversation::default()).unwrap(),
            tools: "[]".into(),
            tool_choice: None,
            max_tokens: 100,
            timeout_ms,
        }
    }
    async fn run(
        events: Vec<Result<StreamEvent, String>>,
        hang_after: bool,
        fail_finalize: bool,
        timeout_ms: u64,
    ) -> (
        djinn_supervisor::services::wire::PlannerAttemptResult,
        std::sync::Arc<TestPlannerLedger>,
    ) {
        use djinn_supervisor::SupervisorServices;
        let provider = std::sync::Arc::new(PlannerStreamProvider { events, hang_after });
        let ledger = std::sync::Arc::new(TestPlannerLedger {
            fail_finalize,
            ..Default::default()
        });
        let ctx = crate::test_helpers::agent_context_from_db(
            crate::test_helpers::create_test_db(),
            tokio_util::sync::CancellationToken::new(),
        );
        let services = DirectServices::with_planner_test_seam(ctx, provider, ledger.clone());
        (
            SupervisorServices::plan_memory_intents(&services, request(timeout_ms))
                .await
                .unwrap(),
            ledger,
        )
    }
    fn usage() -> TokenUsage {
        TokenUsage {
            input: 11,
            output: 7,
            cache_read: 3,
            cache_write: 2,
            ..Default::default()
        }
    }
    fn valid() -> ContentBlock {
        ContentBlock::Text{text:r#"{"queries":[{"type":"pattern","query":"Retry backoff configuration prevents request storms"},{"type":"pitfall","query":"Session ownership errors leave attributed calls unfinalized"}]}"#.into()}
    }
    fn assert_final(l: &TestPlannerLedger, o: super::LlmCallOutcome) {
        let f = l.finalized.lock().unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].outcome, o);
        assert_eq!(
            (
                f[0].tokens_in,
                f[0].tokens_out,
                f[0].cache_read_tokens,
                f[0].cache_write_tokens
            ),
            (11, 7, 3, 2)
        );
    }
    #[tokio::test]
    async fn planner_host_success_finalizes_usage_and_attribution() {
        let (r, l) = run(
            vec![
                Ok(StreamEvent::Delta(valid())),
                Ok(StreamEvent::Usage(usage())),
                Ok(StreamEvent::Done),
            ],
            false,
            false,
            100,
        )
        .await;
        assert_eq!(
            r.outcome,
            djinn_supervisor::services::wire::PlannerOutcome::Success
        );
        assert!(r.content.is_some());
        let c = l.created.lock().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(
            (
                &c[0].project_id.as_str(),
                &c[0].task_id.as_str(),
                &c[0].task_run_id.as_str(),
                &c[0].session_id.as_str(),
                &c[0].created_by_user_id.as_str()
            ),
            (
                &"project-1",
                &"task-1",
                &"run-1",
                &"session-1",
                &"creator-1"
            )
        );
        drop(c);
        assert_final(&l, super::LlmCallOutcome::Success);
    }
    #[tokio::test]
    async fn planner_host_timeout_finalizes_retained_usage() {
        let (r, l) = run(vec![Ok(StreamEvent::Usage(usage()))], true, false, 5).await;
        assert_eq!(
            r.outcome,
            djinn_supervisor::services::wire::PlannerOutcome::Timeout
        );
        assert!(r.content.is_none());
        assert_final(&l, super::LlmCallOutcome::Timeout);
    }
    #[tokio::test]
    async fn planner_host_invalid_payload_finalizes_retained_usage() {
        let bad = ContentBlock::Text {
            text: r#"{"queries":[{"type":"pattern","query":"Find information about retry configuration"},{"type":"reference","query":"Retry configuration controls exponential backoff limits"}]}"#.into(),
        };
        let (r, l) = run(
            vec![
                Ok(StreamEvent::Delta(bad)),
                Ok(StreamEvent::Usage(usage())),
                Ok(StreamEvent::Done),
            ],
            false,
            false,
            100,
        )
        .await;
        assert_eq!(
            r.outcome,
            djinn_supervisor::services::wire::PlannerOutcome::InvalidPayload
        );
        assert!(r.content.is_none());
        assert_final(&l, super::LlmCallOutcome::InvalidPayload);
    }
    #[tokio::test]
    async fn planner_host_validates_completed_payload_despite_other_operation() {
        use djinn_supervisor::SupervisorServices;

        let provider = std::sync::Arc::new(PlannerStreamProvider {
            events: vec![
                Ok(StreamEvent::Delta(ContentBlock::Text {
                    text: "not JSON".into(),
                })),
                Ok(StreamEvent::Usage(usage())),
                Ok(StreamEvent::Done),
            ],
            hang_after: false,
        });
        let ledger = std::sync::Arc::new(TestPlannerLedger::default());
        let ctx = crate::test_helpers::agent_context_from_db(
            crate::test_helpers::create_test_db(),
            tokio_util::sync::CancellationToken::new(),
        );
        let services = DirectServices::with_planner_test_seam(ctx, provider, ledger.clone());
        let mut attributed_request = request(100);
        attributed_request.operation = "other".into();

        let result = SupervisorServices::plan_memory_intents(&services, attributed_request)
            .await
            .unwrap();

        assert_eq!(
            result.outcome,
            djinn_supervisor::services::wire::PlannerOutcome::InvalidPayload
        );
        assert!(result.content.is_none());
        assert_final(&ledger, super::LlmCallOutcome::InvalidPayload);
    }
    #[tokio::test]
    async fn planner_host_late_oversized_error_finalizes_retained_usage() {
        let (r, l) = run(
            vec![Ok(StreamEvent::Usage(usage())), Err("x".repeat(2000))],
            false,
            false,
            100,
        )
        .await;
        assert_eq!(
            r.outcome,
            djinn_supervisor::services::wire::PlannerOutcome::ProviderError
        );
        assert!(r.content.is_none());
        assert_final(&l, super::LlmCallOutcome::ProviderError);
    }
    #[tokio::test]
    async fn planner_host_finalization_failure_suppresses_valid_content_and_leaves_pending() {
        let (r, l) = run(
            vec![
                Ok(StreamEvent::Delta(valid())),
                Ok(StreamEvent::Usage(usage())),
                Ok(StreamEvent::Done),
            ],
            false,
            true,
            100,
        )
        .await;
        assert_eq!(
            r.outcome,
            djinn_supervisor::services::wire::PlannerOutcome::ProviderError
        );
        assert!(r.content.is_none());
        assert_eq!(l.created.lock().unwrap().len(), 1);
        assert!(l.finalized.lock().unwrap().is_empty());
    }
}
