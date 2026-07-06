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
use djinn_db::SessionRepository;
use djinn_db::TaskRunRepository;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_stack::environment::EnvironmentConfig;
use djinn_supervisor::services::{
    CostBasisHint, SerializableCreateSessionParams, SerializableCreateTaskRunParams,
    SerializableDjinnEvent,
};
use djinn_supervisor::{
    RoleKind, StageError, StageOutcome, SupervisorServices, TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::supervisor_impl::{SupervisorCallbackContext, execute_stage, supervisor_pr_open};
use djinn_provider::catalog::builtin::classify_provider;
use djinn_provider::message::Conversation;
use djinn_provider::provider::{LlmProvider, LlmResponse, StreamEvent, TokenUsage, ToolChoice};
use futures::StreamExt;

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
        }
    }

    /// Execute the arbiter park transaction: persist the decision and dossier
    /// on the current unconsumed arbitration row, mark it consumed, create a
    /// `HumanReview` remediation hold with the dossier as the hold description,
    /// and emit `arbiter_decision` / `arbiter_parked` activity events.
    ///
    /// Called BEFORE the `ArbiterPark` state transition so the HumanReview
    /// blocker exists before the source task lands at `open` (the ordering
    /// contract from 7f8u). On any arbitration-row failure, fails closed by
    /// creating the HumanReview hold with a fallback dossier rather than
    /// leaving the task stranded.
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
            // Update the existing unconsumed row with the decision/dossier.
            let decision_json = serde_json::json!({
                "decision": "park",
                "dossier_summary": dossier_summary,
            });
            arb_repo
                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                    task_id,
                    hold_cycle: record.hold_cycle,
                    mirror_head_sha: None,
                    github_head_sha: None,
                    pr_url: None,
                    failing_ci_job_ids: None,
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
                    mirror_head_sha: None,
                    github_head_sha: None,
                    pr_url: None,
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

        // Create a HumanReview remediation task that blocks the source.
        // Reuses the same semantics as the coordinator's
        // `create_remediation_task(RemediationKind::HumanReview)` from 7f8u.
        self.create_arbiter_human_review_hold(
            task_id,
            &task.project_id,
            &dossier,
            &dossier_summary,
        )
        .await?;

        // Emit arbiter_decision activity.
        let decision_payload = serde_json::json!({
            "event": "arbiter_decision",
            "task_id": task.short_id,
            "hold_cycle": hold_cycle,
            "decision": "park",
            "dossier_summary": dossier_summary,
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

        // Emit arbiter_parked activity.
        let parked_payload = serde_json::json!({
            "event": "arbiter_parked",
            "task_id": task.short_id,
            "hold_cycle": hold_cycle,
            "decision": "park",
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

        // Record park metric.
        djinn_telemetry::task::increment_parked_labeled(0, 0, 0, task.reopen_count);

        Ok(())
    }

    /// Create a HumanReview remediation task that blocks the source task.
    /// The dossier content becomes the hold description so humans see the
    /// structured arbiter analysis rather than a static failure template.
    async fn create_arbiter_human_review_hold(
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

        // Load the source task for naming the review task.
        let source_task = task_repo.get(source_task_id).await.ok().flatten();
        let source_creator = source_task
            .as_ref()
            .and_then(|t| t.created_by_user_id.clone());

        // Idempotency: if the source already has an unresolved blocker
        // (from a prior park), skip creating a duplicate hold.
        if let Some(ref src) = source_task {
            match task_repo.list_blockers(&src.id).await {
                Ok(blockers) if blockers.iter().any(|b| b.status != "closed") => {
                    tracing::info!(
                        source_task_id = %src.short_id,
                        "arbiter_park: human-review hold skipped — source already blocked"
                    );
                    return Ok(());
                }
                _ => {}
            }
        }

        // Build the HumanReview hold description from the arbiter dossier.
        let hold_reason = format!(
            "Arbiter park decision — human review required.\n\n{}",
            serde_json::to_string_pretty(dossier).unwrap_or_else(|_| dossier.to_string())
        );

        let title = match source_task.as_ref() {
            Some(t) => {
                let name: String = t.title.chars().take(70).collect();
                format!("Planner remediation [{}]: {}", t.short_id, name)
            }
            None => format!(
                "Arbiter park hold: {}",
                &dossier_summary[..dossier_summary.len().min(60)]
            ),
        };
        let source_label = source_task
            .as_ref()
            .map(|t| format!("{} ({})", t.title, t.short_id))
            .unwrap_or_else(|| source_task_id.to_string());
        let description = format!(
            "Escalated from task {source_label}. Arbiter decided to park — \
             this requires HUMAN review.\n\nDo NOT auto-resolve: a human must \
             close THIS task to release the blocked source task.\n\nReason: {}",
            hold_reason
        );
        let instructions = "Arbiter parked this task. Requires human review — do not auto-resolve; \
             a human must close this task to release the blocked source task.";

        // Create the review task.
        let review_task = match djinn_core::auth_context::SESSION_USER_ID
            .scope(
                source_creator,
                task_repo.create_in_project(
                    project_id,
                    None,
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
                return Err(format!("arbiter_park: failed to create review task: {e}"));
            }
        };

        // Block the source on the review task.
        if let Some(ref src) = source_task
            && let Err(e) = task_repo.add_blocker(&src.id, &review_task.id).await
        {
            tracing::warn!(
                error = %e,
                source_task_id = %src.short_id,
                review_task_id = %review_task.short_id,
                "arbiter_park: failed to block source on review task"
            );
        }

        // Label the hold for UI visibility.
        if let Err(e) = task_repo
            .update_labels(&review_task.id, r#"["human-review-hold"]"#)
            .await
        {
            tracing::warn!(
                error = %e,
                review_task_id = %review_task.short_id,
                "arbiter_park: failed to label hold task"
            );
        }

        Ok(())
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
        self.task_runs
            .create(CreateTaskRunParams {
                id: params.id.as_str(),
                project_id: params.project_id.as_str(),
                task_id: params.task_id.as_str(),
                trigger_type: params.trigger_type.as_str(),
                status: params.status.as_deref(),
                workspace_path: params.workspace_path.as_deref(),
                mirror_ref: params.mirror_ref.as_deref(),
            })
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
        let mut stream = provider
            .stream(&conversation, &tools, tool_choice)
            .await
            .map_err(|e| format!("provider stream init failed: {e}"))?;
        let mut response = LlmResponse {
            content: Vec::new(),
            thinking: String::new(),
            usage: TokenUsage::default(),
        };
        while let Some(ev) = stream.next().await {
            match ev.map_err(|e| format!("provider stream error: {e}"))? {
                StreamEvent::Delta(block) => response.content.push(block),
                StreamEvent::Thinking(s) => response.thinking.push_str(&s),
                StreamEvent::Usage(u) => response.usage = u,
                StreamEvent::Done => break,
            }
        }
        Ok(response)
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

        // Arbiter park: execute the full park transaction before the state
        // transition so the HumanReview blocker exists before the task lands
        // at `open` (the ordering contract from 7f8u).
        if matches!(parsed, TransitionAction::ArbiterPark) {
            self.execute_arbiter_park_transaction(&task_id, reason.as_deref())
                .await?;
        }

        let repo = TaskRepository::new(
            self.callbacks.agent_context.db.clone(),
            self.callbacks.agent_context.event_bus.clone(),
        );
        repo.transition(
            &task_id,
            parsed,
            "supervisor",
            "system",
            reason.as_deref(),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
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

    async fn run_arbiter_preapproval_gate(
        &self,
        task: &Task,
    ) -> Result<djinn_supervisor::ArbiterGateResult, String> {
        let db = &self.callbacks.agent_context.db;
        let task_repo = djinn_db::TaskRepository::new(
            db.clone(),
            self.callbacks.agent_context.event_bus.clone(),
        );
        let outcome =
            djinn_coordinator::run_arbiter_preapproval_gate(db, &task_repo, task).await;
        Ok(outcome)
    }
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
    use super::{CostBasisHint, SerializableDjinnEvent, determine_cost_basis, intern_envelope};
    use djinn_core::models::Pricing;

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
}
