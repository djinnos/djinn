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
    SerializableCreateSessionParams, SerializableCreateTaskRunParams, SerializableDjinnEvent,
};
use djinn_supervisor::{
    RoleKind, StageError, StageOutcome, SupervisorServices, TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::supervisor_impl::{SupervisorCallbackContext, execute_stage, supervisor_pr_open};
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
        let pricing = ctx
            .catalog
            .find_model(params.model.as_str())
            .map(|m| m.pricing);
        repo.create(CreateSessionParams {
            project_id: params.project_id.as_str(),
            task_id: params.task_id.as_deref(),
            model: params.model.as_str(),
            agent_type: params.agent_type.as_str(),
            metadata_json: params.metadata_json.as_deref(),
            task_run_id: params.task_run_id.as_deref(),
            pricing: pricing.as_ref(),
        })
        .await
        .map_err(|e| e.to_string())
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
        let telemetry_meta =
            crate::actors::slot::helpers::build_telemetry_meta("invoke_llm", &synthetic_task_id);
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
        let provider: Box<dyn LlmProvider> =
            crate::actors::slot::helpers::build_provider_from_resolved(
                resolved,
                context_window,
                Some(telemetry_meta),
                None,
                base_url,
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

#[cfg(test)]
mod tests {
    use super::{SerializableDjinnEvent, intern_envelope};

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
}
