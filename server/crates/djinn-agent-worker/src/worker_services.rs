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
use djinn_agent::actors::slot::helpers::{
    OAuthConfigWire, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, parse_model_id,
};
use djinn_agent::context::AgentContext;
use djinn_agent::supervisor::worker_execute_stage;
use djinn_core::models::{SessionRecord, SessionStatus, Task, TaskRunStatus};
use djinn_provider::message::Conversation;
use djinn_provider::provider::{
    LlmProvider, LlmResponse, ProviderCapabilities, ProviderConfig, ToolChoice, create_provider,
    default_reasoning_effort_for_model,
};
use djinn_runtime::{ResolvedCredentials, RoleKind, SerializableCredential};
use djinn_stack::environment::EnvironmentConfig;
use djinn_supervisor::services::{
    SerializableCreateSessionParams, SerializableCreateTaskRunParams, SerializableDjinnEvent,
};
use djinn_supervisor::{
    RpcServices, StageError, StageOutcome, SupervisorServices, TaskRunOutcome, TaskRunSpec,
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
            let reasoning_effort = djinn_provider::catalog::CatalogService::new()
                .find_model(model_id)
                .and_then(|model| {
                    default_reasoning_effort_for_model(model.reasoning, format_family, &model.id)
                });

            Ok(ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&provider_id, api_key),
                format_family,
                model_id: model_name,
                context_window,
                telemetry: None,
                session_affinity_key: None,
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&provider_id),
                reasoning_effort,
            })
        }
        SerializableCredential::OAuthConfig { config_json } => {
            let wire: OAuthConfigWire = serde_json::from_str(config_json).map_err(|e| {
                StageError::ModelResolution(format!("deserialize OAuth ProviderConfig: {e}"))
            })?;
            let (_, model_name) = parse_model_id(model_id)
                .map_err(|e| StageError::ModelResolution(format!("parse_model_id: {e}")))?;
            let mut cfg = wire.to_provider_config();
            cfg.model_id = model_name;
            cfg.context_window = context_window;
            cfg.telemetry = None;
            cfg.session_affinity_key = None;
            // `capabilities` survives the round-trip via OAuthCapabilitiesWire
            // but a defensive default keeps `streaming` truthy if the host
            // shipped a zero-value blob.
            if !cfg.capabilities.streaming {
                cfg.capabilities = ProviderCapabilities {
                    streaming: true,
                    max_tokens_default: cfg.capabilities.max_tokens_default,
                };
            }
            Ok(cfg)
        }
    }
}

#[async_trait]
impl SupervisorServices for WorkerSupervisorServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
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
        // before delegating the host RPC.  The supervisor passes the same
        // `&Workspace` to every stage, so capturing on the first call is
        // sufficient; subsequent calls overwrite with the same path.
        {
            let mut slot = self
                .captured_workspace_path
                .lock()
                .expect("captured_workspace_path mutex poisoned");
            *slot = Some(workspace.path().to_path_buf());
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

        worker_execute_stage(
            task,
            workspace,
            role_kind,
            task_run_id,
            spec,
            self.agent_context.clone(),
            self.cancel.clone(),
            provider,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::{AuthMethod, FormatFamily, ReasoningEffort};

    fn api_key_credential() -> SerializableCredential {
        SerializableCredential::ApiKey {
            key_name: "MINIMAX_API_KEY".to_string(),
            api_key: "test-key".to_string(),
        }
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
    fn oauth_reconstruction_preserves_wire_reasoning_effort() {
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
            reasoning_effort: Some(ReasoningEffort::High),
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
        assert_eq!(rebuilt.reasoning_effort, Some(ReasoningEffort::High));
    }
}
