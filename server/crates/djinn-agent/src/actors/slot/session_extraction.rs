// Session extraction shim: converts AgentContext → SlotContext and delegates
// to canonical djinn-slot extraction.

use crate::context::AgentContext;
use djinn_slot::TerminalExtractionContext;

use super::adapter::agent_to_slot_context;

/// One-shot recovery sweep that backfills post-session knowledge extraction
/// over completed-but-unextracted task-runs. Called from the server boot path.
pub async fn run_extraction_backfill(app_state: AgentContext) {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_extraction_backfill(slot_ctx).await;
}

/// Server-side post-task-run knowledge extraction (thin adapter).
pub(crate) async fn run_post_session_extraction(
    task_id: String,
    task_run_id: String,
    terminal_context: TerminalExtractionContext,
    app_state: AgentContext,
) {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_post_session_extraction(task_id, task_run_id, terminal_context, slot_ctx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::Database;
    use djinn_provider::repos::CredentialRepository;
    use tokio_util::sync::CancellationToken;
    struct ExtractionFixtures {
        db: Database,
        session_id: String,
        captured_events:
            std::sync::Arc<std::sync::Mutex<Vec<djinn_core::events::DjinnEventEnvelope>>>,
    }
    /// Common setup: DB, project/epic/task, task_run, session + messages,
    /// credential, event-capturing bus, and dispatch to the extraction fn.
    async fn setup_extraction_fixtures(
        credential_key: &str,
        credential_value: &str,
        task_run_status: Option<&str>,
        action: &str,
    ) -> ExtractionFixtures {
        use djinn_core::events::EventBus;
        use djinn_core::message::{ContentBlock, Message, Role};
        use djinn_db::{CreateSessionParams, SessionMessageRepository, SessionRepository};
        use std::sync::Arc;
        let db = Database::open_in_memory().expect("db");
        db.ensure_initialized().await.expect("init db");
        let noop = EventBus::noop();
        let project = crate::test_helpers::create_test_project(&db).await;
        let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
        let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let task_run_id = uuid::Uuid::now_v7().to_string();
        djinn_db::TaskRunRepository::new(db.clone())
            .create(djinn_db::CreateTaskRunParams {
                id: &task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: task_run_status,
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
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
        let session_id = session.id.clone();
        SessionMessageRepository::new(db.clone(), noop.clone())
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
        CredentialRepository::new(db.clone(), noop.clone())
            .set_with_owner("anthropic", credential_key, credential_value, None)
            .await
            .expect("store credential");
        let captured_events: Arc<std::sync::Mutex<Vec<djinn_core::events::DjinnEventEnvelope>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = captured_events.clone();
        let capturing_bus = EventBus::new(move |event| {
            events_clone.lock().unwrap().push(event);
        });
        let mut agent =
            crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        agent.event_bus = capturing_bus;
        match action {
            "post_session" => {
                run_post_session_extraction(
                    task.id.clone(),
                    task_run_id.clone(),
                    TerminalExtractionContext::unknown_historical(),
                    agent,
                )
                .await;
            }
            "backfill" => run_extraction_backfill(agent).await,
            _ => panic!("unknown action: {action}"),
        }
        ExtractionFixtures {
            db,
            session_id,
            captured_events,
        }
    }
    fn assert_credential_loading_event(
        f: &ExtractionFixtures,
        expected_provider: &str,
        label: &str,
    ) {
        let events = f.captured_events.lock().unwrap().clone();
        let cred_evt = events.iter().find(|e| {
            e.entity_type == "lifecycle"
                && e.action == "step"
                && e.payload.get("step").and_then(|v| v.as_str()) == Some("credential_loading")
        });
        assert!(
            cred_evt.is_some(),
            "expected credential_loading lifecycle event from {label}; events: {events:?}"
        );
        assert_eq!(
            cred_evt
                .unwrap()
                .payload
                .get("detail")
                .and_then(|d| d.get("provider_id"))
                .and_then(|v| v.as_str()),
            Some(expected_provider),
        );
    }
    async fn assert_taxonomy_stored(f: &ExtractionFixtures) {
        use djinn_db::SessionRepository;
        let session_repo =
            SessionRepository::new(f.db.clone(), djinn_core::events::EventBus::noop());
        let taxonomy_json = session_repo
            .get_event_taxonomy_json(&f.session_id)
            .await
            .expect("get taxonomy");
        assert!(
            taxonomy_json.is_some(),
            "structural extraction must store event_taxonomy"
        );
    }
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_session_extraction_reaches_credential_resolution_through_real_adapter() {
        let f = setup_extraction_fixtures(
            "ANTHROPIC_API_KEY",
            "sk-test-extraction",
            None,
            "post_session",
        )
        .await;
        assert_credential_loading_event(&f, "anthropic", "the real ExtractionCallbacks adapter");
        assert_taxonomy_stored(&f).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extraction_backfill_reaches_credential_resolution_through_real_adapter() {
        let f = setup_extraction_fixtures(
            "ANTHROPIC_API_KEY",
            "sk-test-backfill",
            Some("completed"),
            "backfill",
        )
        .await;
        assert_credential_loading_event(
            &f,
            "anthropic",
            "the real ExtractionCallbacks adapter via backfill",
        );
        assert_taxonomy_stored(&f).await;
    }
}
