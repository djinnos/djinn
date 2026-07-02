// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
// ─── hfhw cutover: session extraction delegated to djinn-slot ────────────
//
// The full session extraction implementation (structural taxonomy, LLM
// distillation, co-access flush, `run_extraction_backfill`) now lives in
// `djinn_slot::session_extraction`.
//
// This module is a thin adapter.  The public `run_extraction_backfill` entry
// point converts `AgentContext` → `SlotContext` and delegates to
// `djinn_slot::run_extraction_backfill`.
//
// The old production implementation (1700+ lines of direct `sqlx` queries,
// structural extraction, LLM extraction callout, and helper functions) has
// been removed from `djinn-agent`.  It is no longer production-reachable.
//
// Public types (`ExtractionQuality`, `SessionTaxonomy`, etc.) are re-exported
// from `djinn-slot`.  Test-only functions (`extract_session_signals`)
// are gated behind `#[cfg(test)]`.

use crate::context::AgentContext;

// ─── Re-exports from djinn-slot ──────────────────────────────────────────
// Types used by `llm_extraction.rs` and its tests.  The implementations
// now live in `djinn-slot`; these re-exports keep `crate::actors::slot::session_extraction::*`
// paths resolving for agent-internal callers.

#[cfg(test)]
#[allow(unused_imports)]
// retained for agent facade compatibility tests; canonical home is djinn-slot
pub use djinn_slot::{ExtractionQuality, SessionTaxonomy};
// Only needed by llm_extraction_tests; suppressed in non-test builds to avoid
// unused-import warnings from clippy -D warnings.
#[cfg(test)]
#[allow(unused_imports)]
// retained for agent facade compatibility tests; canonical home is djinn-slot
pub use djinn_slot::extract_session_signals;

// `run_structural_extraction` is re-exported from djinn-slot but takes
// `SlotContext`.  We provide a thin adapter that accepts `AgentContext`
// for tests that still construct the agent-side context.
#[cfg(test)]
#[allow(dead_code)] // retained for agent facade compatibility tests; canonical home is djinn-slot
pub(crate) async fn run_structural_extraction(
    session_id: String,
    messages: Vec<djinn_core::message::Message>,
    app_state: AgentContext,
) -> Option<SessionTaxonomy> {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_structural_extraction(session_id, messages, slot_ctx).await
}

/// Host callbacks for the extraction adapter.
///
/// Extraction uses the normal slot-side model-resolution path, which delegates
/// credential loading through [`djinn_slot::host::SlotHostCallbacks`]. Keep the
/// non-extraction callbacks as no-ops, but preserve the `AgentContext` required
/// to reuse the dispatch-side credential loader (including OAuth refresh and
/// credential-vault fallback behavior).
struct ExtractionCallbacks(AgentContext);

fn agent_credential_to_slot(
    credential: crate::actors::slot::helpers::ProviderCredential,
) -> djinn_slot::helpers::ProviderCredential {
    match credential {
        crate::actors::slot::helpers::ProviderCredential::ApiKey(key_name, api_key) => {
            djinn_slot::helpers::ProviderCredential::ApiKey(key_name, api_key)
        }
        crate::actors::slot::helpers::ProviderCredential::OAuthConfig(config) => {
            djinn_slot::helpers::ProviderCredential::OAuthConfig(config)
        }
    }
}

impl djinn_slot::host::SlotHostCallbacks for ExtractionCallbacks {
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("not available in extraction backfill".into()) })
    }

    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &djinn_core::models::Task,
        _context_json: &serde_json::Value,
    ) -> String {
        String::new()
    }

    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn build_mcp_state(
        &self,
        _ctx: &djinn_slot::host::SlotContext,
    ) -> djinn_control_plane::McpState {
        // Not invoked during extraction backfill.  Only `db` and `event_bus`
        // are used by the extraction path.
        unreachable!("build_mcp_state not available in extraction backfill adapter")
    }

    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not available in extraction backfill".into(),
            })
        })
    }

    fn resolve_provider_credential<'a>(
        &'a self,
        provider_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<djinn_slot::helpers::ProviderCredential, String>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            crate::actors::slot::helpers::load_provider_credential(provider_id, &self.0)
                .await
                .map(agent_credential_to_slot)
                .map_err(|e| {
                    format!(
                        "extraction credential resolution failed for provider {provider_id}: {e}"
                    )
                })
        })
    }

    fn run_task_dispatch<'a>(
        &'a self,
        _task_id: String,
        _project_path: String,
        _model_id: String,
        _ctx: djinn_slot::host::SlotContext,
        _kill: tokio_util::sync::CancellationToken,
        _pause: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn touch_activity_rpc<'a>(
        &'a self,
        _task_id: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn flush_session_tokens_rpc<'a>(
        &'a self,
        _session_id: String,
        _tokens_in: i64,
        _tokens_out: i64,
        _cache_read: i64,
        _cache_write: i64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

/// Convert `AgentContext` → `SlotContext` for extraction backfill.
///
/// Maps the shared service-handle fields and installs extraction host callbacks
/// that keep non-extraction operations no-op while resolving provider
/// credentials through the normal AgentContext-backed loader.
pub(crate) fn agent_to_slot_context(agent: &AgentContext) -> djinn_slot::host::SlotContext {
    djinn_slot::host::SlotContext {
        db: agent.db.clone(),
        event_bus: agent.event_bus.clone(),
        catalog: agent.catalog.clone(),
        health_tracker: agent.health_tracker.clone(),
        background_work_tasks: agent.background_work_tasks.clone(),
        active_tasks: agent.active_tasks.clone(),
        default_project_id: agent.default_project_id.clone(),
        working_root: agent.working_root.clone(),
        coordinator_trigger: None,
        runtime_ops: agent.runtime_ops.clone(),
        repo_graph_ops: agent.repo_graph_ops.clone(),
        clock: std::sync::Arc::new(djinn_core::clock::SystemClock::new()),
        callbacks: std::sync::Arc::new(ExtractionCallbacks(agent.clone())),
        tool_dispatcher: None,
    }
}

#[cfg(test)]
mod tests {
    use djinn_db::Database;
    use djinn_provider::repos::CredentialRepository;
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extraction_callbacks_resolve_credentials_through_agent_loader() {
        let db = Database::open_in_memory().expect("db");
        db.ensure_initialized().await.expect("init db");
        let agent =
            crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        let credential_repo = CredentialRepository::new(db, agent.event_bus.clone());
        credential_repo
            .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "sk-test-extraction", None)
            .await
            .expect("store credential");

        let slot_ctx = agent_to_slot_context(&agent);
        let resolved = slot_ctx
            .callbacks
            .resolve_provider_credential("anthropic", &slot_ctx)
            .await
            .expect("resolve credential through extraction callback");

        match resolved {
            djinn_slot::helpers::ProviderCredential::ApiKey(key_name, api_key) => {
                assert_eq!(key_name, "ANTHROPIC_API_KEY");
                assert_eq!(api_key, "sk-test-extraction");
            }
            djinn_slot::helpers::ProviderCredential::OAuthConfig(_) => {
                panic!("expected API-key credential for anthropic test provider")
            }
        }
    }

    /// Regression: prove the live post-session extraction entrypoint reaches
    /// slot-side credential resolution through the real `agent_to_slot_context`
    /// / `ExtractionCallbacks` adapter — not through an injected-provider
    /// helper or a direct unit call to `resolve_provider_credential`.
    ///
    /// Sets up minimal DB fixtures (project, task, session with messages and a
    /// stored credential), calls `run_post_session_extraction` (the real agent
    /// entrypoint), and asserts that:
    /// 1. `resolve_model_and_credential` emits a `credential_loading` lifecycle
    ///    event (proving the model-resolution → credential-loading path ran).
    /// 2. Structural extraction completed (event_taxonomy stored on session).
    ///
    /// The LLM call itself fails gracefully (fake key, no network); the proof
    /// is that the credential-resolution callback was invoked through the real
    /// adapter seam, not bypassed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_session_extraction_reaches_credential_resolution_through_real_adapter() {
        use std::sync::Arc;

        use djinn_core::events::EventBus;
        use djinn_core::message::{ContentBlock, Message, Role};
        use djinn_db::{CreateSessionParams, SessionMessageRepository, SessionRepository};

        // ── 1. DB + fixtures ────────────────────────────────────────────
        let db = Database::open_in_memory().expect("db");
        db.ensure_initialized().await.expect("init db");
        let noop = EventBus::noop();

        let project = crate::test_helpers::create_test_project(&db).await;
        let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
        let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let task_run_id = uuid::Uuid::now_v7().to_string();

        // Create the task_run row so the session's FK constraint is satisfied.
        let task_run_repo = djinn_db::TaskRunRepository::new(db.clone());
        task_run_repo
            .create(djinn_db::CreateTaskRunParams {
                id: &task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .expect("create task_run");

        let session_repo = SessionRepository::new(db.clone(), noop.clone());
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "anthropic/claude-sonnet-4-20250514",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(&task_run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");

        // ≥2 messages so structural extraction runs
        let msg_repo = SessionMessageRepository::new(db.clone(), noop.clone());
        msg_repo
            .insert_messages_batch(
                &session.id,
                &task.id,
                &[
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::text("I'll help with that.")],
                        metadata: None,
                    },
                    Message {
                        role: Role::User,
                        content: vec![ContentBlock::text("Thanks!")],
                        metadata: None,
                    },
                ],
            )
            .await
            .expect("insert messages");

        // Store credential for "anthropic" — the real ExtractionCallbacks
        // adapter will load this through the agent-side credential loader.
        let credential_repo = CredentialRepository::new(db.clone(), noop.clone());
        credential_repo
            .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "sk-test-extraction", None)
            .await
            .expect("store credential");

        // ── 2. AgentContext with capturing event bus ────────────────────
        let captured_events: Arc<std::sync::Mutex<Vec<djinn_core::events::DjinnEventEnvelope>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = captured_events.clone();
        let capturing_bus = EventBus::new(move |event| {
            events_clone.lock().unwrap().push(event);
        });

        let mut agent =
            crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        agent.event_bus = capturing_bus;

        // ── 3. Invoke the real entrypoint ───────────────────────────────
        super::run_post_session_extraction(task.id.clone(), task_run_id.clone(), agent).await;

        // ── 4. Assert credential_loading lifecycle event was emitted ────
        // `resolve_model_and_credential` emits this event before calling
        // `load_provider_credential` → `ctx.callbacks.resolve_provider_credential`.
        // Its presence proves the real adapter/callback seam was exercised.
        // Clone the events so we can drop the lock before any `.await`.
        let events: Vec<_> = captured_events.lock().unwrap().clone();
        let credential_loading = events.iter().find(|e| {
            e.entity_type == "lifecycle"
                && e.action == "step"
                && e.payload.get("step").and_then(|v| v.as_str()) == Some("credential_loading")
        });
        assert!(
            credential_loading.is_some(),
            "expected credential_loading lifecycle event from \
             resolve_model_and_credential through the real \
             ExtractionCallbacks adapter; events captured: {events:?}"
        );
        let detail = &credential_loading.unwrap().payload;
        assert_eq!(
            detail
                .get("detail")
                .and_then(|d| d.get("provider_id"))
                .and_then(|v| v.as_str()),
            Some("anthropic"),
            "credential_loading event must reference the anthropic provider"
        );

        // ── 5. Structural extraction completed ─────────────────────────
        let taxonomy_json = session_repo
            .get_event_taxonomy_json(&session.id)
            .await
            .expect("get taxonomy");
        assert!(
            taxonomy_json.is_some(),
            "structural extraction must store event_taxonomy on the session"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extraction_credential_errors_include_provider_context() {
        let db = Database::open_in_memory().expect("db");
        db.ensure_initialized().await.expect("init db");
        let agent = crate::test_helpers::agent_context_from_db(db, CancellationToken::new());
        let slot_ctx = agent_to_slot_context(&agent);

        let err = match slot_ctx
            .callbacks
            .resolve_provider_credential("missing-provider", &slot_ctx)
            .await
        {
            Ok(_) => panic!("missing credential should fail"),
            Err(err) => err,
        };

        assert!(err.contains("missing-provider"), "error was: {err}");
        assert!(
            !err.contains("not available in extraction backfill"),
            "error was: {err}"
        );
    }

    /// Regression: prove the boot-time extraction backfill entrypoint reaches
    /// slot-side credential resolution through the real `agent_to_slot_context`
    /// / `ExtractionCallbacks` adapter — not through an injected-provider
    /// helper or a direct unit call to `resolve_provider_credential`.
    ///
    /// Sets up minimal DB fixtures (project, task, completed task-run, session
    /// with messages and a stored credential), calls `run_extraction_backfill`
    /// (the real agent boot-time backfill entrypoint), and asserts that:
    /// 1. The backfill candidate query picks up the completed-but-unextracted
    ///    task-run session.
    /// 2. `resolve_model_and_credential` emits a `credential_loading` lifecycle
    ///    event (proving the model-resolution → credential-loading path ran
    ///    through the real adapter seam).
    /// 3. Structural extraction completed (event_taxonomy stored on session).
    ///
    /// The LLM call itself fails gracefully (fake key, no network); the proof
    /// is that the credential-resolution callback was invoked through the real
    /// adapter seam via the backfill candidate traversal, not bypassed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extraction_backfill_reaches_credential_resolution_through_real_adapter() {
        use std::sync::Arc;

        use djinn_core::events::EventBus;
        use djinn_core::message::{ContentBlock, Message, Role};
        use djinn_db::{CreateSessionParams, SessionMessageRepository, SessionRepository};

        // ── 1. DB + fixtures ────────────────────────────────────────────
        let db = Database::open_in_memory().expect("db");
        db.ensure_initialized().await.expect("init db");
        let noop = EventBus::noop();

        let project = crate::test_helpers::create_test_project(&db).await;
        let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
        let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let task_run_id = uuid::Uuid::now_v7().to_string();

        // Create a COMPLETED task_run so the backfill candidate query
        // (`list_unextracted_completed_candidates`) picks it up.
        let task_run_repo = djinn_db::TaskRunRepository::new(db.clone());
        task_run_repo
            .create(djinn_db::CreateTaskRunParams {
                id: &task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: Some("completed"),
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .expect("create completed task_run");

        // Session linked to the completed task_run, with event_taxonomy NULL
        // (the default) so the backfill query considers it unextracted.
        let session_repo = SessionRepository::new(db.clone(), noop.clone());
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "anthropic/claude-sonnet-4-20250514",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(&task_run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");

        // ≥2 messages so structural extraction runs
        let msg_repo = SessionMessageRepository::new(db.clone(), noop.clone());
        msg_repo
            .insert_messages_batch(
                &session.id,
                &task.id,
                &[
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::text("I'll help with that.")],
                        metadata: None,
                    },
                    Message {
                        role: Role::User,
                        content: vec![ContentBlock::text("Thanks!")],
                        metadata: None,
                    },
                ],
            )
            .await
            .expect("insert messages");

        // Store credential for "anthropic" — the real ExtractionCallbacks
        // adapter will load this through the agent-side credential loader.
        let credential_repo = CredentialRepository::new(db.clone(), noop.clone());
        credential_repo
            .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "sk-test-backfill", None)
            .await
            .expect("store credential");

        // ── 2. AgentContext with capturing event bus ────────────────────
        let captured_events: Arc<std::sync::Mutex<Vec<djinn_core::events::DjinnEventEnvelope>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = captured_events.clone();
        let capturing_bus = EventBus::new(move |event| {
            events_clone.lock().unwrap().push(event);
        });

        let mut agent =
            crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        agent.event_bus = capturing_bus;

        // ── 3. Invoke the real backfill entrypoint ─────────────────────
        // This is the boot-time recovery sweep. It queries
        // `list_unextracted_completed_candidates()`, finds our completed
        // task-run, and delegates to `run_post_session_extraction` which
        // exercises the full `agent_to_slot_context` → callback seam.
        super::run_extraction_backfill(agent).await;

        // ── 4. Assert credential_loading lifecycle event was emitted ────
        // `resolve_model_and_credential` emits this event before calling
        // `load_provider_credential` → `ctx.callbacks.resolve_provider_credential`.
        // Its presence proves the real adapter/callback seam was exercised
        // through the backfill candidate traversal path.
        let events: Vec<_> = captured_events.lock().unwrap().clone();
        let credential_loading = events.iter().find(|e| {
            e.entity_type == "lifecycle"
                && e.action == "step"
                && e.payload.get("step").and_then(|v| v.as_str()) == Some("credential_loading")
        });
        assert!(
            credential_loading.is_some(),
            "expected credential_loading lifecycle event from \
             resolve_model_and_credential through the real \
             ExtractionCallbacks adapter via backfill; events captured: {events:?}"
        );
        let detail = &credential_loading.unwrap().payload;
        assert_eq!(
            detail
                .get("detail")
                .and_then(|d| d.get("provider_id"))
                .and_then(|v| v.as_str()),
            Some("anthropic"),
            "credential_loading event must reference the anthropic provider"
        );

        // ── 5. Structural extraction completed ─────────────────────────
        let taxonomy_json = session_repo
            .get_event_taxonomy_json(&session.id)
            .await
            .expect("get taxonomy");
        assert!(
            taxonomy_json.is_some(),
            "structural extraction must store event_taxonomy on the session"
        );
    }
}

/// One-shot recovery sweep that backfills post-session knowledge extraction
/// over completed-but-unextracted task-runs.
///
/// This is the public entry point called from the server boot path.
/// It delegates to `djinn_slot::run_extraction_backfill` after converting
/// the `AgentContext` into a `SlotContext`.
pub async fn run_extraction_backfill(app_state: AgentContext) {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_extraction_backfill(slot_ctx).await;
}

/// Server-side post-task-run knowledge extraction (thin adapter).
///
/// Converts `AgentContext` → `SlotContext` and delegates to
/// `djinn_slot::run_post_session_extraction`.
pub(crate) async fn run_post_session_extraction(
    task_id: String,
    task_run_id: String,
    app_state: AgentContext,
) {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_post_session_extraction(task_id, task_run_id, slot_ctx).await;
}
