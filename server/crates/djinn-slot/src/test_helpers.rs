//! Shared test utilities for djinn-slot tests.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("open in-memory test database")
}

pub fn test_events() -> EventBus {
    EventBus::noop()
}

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("failed to create tempdir")
}

pub fn test_path(prefix: &str) -> std::path::PathBuf {
    test_tempdir(prefix).keep()
}

pub fn agent_context_from_db(db: Database, _cancel: CancellationToken) -> SlotContext {
    let event_bus = test_events();
    let catalog = djinn_provider::catalog::CatalogService::new();
    let health_tracker = djinn_provider::catalog::HealthTracker::default();
    let background_work =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // No-op host callbacks for tests
    struct NoopCallbacks;
    impl crate::host::SlotHostCallbacks for NoopCallbacks {
        fn interrupt_paused_worker_session<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn resolve_mcp_tools<'a>(
            &'a self,
            _worktree_path: &'a str,
            _role_name: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::host::ResolvedMcpTools, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in test".into()) })
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
            _ctx: &'a SlotContext,
        ) -> Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async { String::new() })
        }
        fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
            panic!(
                "build_mcp_state not implemented in test NoopCallbacks; \
                 override via a custom SlotHostCallbacks impl if your test needs McpState"
            )
        }
        fn require_project_id_for_task_ops<'a>(
            &'a self,
            _project: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            String,
                            djinn_control_plane::tools::task_tools::ErrorResponse,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                    error: "not implemented".into(),
                })
            })
        }
        fn resolve_provider_credential<'a>(
            &'a self,
            _provider_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::helpers::ProviderCredential, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in test".into()) })
        }
        fn run_task_dispatch<'a>(
            &'a self,
            _task_id: String,
            _project_path: String,
            _model_id: String,
            _ctx: SlotContext,
            _kill: tokio_util::sync::CancellationToken,
            _pause: tokio_util::sync::CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    SlotContext {
        db,
        event_bus,
        catalog,
        health_tracker,
        background_work_tasks: background_work,
        active_tasks,
        default_project_id: None,
        working_root: None,
        coordinator_trigger: None,
        runtime_ops: None,
        repo_graph_ops: None,
        callbacks: std::sync::Arc::new(NoopCallbacks),
    }
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let event_bus = test_events();
    let repo = djinn_db::ProjectRepository::new(db.clone(), event_bus);
    repo.create("test-project", "Test Project", "main")
        .await
        .expect("create project")
}

pub async fn create_test_epic(db: &Database, project_id: &str) -> djinn_core::models::Epic {
    let event_bus = test_events();
    let repo = djinn_db::EpicRepository::new(db.clone(), event_bus);
    repo.create_for_project(
        project_id,
        djinn_db::EpicCreateInput {
            title: "test-epic",
            description: "test epic description",
            emoji: "🧪",
            color: "blue",
            owner: "test-owner",
            memory_refs: None,
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
            blocked_by: None,
        },
    )
    .await
    .expect("create epic")
}

pub async fn create_test_task(
    db: &Database,
    project_id: &str,
    epic_id: &str,
) -> djinn_core::models::Task {
    let event_bus = test_events();
    let repo = djinn_db::TaskRepository::new(db.clone(), event_bus);
    repo.create_in_project(
        project_id,
        Some(epic_id),
        "Test task",
        "Test task description",
        "",
        "task",
        2,
        "test-owner",
        None,
        None,
    )
    .await
    .expect("create task")
}

// ─── FakeProvider ────────────────────────────────────────────────────────────

/// Pre-scripted `LlmProvider` for tests. Each "turn" is a list of
/// `StreamEvent`s that will be returned in order.
pub struct FakeProvider {
    scripted_turns: Arc<StdMutex<VecDeque<Vec<anyhow::Result<StreamEvent>>>>>,
}

impl FakeProvider {
    /// Create a provider that returns a single text-only turn.
    pub fn text(text: impl Into<String>) -> Self {
        Self::script(vec![vec![
            StreamEvent::Delta(ContentBlock::Text { text: text.into() }),
            StreamEvent::Done,
        ]])
    }

    /// Create a provider that returns a single tool-call turn.
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::script(vec![vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }),
            StreamEvent::Done,
        ]])
    }

    /// Create a provider with a fully custom sequence of turns.
    pub fn script(turns: Vec<Vec<StreamEvent>>) -> Self {
        let scripted_turns = turns
            .into_iter()
            .map(|turn| turn.into_iter().map(Ok).collect())
            .collect();
        Self {
            scripted_turns: Arc::new(StdMutex::new(scripted_turns)),
        }
    }

    /// How many scripted turns remain.
    pub fn remaining(&self) -> usize {
        self.scripted_turns.lock().unwrap().len()
    }
}

impl LlmProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [Value],
        _tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let scripted_turns = Arc::clone(&self.scripted_turns);
        Box::pin(async move {
            let mut turns = scripted_turns.lock().expect("scripted_turns mutex");
            let events = turns
                .pop_front()
                .unwrap_or_else(|| vec![Ok(StreamEvent::Done)]);
            Ok(Box::pin(futures::stream::iter(events))
                as Pin<Box<dyn futures::Stream<Item = _> + Send>>)
        })
    }
}

// ─── FailingProvider ─────────────────────────────────────────────────────────

/// A `LlmProvider` that always returns an error.
pub struct FailingProvider {
    message: Arc<String>,
}

impl FailingProvider {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Arc::new(message.into()),
        }
    }
}

impl Default for FailingProvider {
    fn default() -> Self {
        Self::new("scripted provider failure")
    }
}

impl LlmProvider for FailingProvider {
    fn name(&self) -> &str {
        "failing"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [Value],
        _tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let message = Arc::clone(&self.message);
        Box::pin(async move { Err(anyhow::anyhow!(message.as_str().to_owned())) })
    }
}
