//! `WorkerSupervisorServices` — the in-Pod [`SupervisorServices`] impl that
//! drives the real [`djinn_supervisor::TaskRunSupervisor`] inside each
//! per-task-run worker Pod.
//!
//! Phase 7b of `~/.claude/plans/phase2-worker-execution-architecture.md`.
//!
//! ## Shape
//!
//! Every host-bound trait method (DB writes, SSE publish, catalog reads, PR
//! open, …) delegates straight to the [`RpcServices`] connection the worker
//! already holds open to djinn-server. `execute_stage` is the load-bearing
//! deviation: it runs LOCALLY by:
//!
//! 1. Constructing an [`LlmProvider`] worker-side from the per-role
//!    [`SerializableCredential`] mounted via the K8s Secret (Phase 7a).
//! 2. Invoking [`djinn_agent::supervisor::worker_execute_stage`] with that
//!    provider injected via `provider_override`, so the in-tree per-stage
//!    executor skips its catalog/vault path entirely.
//!
//! `invoke_llm` is `unreachable!()` on the worker — the worker builds
//! providers locally from Secret-mounted credentials and calls
//! `provider.stream(..)` directly from the reply loop. The trait method
//! itself stays because the host-side `DirectServices` impl uses it for
//! chat-tool invocations.
//!
//! ## AgentContext caveat
//!
//! `supervisor_impl::stage::execute_stage` still threads an `AgentContext`
//! through every helper it calls. The worker builds an `AgentContext` whose
//! `db` connects to the test Dolt for integration coverage; production-Pod
//! behaviour for the DB-touching helpers
//! (`resolve_role_overrides`, `build_prompt_context`,
//! `spawn_post_session_work`, `task_merge::resolve_project_path_for_id`) is
//! a Phase 7-followup. See the integration test in
//! `tests/in_pod_drive.rs` for what currently surfaces.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
// `OAuthConfigWire` is a host-specific wire type (worker Secret serialization) that
// stays in `djinn-agent`; the pure provider-identification helpers are consumed from
// their canonical home in `djinn-slot`.
use djinn_agent::actors::slot::helpers::OAuthConfigWire;
use djinn_agent::context::AgentContext;
use djinn_agent::supervisor::worker_execute_stage;
use djinn_core::models::{SessionRecord, SessionStatus, Task, TaskRunStatus};
use djinn_provider::message::Conversation;
use djinn_provider::provider::{
    LlmProvider, LlmResponse, ProviderConfig, RestampTarget, ToolChoice, create_provider,
    restamp_provider_config_for_model,
};
use djinn_runtime::{ResolvedCredentials, RoleKind, SerializableCredential};
use djinn_slot::helpers::{
    auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, parse_model_id,
};
use djinn_stack::environment::EnvironmentConfig;
use djinn_supervisor::services::{
    BillingSource, CostBasisHint, LeaseAbandonRequest, LeaseBindRequest, LeaseCancelRequest,
    LeaseGrantRequest, LeaseQueueRequest, LeaseReleaseRequest, LeaseResult, LeaseStatusRequest,
    SerializableCreateSessionParams, SerializableCreateTaskRunParams, SerializableDjinnEvent,
};
use djinn_supervisor::{
    BranchPublicationResult, RpcServices, StageError, StageOutcome, SupervisorServices,
    TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

/// In-Pod `SupervisorServices` implementation used by `djinn-agent-worker`.
pub struct WorkerSupervisorServices {
    rpc: Arc<RpcServices>,
    credentials: ResolvedCredentials,
    cancel: CancellationToken,
    agent_context: AgentContext,
    /// Path of the supervisor-created ephemeral [`Workspace`], captured the
    /// first time `execute_stage` is invoked.  Used by `open_pr` to
    /// reconstruct a [`Workspace`] handle (via [`Workspace::attach_existing`])
    /// so we can push the worker's `task_branch` back to the mirror before
    /// delegating the PR-open RPC to the host.
    ///
    /// `Mutex<Option<_>>` rather than `OnceLock` because the value is owned
    /// behind `&self` from inside an `async_trait` method and the mutex
    /// guard is dropped before any `.await`.
    ///
    /// Shared via `Arc` with the SIGTERM / soft-deadline checkpoint handlers in
    /// `main.rs`: the wind-down checkpoint must commit + push the SAME live
    /// ephemeral clone (the supervisor's `TempDir` clone, not `/workspace`), so
    /// the handlers read this slot lazily at fire time.
    captured_workspace_path: Arc<Mutex<Option<PathBuf>>>,
}

impl WorkerSupervisorServices {
    /// Wire a worker-side services impl around the RPC connection,
    /// resolved credentials bundle, supervisor cancel token, and the panic-
    /// stub-ish `AgentContext` the in-Pod supervisor threads through the
    /// per-stage executor.
    ///
    /// `captured_workspace_path` is created in `main.rs::run_task_run` and
    /// shared with the SIGTERM / soft-deadline checkpoint handlers so the
    /// wind-down checkpoint targets the live ephemeral stage clone this impl
    /// records on its first `execute_stage` call.
    pub fn new(
        rpc: Arc<RpcServices>,
        credentials: ResolvedCredentials,
        cancel: CancellationToken,
        agent_context: AgentContext,
        captured_workspace_path: Arc<Mutex<Option<PathBuf>>>,
    ) -> Self {
        Self {
            rpc,
            credentials,
            cancel,
            agent_context,
            captured_workspace_path,
        }
    }
}

/// Derive the `(CostBasisHint, BillingSource)` for a worker-pod session from the
/// Secret-mounted credential kind plus the model id.
///
/// The in-Pod path builds its provider locally and never runs the host's
/// `resolve_model_and_credential`, so `stage.rs` cannot derive the billing
/// signal from a resolved credential — the worker must derive it here and hand
/// it through `worker_execute_stage`. The `SerializableCredential` variant is
/// exactly the `credential_is_oauth` evidence `derive_billing_signal` needs.
///
/// Returns `None` only when `model_id` fails to parse into `provider/model`; the
/// stage then keeps its legacy string-classifier behavior for that session.
fn worker_billing_signal(
    cred: &SerializableCredential,
    model_id: &str,
) -> Option<(CostBasisHint, BillingSource)> {
    let (provider_id, model_name) = parse_model_id(model_id).ok()?;
    let credential_is_oauth = matches!(cred, SerializableCredential::OAuthConfig { .. });
    Some(djinn_agent::supervisor::derive_billing_signal(
        &provider_id,
        &model_name,
        credential_is_oauth,
    ))
}

/// Reconstruct an [`LlmProvider`] from a per-role [`SerializableCredential`]
/// and the model identifier resolved for that role.
///
/// API-key credentials mirror the host-side construction in
/// `djinn_agent::supervisor_impl::stage` — same auth method / format family /
/// capability defaults — minus telemetry (host owns Langfuse) and minus a
/// session-affinity key (the worker has no session_id yet at construction).
/// OAuth credentials deserialise the opaque JSON blob into
/// [`OAuthConfigWire`] and back into a live [`ProviderConfig`].
///
/// `base_url_override` carries the catalog-resolved provider base URL for the
/// API-key arm (the caller resolves it via `get_provider_base_url`); `None`
/// falls back to [`default_base_url`]. It MUST be supplied for API-key
/// third-party providers — `default_base_url` only knows Anthropic/Google and
/// routes everything else to `api.openai.com`, so a Fireworks/Together/Groq key
/// would otherwise hit the wrong host and 404. The OAuth arm ignores it (the
/// wire blob already carries the correct base_url).
pub(crate) fn build_provider_from_serializable(
    cred: &SerializableCredential,
    model_id: &str,
    context_window: u32,
    base_url_override: Option<String>,
) -> Result<Arc<dyn LlmProvider>, StageError> {
    let cfg =
        build_provider_config_from_serializable(cred, model_id, context_window, base_url_override)?;
    Ok(Arc::from(create_provider(cfg)))
}

pub(crate) fn build_provider_config_from_serializable(
    cred: &SerializableCredential,
    model_id: &str,
    context_window: u32,
    base_url_override: Option<String>,
) -> Result<ProviderConfig, StageError> {
    match cred {
        SerializableCredential::ApiKey { api_key, .. } => {
            let (provider_id, model_name) = parse_model_id(model_id)
                .map_err(|e| StageError::ModelResolution(format!("parse_model_id: {e}")))?;
            let format_family = format_family_for_provider(&provider_id, &model_name);
            let base_url = base_url_override.unwrap_or_else(|| default_base_url(&provider_id));

            // Resolve the target model's catalog metadata so the shared restamp
            // helper re-resolves reasoning_effort from the *target* model rather
            // than computing it inline. A catalog miss falls back to
            // non-reasoning / provider-level defaults, matching the pre-restamp
            // behaviour for unknown models.
            let reasoning = djinn_provider::catalog::CatalogService::new()
                .find_model(model_id)
                .is_some_and(|model| model.reasoning);

            let target = RestampTarget {
                model_id: model_name.clone(),
                format_family,
                reasoning,
                context_window,
                capabilities: capabilities_for_provider(&provider_id),
                tool_schema_compat: djinn_provider::catalog::builtin::tool_schema_compat_for(
                    &provider_id,
                    &model_name,
                ),
            };

            // Build a seed `ProviderConfig` from auth/transport fields, then run
            // it through the shared restamp helper so every model-dependent field
            // (reasoning_effort, capabilities/max_tokens_default, tool_schema_compat,
            // context_window) is resolved through the single shared policy — the
            // same path host/canonical slot construction uses. Previously this arm
            // duplicated `default_reasoning_effort_for_model` inline.
            let seed = ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&provider_id, api_key),
                format_family,
                model_id: model_name,
                context_window,
                telemetry: None,
                session_affinity_key: None,
                provider_headers: Default::default(),
                capabilities: target.capabilities.clone(),
                reasoning_effort: None,
                tool_schema_compat: None,
            };
            Ok(restamp_provider_config_for_model(seed, &target))
        }
        SerializableCredential::OAuthConfig { config_json } => {
            let wire: OAuthConfigWire = serde_json::from_str(config_json).map_err(|e| {
                StageError::ModelResolution(format!("deserialize OAuth ProviderConfig: {e}"))
            })?;
            let (provider_id, model_name) = parse_model_id(model_id)
                .map_err(|e| StageError::ModelResolution(format!("parse_model_id: {e}")))?;
            let cfg = wire.to_provider_config();

            // Route through the shared restamp helper instead of a bare
            // `cfg.model_id = model_name` assignment. This is the failover path:
            // the wire blob carries model A's resolved defaults, but the worker
            // may be dispatched against model B, so every model-dependent field
            // (reasoning_effort, capabilities/max_tokens_default, tool_schema_compat,
            // context_window) must be re-resolved from B's catalog/provider identity.
            let reasoning = djinn_provider::catalog::CatalogService::new()
                .find_model(model_id)
                .is_some_and(|model| model.reasoning);
            let format_family = format_family_for_provider(&provider_id, &model_name);

            // Re-resolve capabilities from the TARGET provider identity (same as
            // the API-key arm) so failover to model B picks up B's
            // max_tokens_default instead of carrying model A's stale value from
            // the wire blob. A defensive default keeps `streaming` truthy.
            let capabilities = capabilities_for_provider(&provider_id);
            let capabilities = if capabilities.streaming {
                capabilities
            } else {
                djinn_provider::provider::ProviderCapabilities {
                    streaming: true,
                    max_tokens_default: capabilities.max_tokens_default,
                }
            };

            let target = RestampTarget {
                model_id: model_name.clone(),
                format_family,
                reasoning,
                context_window,
                capabilities,
                tool_schema_compat: djinn_provider::catalog::builtin::tool_schema_compat_for(
                    &provider_id,
                    &model_name,
                ),
            };

            let mut restamped = restamp_provider_config_for_model(cfg, &target);
            // The worker has no session_id at construction time and owns no
            // Langfuse telemetry; clear host-side values that survived the
            // round-trip.
            restamped.telemetry = None;
            restamped.session_affinity_key = None;
            Ok(restamped)
        }
    }
}

#[async_trait]
impl SupervisorServices for WorkerSupervisorServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    // Lease-v1 authority remains on the host. `RpcServices` owns both the
    // canonical wire mapping and the distinction between a durable typed
    // result (including `LeaseWaitTimeout`) and a failed/closed transport
    // (`LeaseUnavailable`), so the worker must only delegate each operation.
    async fn queue_lease(&self, request: LeaseQueueRequest) -> LeaseResult {
        self.rpc.queue_lease(request).await
    }

    async fn grant_lease(&self, request: LeaseGrantRequest) -> LeaseResult {
        self.rpc.grant_lease(request).await
    }

    async fn lease_status(&self, request: LeaseStatusRequest) -> LeaseResult {
        self.rpc.lease_status(request).await
    }

    async fn abandon_lease(&self, request: LeaseAbandonRequest) -> LeaseResult {
        self.rpc.abandon_lease(request).await
    }

    async fn bind_lease_pod(&self, request: LeaseBindRequest) -> LeaseResult {
        self.rpc.bind_lease_pod(request).await
    }

    async fn cancel_lease(&self, request: LeaseCancelRequest) -> LeaseResult {
        self.rpc.cancel_lease(request).await
    }

    async fn release_lease(&self, request: LeaseReleaseRequest) -> LeaseResult {
        self.rpc.release_lease(request).await
    }

    async fn report_stage_step(&self, step: &'static str) -> Result<(), String> {
        self.rpc.report_stage_step(step).await
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        self.rpc.load_task(task_id).await
    }

    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        // Snapshot the supervisor's workspace path on the first stage so
        // `open_pr` can push the worker's task_branch back to the mirror
        // before delegating the host RPC. The first stage owns this path; a
        // later stage must not replace it with another workspace.
        let first_capture = {
            let mut slot = self
                .captured_workspace_path
                .lock()
                .expect("captured_workspace_path mutex poisoned");
            if slot.is_none() {
                *slot = Some(workspace.path().to_path_buf());
                true
            } else {
                false
            }
        };
        // Persist the workspace path onto the task_runs row exactly once, on
        // the first capture. The coordinator creates K8s pod-run rows with
        // `workspace_path = NULL` (it cannot know the in-pod clone path), and
        // the completion boundary (`resolve_final_verification`) and the
        // auto-submit fingerprint path resolve the run's worktree from that
        // row — a NULL there fails every configured completion boundary. This
        // is a prerequisite for stage execution: continuing after a failed
        // write would leave the run unable to resolve its own worktree.
        if first_capture {
            let workspace_path = workspace.path().to_string_lossy().into_owned();
            djinn_db::repositories::task_run::TaskRunRepository::new(self.agent_context.db.clone())
                .set_workspace_path(task_run_id, &workspace_path)
                .await
                .map_err(|e| {
                    StageError::Setup(format!(
                        "persist workspace_path for task run {task_run_id} before stage execution: {e}"
                    ))
                })?;
        }

        let cred = self.credentials.per_role.get(&role_kind).ok_or_else(|| {
            StageError::ModelResolution(format!(
                "no credential mounted for role {}",
                role_kind.as_str()
            ))
        })?;
        let model_id = spec
            .model_id_per_role
            .get(&role_kind)
            .cloned()
            .ok_or_else(|| {
                StageError::ModelResolution(format!(
                    "no model assigned for role {} in TaskRunSpec",
                    role_kind.as_str()
                ))
            })?;
        let context_window = self
            .get_model_context_window(model_id.clone())
            .await
            .unwrap_or(0)
            .max(0) as u32;
        // Resolve the catalog base_url for API-key creds (OAuth carries its own
        // in the wire blob). Without this the worker's `default_base_url` routes
        // every non-Anthropic/Google provider to `api.openai.com` → 404. Mirrors
        // the host stage's soft fallback to `default_base_url` on RPC error.
        let base_url_override = if let SerializableCredential::ApiKey { .. } = cred {
            let (provider_id, _) = parse_model_id(&model_id)
                .map_err(|e| StageError::ModelResolution(format!("parse_model_id: {e}")))?;
            Some(
                self.get_provider_base_url(provider_id.clone())
                    .await
                    .unwrap_or_else(|_| default_base_url(&provider_id)),
            )
        } else {
            None
        };
        let provider =
            build_provider_from_serializable(cred, &model_id, context_window, base_url_override)?;

        // Derive the billing signal HERE — the in-Pod path never runs the host's
        // `resolve_model_and_credential`, so `stage.rs` cannot derive it from a
        // resolved credential. The `SerializableCredential` kind IS the
        // `credential_is_oauth` evidence. Without this, every worker-pod session
        // fell to the legacy string path and mis-booked openai plan usage as
        // `cost_basis = 'actual'` with a NULL `billing_source`.
        let billing_signal = worker_billing_signal(cred, &model_id);

        worker_execute_stage(
            task,
            workspace,
            role_kind,
            task_run_id,
            spec,
            self.agent_context.clone(),
            self.cancel.clone(),
            provider,
            billing_signal,
            self,
        )
        .await
    }

    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome {
        // Push the worker's task_branch from the supervisor's ephemeral
        // workspace into the mirror so the host's
        // `squash_merge_via_mirror` (called from the host-side `open_pr`
        // we delegate to below) can find our commits. Without this, the
        // worker's commits live only in the ephemeral TempDir clone (whose
        // origin is the mirror) and vanish when the Pod exits, so the PR
        // open fails with "task_branch has no commits".
        let captured_path = {
            self.captured_workspace_path
                .lock()
                .expect("captured_workspace_path mutex poisoned")
                .clone()
        };
        match captured_path {
            Some(path) => match Workspace::attach_existing(&path, spec.task_branch.clone()) {
                Ok(ws) => {
                    if let Err(e) = ws.push_to_origin(&spec.task_branch).await {
                        tracing::error!(
                            error = %e,
                            branch = %spec.task_branch,
                            path = %path.display(),
                            "worker_services::open_pr: failed to push task_branch to mirror"
                        );
                        return TaskRunOutcome::Failed {
                            stage: "pr_open".into(),
                            provider_failure: None,
                            reason: format!(
                                "worker failed to push task_branch '{}' to mirror: {e}",
                                spec.task_branch
                            ),
                            error_class: None,
                            hint: None,
                            body_excerpt: None,
                        };
                    }
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        branch = %spec.task_branch,
                        path = %path.display(),
                        "worker_services::open_pr: failed to attach workspace for push"
                    );
                    return TaskRunOutcome::Failed {
                        stage: "pr_open".into(),
                        provider_failure: None,
                        reason: format!(
                            "worker failed to attach workspace at {} for task_branch '{}' push: {e}",
                            path.display(),
                            spec.task_branch
                        ),
                        error_class: None,
                        hint: None,
                        body_excerpt: None,
                    };
                }
            },
            None => {
                // No stage ever ran (sequence empty / supervisor short-
                // circuited).  Nothing to push; fall through to the host
                // RPC, which will surface its own "no commits" error if
                // appropriate.
                tracing::warn!(
                    branch = %spec.task_branch,
                    "worker_services::open_pr: no captured workspace path; \
                     skipping push (no stage ran?)"
                );
            }
        }
        self.rpc.open_pr(spec, task).await
    }

    async fn create_task_run(&self, params: SerializableCreateTaskRunParams) -> Result<(), String> {
        self.rpc.create_task_run(params).await
    }

    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String> {
        self.rpc.update_task_run_status(run_id, status).await
    }

    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String> {
        self.rpc.get_model_context_window(model_id).await
    }

    async fn get_provider_base_url(&self, catalog_provider_id: String) -> Result<String, String> {
        self.rpc.get_provider_base_url(catalog_provider_id).await
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        self.rpc.pick_any_default_model().await
    }

    async fn create_session(
        &self,
        params: SerializableCreateSessionParams,
    ) -> Result<SessionRecord, String> {
        self.rpc.create_session(params).await
    }

    async fn publish_session_message(
        &self,
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    ) -> Result<(), String> {
        self.rpc
            .publish_session_message(session_id, task_id, agent_type, message)
            .await
    }

    async fn get_environment_config(
        &self,
        project_id: String,
    ) -> Result<EnvironmentConfig, String> {
        match self.rpc.get_environment_config(project_id).await {
            Ok(cfg) => Ok(cfg),
            // Mirror the host-side degrade-to-empty semantics; the worker
            // should never hard-fail the stage on an environment_config gap.
            Err(_) => Ok(EnvironmentConfig::empty()),
        }
    }

    async fn invoke_llm(
        &self,
        _model_id: String,
        _conversation: Conversation,
        _tools: Vec<serde_json::Value>,
        _tool_choice: Option<ToolChoice>,
    ) -> Result<LlmResponse, String> {
        // Worker builds providers locally from Secret-mounted credentials
        // (Phase 7a/b) and calls `provider.stream(..)` directly from the
        // reply loop — it never routes LLM invocations through services.
        // The trait method itself stays because `DirectServices` uses it on
        // the host-side chat path. Any worker-side call here is a bug;
        // surface it as a loud panic instead of a silent RPC roundtrip.
        unreachable!(
            "WorkerSupervisorServices::invoke_llm — worker uses local provider; \
             chat path is host-only via DirectServices"
        )
    }

    async fn update_session_status(
        &self,
        session_id: String,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
        parked_reason: Option<String>,
    ) -> Result<(), String> {
        self.rpc
            .update_session_status(
                session_id,
                status,
                tokens_in,
                tokens_out,
                cache_read,
                cache_write,
                parked_reason,
            )
            .await
    }

    async fn flush_session_tokens(
        &self,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Result<(), String> {
        self.rpc
            .flush_session_tokens(session_id, tokens_in, tokens_out, cache_read, cache_write)
            .await
    }

    async fn emit_djinn_event(&self, event: SerializableDjinnEvent) -> Result<(), String> {
        self.rpc.emit_djinn_event(event).await
    }

    async fn touch_activity(&self, task_id: String) -> Result<(), String> {
        self.rpc.touch_activity(task_id).await
    }

    async fn transition_task(
        &self,
        task_id: String,
        action: String,
        reason: Option<String>,
    ) -> Result<(), String> {
        self.rpc.transition_task(task_id, action, reason).await
    }

    async fn tool_github_search(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        self.rpc.tool_github_search(project_id, arguments).await
    }

    async fn tool_github_fetch_file(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        self.rpc.tool_github_fetch_file(project_id, arguments).await
    }

    async fn tool_ci_job_log(
        &self,
        session_task_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        self.rpc.tool_ci_job_log(session_task_id, arguments).await
    }

    async fn tool_ci_artifact(
        &self,
        session_task_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        self.rpc.tool_ci_artifact(session_task_id, arguments).await
    }

    async fn record_arbiter_decision(
        &self,
        task_id: String,
        decision: String,
        evidence_json: String,
    ) -> Result<(), String> {
        self.rpc
            .record_arbiter_decision(task_id, decision, evidence_json)
            .await
    }

    async fn start_monitored_reopen(
        &self,
        task_id: String,
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    ) -> Result<(), String> {
        self.rpc
            .start_monitored_reopen(task_id, directive, verification_command, exclude_models)
            .await
    }

    async fn complete_monitored_reopen(&self, task_id: String) -> Result<(), String> {
        self.rpc.complete_monitored_reopen(task_id).await
    }

    async fn record_arbiter_session_termination(
        &self,
        task_id: String,
        is_infra_failure: bool,
    ) -> Result<bool, String> {
        self.rpc
            .record_arbiter_session_termination(task_id, is_infra_failure)
            .await
    }

    async fn publish_branch_to_github(
        &self,
        spec: &TaskRunSpec,
        task: &Task,
    ) -> BranchPublicationResult {
        self.rpc.publish_branch_to_github(spec, task).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::{
        AuthMethod, FormatFamily, ProviderCapabilities, ReasoningEffort,
    };

    fn api_key_credential() -> SerializableCredential {
        SerializableCredential::ApiKey {
            key_name: "MINIMAX_API_KEY".to_string(),
            api_key: "test-key".to_string(),
        }
    }

    /// Regression for the prod pod-path bug: a ChatGPT/Codex PLAN OAuth
    /// credential on an `openai/*` model must yield `SubscriptionPlan` +
    /// `PlanOauth` through `worker_billing_signal`, so the worker-created
    /// session books `projected` instead of `actual` with a NULL billing_source.
    #[test]
    fn worker_billing_signal_openai_oauth_is_subscription_plan() {
        let oauth = SerializableCredential::OAuthConfig {
            config_json: "{}".to_string(),
        };
        let (hint, source) =
            worker_billing_signal(&oauth, "openai/gpt-5.6-terra").expect("model id parses");
        assert_eq!(hint, CostBasisHint::SubscriptionPlan);
        assert_eq!(source, BillingSource::PlanOauth);
    }

    /// An API-key credential on a metered provider stays `MeteredApi` +
    /// `ApiKey` — OAuth transport is the only thing that flips a session to a
    /// plan, and there is none here.
    #[test]
    fn worker_billing_signal_anthropic_api_key_is_metered() {
        let (hint, source) =
            worker_billing_signal(&api_key_credential(), "anthropic/claude-opus-4-8")
                .expect("model id parses");
        assert_eq!(hint, CostBasisHint::MeteredApi);
        assert_eq!(source, BillingSource::ApiKey);
    }

    #[test]
    fn api_key_reconstruction_enables_minimax_reasoning_from_catalog() {
        let cfg = build_provider_config_from_serializable(
            &api_key_credential(),
            "minimax-coding-plan/MiniMax-M2.5",
            262_144,
            Some("https://api.minimax.io/anthropic/v1".to_string()),
        )
        .expect("config builds");

        assert_eq!(cfg.format_family, FormatFamily::Anthropic);
        assert_eq!(cfg.model_id, "MiniMax-M2.5");
        assert_eq!(cfg.reasoning_effort, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn api_key_reconstruction_keeps_non_reasoning_catalog_model_none() {
        let cfg = build_provider_config_from_serializable(
            &api_key_credential(),
            "openai/gpt-4.1-mini",
            1_047_576,
            Some("https://api.openai.com".to_string()),
        )
        .expect("config builds");

        assert_eq!(cfg.format_family, FormatFamily::OpenAI);
        assert_eq!(cfg.model_id, "gpt-4.1-mini");
        assert_eq!(cfg.reasoning_effort, None);
    }

    #[test]
    fn api_key_reconstruction_falls_back_to_none_when_catalog_lookup_misses() {
        let cfg = build_provider_config_from_serializable(
            &api_key_credential(),
            "minimax-coding-plan/not-in-catalog",
            262_144,
            Some("https://api.minimax.io/anthropic/v1".to_string()),
        )
        .expect("config builds");

        assert_eq!(cfg.format_family, FormatFamily::Anthropic);
        assert_eq!(cfg.reasoning_effort, None);
    }

    #[test]
    fn oauth_reconstruction_re_resolves_target_reasoning_effort() {
        let cfg = ProviderConfig {
            base_url: "https://api.minimax.io/anthropic/v1".to_string(),
            auth: AuthMethod::BearerToken("oauth-token".to_string()),
            format_family: FormatFamily::Anthropic,
            model_id: "provider-default".to_string(),
            context_window: 1,
            telemetry: None,
            session_affinity_key: Some("host-affinity".to_string()),
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: Some(64_000),
            },
            // Wire carries a stale `High` tier for the host's original model;
            // the worker must re-resolve it from the *target* model via the
            // shared restamp helper rather than preserving the wire value.
            reasoning_effort: Some(ReasoningEffort::High),
            tool_schema_compat: None,
        };
        let wire = OAuthConfigWire::from_provider_config(&cfg);
        let cred = SerializableCredential::OAuthConfig {
            config_json: serde_json::to_string(&wire).expect("wire serializes"),
        };

        let rebuilt = build_provider_config_from_serializable(
            &cred,
            "minimax-coding-plan/MiniMax-M2.5",
            262_144,
            None,
        )
        .expect("config builds");

        assert_eq!(rebuilt.model_id, "MiniMax-M2.5");
        assert_eq!(rebuilt.context_window, 262_144);
        assert_eq!(rebuilt.session_affinity_key, None);
        // Transport/auth fields survive the round-trip.
        assert_eq!(rebuilt.base_url, "https://api.minimax.io/anthropic/v1");
        assert!(matches!(
            &rebuilt.auth,
            AuthMethod::BearerToken(t) if t == "oauth-token",
        ));
        // Reasoning-capable Anthropic target → Medium (re-resolved, not preserved).
        assert_eq!(rebuilt.reasoning_effort, Some(ReasoningEffort::Medium));
    }

    /// Regression: OAuth failover from a source config carrying model A's
    /// defaults to model B re-resolves B's `reasoning_effort` and
    /// `max_tokens_default` while preserving the wire blob's auth/base_url.
    #[test]
    fn oauth_failover_re_resolves_target_model_defaults() {
        // Source OAuth blob built for model A: an OpenAI-format non-reasoning
        // model whose defaults (reasoning_effort=None, max_tokens_default=None)
        // must NOT carry forward to model B.
        let source_model_a = ProviderConfig {
            base_url: "https://api.openai.com".to_string(),
            auth: AuthMethod::BearerToken("oauth-token".to_string()),
            format_family: FormatFamily::OpenAI,
            model_id: "gpt-4.1-mini".to_string(),
            context_window: 1,
            telemetry: None,
            session_affinity_key: Some("host-affinity".to_string()),
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities {
                streaming: true,
                max_tokens_default: None,
            },
            reasoning_effort: None,
            tool_schema_compat: None,
        };
        let wire = OAuthConfigWire::from_provider_config(&source_model_a);
        let cred = SerializableCredential::OAuthConfig {
            config_json: serde_json::to_string(&wire).expect("wire serializes"),
        };

        // Failover target: model B = anthropic/claude-sonnet-4-5, an
        // Anthropic-format reasoning-capable model whose provider policy gives
        // reasoning_effort = Some(Medium) and max_tokens_default = Some(64_000).
        // (Anthropic matches the `capabilities_for_provider` special-case, so
        // its max-token default is actually re-resolved — MiniMax does not.)
        let rebuilt = build_provider_config_from_serializable(
            &cred,
            "anthropic/claude-sonnet-4-5",
            200_000,
            None,
        )
        .expect("config builds");

        // Model B's identity + format.
        assert_eq!(rebuilt.model_id, "claude-sonnet-4-5");
        assert_eq!(rebuilt.format_family, FormatFamily::Anthropic);
        // B's re-resolved reasoning_effort (NOT model A's None).
        assert_eq!(rebuilt.reasoning_effort, Some(ReasoningEffort::Medium));
        // B's re-resolved max-token default (NOT model A's None).
        assert_eq!(rebuilt.capabilities.max_tokens_default, Some(64_000));
        assert!(rebuilt.capabilities.streaming);
        // Transport/session fields preserved from the wire blob (auth/base_url)
        // or cleared per worker policy (telemetry/session affinity).
        assert_eq!(rebuilt.base_url, "https://api.openai.com");
        assert!(matches!(
            &rebuilt.auth,
            AuthMethod::BearerToken(t) if t == "oauth-token",
        ));
        assert_eq!(rebuilt.context_window, 200_000);
        assert_eq!(rebuilt.session_affinity_key, None);
        assert!(rebuilt.telemetry.is_none());
        // Anthropic is a native provider (identity schema projection), proving
        // tool_schema_compat is re-resolved from B's provider identity (the
        // source blob carried model A's quirk, here None).
        assert_eq!(rebuilt.tool_schema_compat, None);
    }
}

#[cfg(test)]
mod lease_adapter_conformance_tests {
    use super::WorkerSupervisorServices;
    use djinn_agent::direct_services::DirectServices;
    use djinn_db::BuildLeaseRepository;
    use djinn_runtime::ResolvedCredentials;
    use djinn_supervisor::services::{
        LeaseAbandonRequest, LeaseBindRequest, LeaseCancelRequest, LeaseDeadlines,
        LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest, LeaseResult,
        LeaseState, LeaseStatusRequest, TaskInvocationLeaseIdentity,
    };
    use djinn_supervisor::{RpcServices, SupervisorServices, serve_on_unix_socket};
    use std::{
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio_util::sync::CancellationToken;

    fn identity(id: &str) -> LeaseIdentity {
        LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
            task_id: "task".into(),
            task_run_id: "run".into(),
            invocation_id: id.into(),
        })
    }
    fn queue(id: &str) -> LeaseQueueRequest {
        LeaseQueueRequest {
            identity: identity(id),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        }
    }
    fn expired_queue(id: &str) -> LeaseQueueRequest {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("wall clock")
            .as_millis() as i64;
        LeaseQueueRequest {
            identity: identity(id),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: now_ms - 1,
                launch_deadline_ms: 0,
            },
        }
    }

    async fn script(s: &dyn SupervisorServices) -> Vec<LeaseResult> {
        let mut out = vec![s.queue_lease(queue("one")).await];
        let token = match &out[0] {
            LeaseResult::Granted(g) => g.fencing_token.clone(),
            x => panic!("grant: {x:?}"),
        };
        out.push(s.queue_lease(queue("one")).await);
        out.push(
            s.queue_lease(LeaseQueueRequest {
                identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                    task_id: "conflict".into(),
                    task_run_id: "run".into(),
                    invocation_id: "one".into(),
                }),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await,
        );
        out.push(
            s.grant_lease(LeaseGrantRequest {
                identity: identity("one"),
                fencing_token: token.clone(),
            })
            .await,
        );
        out.push(
            s.lease_status(LeaseStatusRequest {
                identity: identity("one"),
            })
            .await,
        );
        out.push(
            s.bind_lease_pod(LeaseBindRequest {
                identity: identity("one"),
                fencing_token: token.clone(),
                pod_uid: "pod".into(),
            })
            .await,
        );
        // The bound lease occupies the sole unit, making this a real queued acquire.
        out.push(s.queue_lease(queue("queued")).await);
        out.push(
            s.abandon_lease(LeaseAbandonRequest {
                identity: identity("queued"),
                candidate_cleanup: true,
            })
            .await,
        );
        out.push(
            s.release_lease(LeaseReleaseRequest {
                identity: identity("one"),
                fencing_token: token.clone(),
                candidate_cleanup: true,
            })
            .await,
        );
        out.push(
            s.release_lease(LeaseReleaseRequest {
                identity: identity("one"),
                fencing_token: token,
                candidate_cleanup: false,
            })
            .await,
        );
        out.push(s.queue_lease(queue("cancel")).await);
        out.push(
            s.cancel_lease(LeaseCancelRequest {
                identity: identity("cancel"),
                fencing_token: None,
                candidate_cleanup: false,
            })
            .await,
        );
        out.push(
            s.cancel_lease(LeaseCancelRequest {
                identity: identity("cancel"),
                fencing_token: None,
                candidate_cleanup: true,
            })
            .await,
        );
        assert_eq!(out[0], out[1], "duplicate queue must replay its grant");
        assert!(matches!(out[2], LeaseResult::LeaseIdentityConflict { .. }));
        assert!(
            matches!(&out[3], LeaseResult::Status(status) if status.state == LeaseState::Launching)
        );
        assert!(
            matches!(&out[4], LeaseResult::Status(status) if status.state == LeaseState::Launching)
        );
        assert!(matches!(&out[5], LeaseResult::Bound(status) if status.state == LeaseState::Bound));
        assert!(
            matches!(&out[6], LeaseResult::Queued(status) if status.state == LeaseState::Queued)
        );
        assert_eq!(
            out[7],
            LeaseResult::Abandoned {
                candidate_cleanup: true
            }
        );
        assert_eq!(
            out[8],
            LeaseResult::Released {
                candidate_cleanup: true
            }
        );
        assert_eq!(
            out[8], out[9],
            "duplicate release must replay its terminal winner"
        );
        assert!(matches!(out[10], LeaseResult::Granted(_)));
        assert_eq!(
            out[11],
            LeaseResult::Cancelled {
                candidate_cleanup: false
            }
        );
        assert_eq!(
            out[11], out[12],
            "duplicate cancel must replay its terminal winner"
        );
        out
    }
    async fn committed_timeout(s: &dyn SupervisorServices) -> Vec<LeaseResult> {
        let first = s.queue_lease(expired_queue("expired")).await;
        let retry = s.queue_lease(expired_queue("expired")).await;
        assert!(
            matches!(
                &first,
                LeaseResult::LeaseWaitTimeout {
                    timeout_credit: Some(credit)
                } if credit.units == 1
            ),
            "the first committed timeout must carry its one bounded retry credit: {first:?}"
        );
        assert!(
            matches!(
                &retry,
                LeaseResult::LeaseWaitTimeout {
                    timeout_credit: None
                }
            ),
            "durable timeout replay must not mint another timeout credit: {retry:?}"
        );
        vec![first, retry]
    }
    async fn host_with_cap(cap: i64) -> Arc<DirectServices> {
        let db = djinn_agent::test_helpers::create_test_db();
        BuildLeaseRepository::new(db.clone())
            .set_cap(cap)
            .await
            .expect("cap");
        Arc::new(DirectServices::new(
            djinn_agent::test_helpers::agent_context_from_db(db, CancellationToken::new()),
            CancellationToken::new(),
        ))
    }
    #[tokio::test]
    async fn direct_and_worker_rpc_run_the_same_lease_operation_script() {
        let direct = host_with_cap(1).await;
        let direct_results = script(direct.as_ref()).await;
        let direct_timeout_host = host_with_cap(0).await;
        let direct_timeout = committed_timeout(direct_timeout_host.as_ref()).await;
        let server = host_with_cap(1).await;
        let dir = tempfile::Builder::new()
            .prefix("lease-")
            .tempdir_in("/var/tmp")
            .expect("dir");
        let path = dir.path().join("rpc.sock");
        let server_handle = serve_on_unix_socket(&path, server.clone())
            .await
            .expect("serve");
        let cancel = CancellationToken::new();
        let (rpc, background) = RpcServices::connect_unix(&path, cancel.clone())
            .await
            .expect("rpc");
        let worker = WorkerSupervisorServices::new(
            rpc.clone(),
            ResolvedCredentials::default(),
            CancellationToken::new(),
            djinn_agent::test_helpers::agent_context_from_db(
                djinn_agent::test_helpers::create_test_db(),
                CancellationToken::new(),
            ),
            Arc::new(Mutex::new(None)),
        );
        assert_eq!(direct_results, script(&worker).await);
        let timeout_server = host_with_cap(0).await;
        let timeout_path = dir.path().join("timeout-rpc.sock");
        let timeout_handle = serve_on_unix_socket(&timeout_path, timeout_server)
            .await
            .expect("serve timeout");
        let timeout_cancel = CancellationToken::new();
        let (timeout_rpc, timeout_background) =
            RpcServices::connect_unix(&timeout_path, timeout_cancel.clone())
                .await
                .expect("connect timeout rpc");
        let timeout_worker = WorkerSupervisorServices::new(
            timeout_rpc.clone(),
            ResolvedCredentials::default(),
            CancellationToken::new(),
            djinn_agent::test_helpers::agent_context_from_db(
                djinn_agent::test_helpers::create_test_db(),
                CancellationToken::new(),
            ),
            Arc::new(Mutex::new(None)),
        );
        let rpc_timeout = committed_timeout(&timeout_worker).await;
        assert_eq!(
            direct_timeout, rpc_timeout,
            "durable timeout transcript differs by adapter"
        );
        drop(timeout_worker);
        drop(timeout_rpc);
        let _ = timeout_background.writer.await;
        timeout_cancel.cancel();
        let _ = timeout_background.reader.await;
        timeout_handle.cancel();
        let _ = timeout_handle.join.await;
        server_handle.cancel();
        let _ = server_handle.join.await;
        let unavailable = worker.queue_lease(queue("transport-lost")).await;
        assert!(matches!(unavailable, LeaseResult::LeaseUnavailable));
        assert_ne!(
            unavailable, rpc_timeout[0],
            "transport loss is not a committed timeout"
        );
        drop(worker);
        drop(rpc);
        let _ = background.writer.await;
        cancel.cancel();
        let _ = background.reader.await;
    }
}
