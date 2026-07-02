// djinn:allow-oversize — thin host adapter; all dispatch orchestration lives
// canonically in `djinn_slot::dispatch_orchestrator`.
//! Host-side dispatch adapter for the canonical djinn-slot dispatch orchestrator.
//!
//! This module implements [`djinn_slot::dispatch_orchestrator::TaskDispatchContext`]
//! for [`AgentContext`] and provides the entry point called from
//! [`host_callbacks::AgentDispatchCallbacks::run_task_dispatch`].
//!
//! All reusable dispatch orchestration (prepare → stream → teardown → post-
//! dispatch bookkeeping) lives in `djinn_slot::dispatch_orchestrator`; this file
//! contains only host-specific callback wiring that depends on `AgentContext`.

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

use djinn_core::models::TaskRunStatus;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{SessionRepository, TaskRepository};
use djinn_runtime::{
    ResolvedCredentials, RoleKind, SessionRuntime, SupervisorFlow, TaskRunSpec, TestRuntime,
};

use crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference;
use crate::context::AgentContext;
use crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind};
use crate::supervisor::services_for_agent_context;

use super::helpers::{
    conflict_context_for_dispatch, default_target_branch, load_provider_credential, parse_model_id,
    refresh_oauth_credential_after_401,
};

// ─── Host dispatch entry point ──────────────────────────────────────────────

/// Host-side dispatch entry point — called from
/// [`host_callbacks::AgentDispatchCallbacks::run_task_dispatch`].
///
/// Delegates the full lifecycle to the canonical
/// `djinn_slot::dispatch_orchestrator::dispatch_task_runtime` with an
/// [`AgentDispatchContext`] adapter that wires `AgentContext` operations.
pub(super) async fn dispatch_task_runtime(
    task_id: String,
    project_path: String,
    model_id: String,
    app_state: AgentContext,
    kill: CancellationToken,
    pause: CancellationToken,
) -> anyhow::Result<()> {
    let ctx = AgentDispatchContext { app_state };
    djinn_slot::dispatch_orchestrator::dispatch_task_runtime(
        &ctx,
        task_id,
        project_path,
        model_id,
        kill,
        pause,
    )
    .await
}

// ─── TaskDispatchContext implementation ─────────────────────────────────────

/// Host adapter wiring [`AgentContext`] into the canonical
/// [`TaskDispatchContext`] trait.
struct AgentDispatchContext {
    app_state: AgentContext,
}

impl djinn_slot::dispatch_orchestrator::TaskDispatchContext for AgentDispatchContext {
    fn load_task<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<djinn_core::models::Task>> + Send + 'a>,
    > {
        Box::pin(async move {
            let repo = TaskRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            );
            match repo.get(task_id).await {
                Ok(Some(t)) => Ok(t),
                Ok(None) => anyhow::bail!("supervisor dispatch: task {task_id} not found"),
                Err(e) => {
                    anyhow::bail!("supervisor dispatch: failed to load task {task_id}: {e}")
                }
            }
        })
    }

    fn resolve_dispatch_context<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = (bool, bool)> + Send + 'a>> {
        Box::pin(async move {
            let conflict_ctx =
                conflict_context_for_dispatch(task_id, &self.app_state).await;
            let has_conflict = conflict_ctx.is_some();
            let repo = TaskRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            );
            let has_review_response = match repo.get(task_id).await {
                Ok(Some(t)) => {
                    matches!(t.status.as_str(), "needs_task_review" | "in_task_review")
                }
                _ => false,
            };
            (has_conflict, has_review_response)
        })
    }

    fn resolve_base_branch<'a>(
        &'a self,
        project_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            default_target_branch(project_id, &self.app_state).await
        })
    }

    fn resolve_flow(
        &self,
        task: &djinn_core::models::Task,
        has_conflict: bool,
        has_review_response: bool,
    ) -> SupervisorFlow {
        crate::roles::flow_for_task_dispatch(task, has_conflict, has_review_response)
    }

    fn resolve_model_id_per_role<'a>(
        &'a self,
        project_id: &'a str,
        flow: SupervisorFlow,
        default_model_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = HashMap<RoleKind, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut model_id_per_role: HashMap<RoleKind, String> = HashMap::new();
            for role in flow.role_sequence() {
                let resolved = resolve_role_model_preference(
                    project_id,
                    role.as_str(),
                    &self.app_state,
                )
                .await
                .unwrap_or_else(|| default_model_id.to_string());
                model_id_per_role.insert(*role, resolved);
            }
            model_id_per_role
        })
    }

    fn check_worker_output_durability<'a>(
        &'a self,
        project_id: &'a str,
        task_branch: &'a str,
        base_branch: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match self.app_state.mirror.as_ref() {
                Some(mirror) => {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        mirror.branch_ahead_of_base(project_id, task_branch, base_branch),
                    )
                    .await
                    {
                        Ok(durable) => durable,
                        Err(_) => {
                            tracing::warn!(
                                branch = %task_branch,
                                "supervisor dispatch: branch_ahead_of_base durability probe \
                                 timed out (>10s); keeping full worker redo (ReviewResponse)"
                            );
                            false
                        }
                    }
                }
                None => false,
            }
        })
    }

    fn resolve_credentials<'a>(
        &'a self,
        spec: &'a TaskRunSpec,
        default_model_id: &'a str,
        creator_user_id: Option<String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<ResolvedCredentials>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut credentials = ResolvedCredentials::default();
            let model_id = default_model_id;
            let resolve_creds = async {
                for role in spec.flow.role_sequence() {
                    let role_model_id = spec
                        .model_id_per_role
                        .get(role)
                        .cloned()
                        .unwrap_or_else(|| model_id.to_string());
                    let (provider_id, model_name) =
                        parse_model_id(&role_model_id).map_err(|e| {
                            anyhow::anyhow!(
                                "supervisor dispatch: cannot parse model id \
                                 `{role_model_id}` for role {role:?}: {e}"
                            )
                        })?;
                    let cred = load_provider_credential(&provider_id, &self.app_state)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "supervisor dispatch: load_provider_credential({provider_id}) \
                                 for role {role:?}: {e}"
                            )
                        })?;
                    credentials.insert(*role, cred.with_model_id(&model_name).to_serializable());
                }
                Ok::<(), anyhow::Error>(())
            };
            djinn_core::auth_context::SESSION_USER_ID
                .scope(creator_user_id, resolve_creds)
                .await?;
            Ok(credentials)
        })
    }

    fn construct_runtime<'a>(
        &'a self,
        _task: &'a djinn_core::models::Task,
        _spec: &'a TaskRunSpec,
        kill: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<std::sync::Arc<dyn SessionRuntime>>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mirror = match self.app_state.mirror.as_ref() {
                Some(m) => m.clone(),
                None => {
                    anyhow::bail!(
                        "supervisor dispatch: AgentContext has no MirrorManager configured — \
                         cannot run supervisor-driven task-run"
                    );
                }
            };
            let runtime: std::sync::Arc<dyn SessionRuntime> = match runtime_kind() {
                RuntimeKind::Kubernetes => {
                    let config = djinn_k8s::KubernetesConfig::from_env();
                    let registry = match self.app_state.rpc_registry.as_ref() {
                        Some(reg) => reg.clone(),
                        None => {
                            anyhow::bail!(
                                "supervisor dispatch: AgentContext has no ConnectionRegistry \
                                 — the djinn-server boot path must plumb `rpc_registry` into \
                                 `AppState::agent_context()` before the Kubernetes runtime can \
                                 be constructed"
                            );
                        }
                    };
                    match djinn_k8s::KubernetesRuntime::with_db(
                        config,
                        registry,
                        self.app_state.db.clone(),
                    )
                    .await
                    {
                        Ok(rt) => std::sync::Arc::new(rt),
                        Err(e) => {
                            anyhow::bail!(
                                "supervisor dispatch: failed to construct KubernetesRuntime \
                                 (is a kubeconfig available?): {e}"
                            );
                        }
                    }
                }
                RuntimeKind::Test => {
                    let services =
                        services_for_agent_context(self.app_state.clone(), kill.clone());
                    let runner = SupervisorTaskRunner::new(mirror, services);
                    std::sync::Arc::new(TestRuntime::new(runner))
                }
            };
            Ok(runtime)
        })
    }

    fn resolve_read_sources<'a>(
        &'a self,
        epic_id: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            djinn_db::EpicRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            )
            .read_sources_for_task(epic_id)
            .await
            .unwrap_or_default()
        })
    }

    fn resolve_private_deps<'a>(
        &'a self,
        project_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = (Option<String>, Option<String>)> + Send + 'a>,
    > {
        Box::pin(async move {
            let pd_project_repo = djinn_db::ProjectRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            );
            let github_owner = pd_project_repo
                .get_github_coords(project_id)
                .await
                .ok()
                .flatten()
                .map(|(owner, _repo)| owner);
            let github_install_token =
                match pd_project_repo.get_installation_id(project_id).await {
                    Ok(Some(installation_id)) => {
                        djinn_provider::github_app::installations::get_installation_token(
                            installation_id,
                        )
                        .await
                        .map(|t| t.token)
                        .ok()
                    }
                    _ => None,
                };
            (github_owner, github_install_token)
        })
    }

    fn resolve_creator_user_id<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            let repo = TaskRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            );
            repo.created_by_user_id(task_id).await.ok().flatten()
        })
    }

    fn resolve_commit_author<'a>(
        &'a self,
        creator_user_id: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = (Option<String>, Option<String>)> + Send + 'a>,
    > {
        Box::pin(async move {
            match creator_user_id {
                Some(uid) => {
                    match djinn_db::UserRepository::new(self.app_state.db.clone())
                        .get_by_id(uid)
                        .await
                    {
                        Ok(Some(user)) => (
                            Some(
                                user.github_name
                                    .clone()
                                    .unwrap_or_else(|| user.github_login.clone()),
                            ),
                            Some(format!(
                                "{}+{}@users.noreply.github.com",
                                user.github_id, user.github_login
                            )),
                        ),
                        _ => (None, None),
                    }
                }
                None => (None, None),
            }
        })
    }

    fn try_refresh_oauth_after_401<'a>(
        &'a self,
        model_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            refresh_oauth_credential_after_401(model_id, &self.app_state).await
        })
    }

    fn surface_credential_revocation<'a>(
        &'a self,
        owner: Option<&'a str>,
        model_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let Ok((provider_id, _model_name)) = parse_model_id(model_id) else {
                return;
            };
            let cred_provider =
                djinn_provider::catalog::builtin::resolve_oauth_provider(&provider_id)
                    .map(|s| s.to_string())
                    .unwrap_or(provider_id);
            let reason = format!(
                "{cred_provider} rejected the credential (HTTP 401 — token revoked or invalid). \
                 Reconnect this provider to resume."
            );
            let repo = djinn_provider::repos::CredentialRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            );
            match repo.mark_revoked(&cred_provider, owner, &reason).await {
                Ok(n) if n > 0 => tracing::warn!(
                    provider = %cred_provider,
                    owner = ?owner,
                    "supervisor: marked credential revoked after 401 — owner must reconnect"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    provider = %cred_provider,
                    error = %e,
                    "supervisor: failed to mark credential revoked after 401"
                ),
            }
        })
    }

    fn log_agent_activity<'a>(
        &'a self,
        task_id: &'a str,
        agent_type: &'a str,
        actor: &'a str,
        event_type: &'a str,
        payload: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let repo = TaskRepository::new(
                self.app_state.db.clone(),
                self.app_state.event_bus.clone(),
            );
            if let Err(e) = repo
                .log_activity(Some(task_id), agent_type, actor, event_type, payload)
                .await
            {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "supervisor dispatch: failed to log agent activity"
                );
            }
        })
    }

    fn get_coordinator<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Option<Box<dyn djinn_slot::dispatch_orchestrator::CoordinatorOps>>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.app_state.coordinator().await.map(|c| {
                Box::new(AgentCoordinatorOps(c))
                    as Box<dyn djinn_slot::dispatch_orchestrator::CoordinatorOps>
            })
        })
    }

    fn record_model_success(&self, owner: Option<&str>, model_id: &str) {
        self.app_state
            .health_tracker
            .record_success(owner, model_id);
    }

    fn record_model_stall(&self, owner: Option<&str>, model_id: &str, escalate: bool) {
        self.app_state
            .health_tracker
            .record_stall(owner, model_id, escalate);
    }

    fn record_model_failure(&self, owner: Option<&str>, model_id: &str) {
        self.app_state
            .health_tracker
            .record_failure(owner, model_id);
    }

    fn note_task_provider_failure(
        &self,
        task_id: &str,
        throttle: bool,
        retry_after_ms: Option<u64>,
    ) {
        self.app_state.health_tracker.note_task_provider_failure(
            task_id,
            djinn_provider::catalog::health::TaskFailureSignal {
                throttle,
                retry_after_ms,
            },
        );
    }

    fn interrupt_running_sessions<'a>(
        &'a self,
        task_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let repo =
                SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone());
            match repo.interrupt_running_for_task(task_id).await {
                Ok(n) if n > 0 => tracing::warn!(
                    task_id = %task_id,
                    sessions = n,
                    "supervisor dispatch: finalized orphaned running session(s) after infra death"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "supervisor dispatch: failed to finalize session row after infra death"
                ),
            }
        })
    }

    fn teardown_cargo_target_run_dir<'a>(
        &'a self,
        task_run_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let root = self
                .app_state
                .cargo_target_runs_root
                .clone()
                .unwrap_or_else(djinn_core::paths::cargo_target_runs_root);
            let id = task_run_id.to_string();
            let log_root = root.clone();
            let log_id = id.clone();
            match tokio::task::spawn_blocking(move || {
                djinn_core::cargo_target_runs::teardown_run_dir(&root, &id)
            })
            .await
            {
                Ok(Ok(result)) => {
                    if result.removed {
                        tracing::info!(
                            task_run_id = %log_id,
                            root = %log_root.display(),
                            cleanup_outcome = result.outcome(),
                            removed_count = result.removed_count(),
                            "supervisor dispatch: host teardown removed orphaned cargo target run-dir"
                        );
                    } else {
                        tracing::debug!(
                            task_run_id = %log_id,
                            root = %log_root.display(),
                            cleanup_outcome = result.outcome(),
                            "supervisor dispatch: cargo target run-dir already absent at host teardown"
                        );
                    }
                }
                Ok(Err(e)) => tracing::warn!(
                    task_run_id = %log_id,
                    root = %log_root.display(),
                    error = %e,
                    cleanup_outcome = "failed",
                    "supervisor dispatch: host teardown failed to remove cargo target run-dir"
                ),
                Err(e) => tracing::warn!(
                    task_run_id = %log_id,
                    root = %log_root.display(),
                    error = %e,
                    cleanup_outcome = "failed",
                    "supervisor dispatch: host teardown task join failed"
                ),
            }
        })
    }

    fn trigger_session_extraction(&self, task_id: String, task_run_id: String) {
        let app_state_ext = self.app_state.clone();
        tokio::spawn(async move {
            crate::actors::slot::session_extraction::run_post_session_extraction(
                task_id,
                task_run_id,
                app_state_ext,
            )
            .await;
        });
    }

    fn reap_orphan_task_run<'a>(
        &'a self,
        task_id: &'a str,
        terminal_status: TaskRunStatus,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let repo = TaskRunRepository::new(self.app_state.db.clone());
            match repo
                .reap_running_for_task(task_id, terminal_status)
                .await
            {
                Ok(Some(run_id)) => {
                    tracing::warn!(
                        task_id = %task_id,
                        task_run_id = %run_id,
                        status = %terminal_status,
                        "supervisor dispatch: reaped orphan task_run row \
                         (in-pod supervisor never sent terminal RPC)"
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "supervisor dispatch: reap_running_for_task failed"
                    );
                }
            }
        })
    }
}

// ─── CoordinatorOps adapter ─────────────────────────────────────────────────

/// Adapter bridging the agent's coordinator handle to the
/// [`CoordinatorOps`] trait.
struct AgentCoordinatorOps(crate::actors::coordinator::CoordinatorHandle);

#[async_trait::async_trait]
impl djinn_slot::dispatch_orchestrator::CoordinatorOps for AgentCoordinatorOps {
    async fn clear_planned_dispatch_completion(
        &self,
        task_id: &str,
        event: &str,
    ) -> anyhow::Result<()> {
        self.0
            .clear_planned_dispatch_completion(task_id, event)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn route_loop_guard_planner_intervention(
        &self,
        task_id: &str,
        role: &'static str,
        reason: &str,
    ) -> anyhow::Result<()> {
        self.0
            .route_loop_guard_planner_intervention(task_id, role, reason)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}
