//! Shared test utilities for djinn-slot tests.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use futures::stream;

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::host::{SlotContext, SlotToolDispatcher};
use crate::reply_loop::CompactionCriticalSection;

/// Minimal `SlotToolDispatcher` for tests that exercise the reply loop.
/// Stash tools (`output_view`/`output_grep`) return stub text; extension
/// and MCP tools return errors because tests should not reach them.
pub struct MockToolDispatcher;

impl SlotToolDispatcher for MockToolDispatcher {
    fn is_stash_tool(&self, tool_name: &str) -> bool {
        tool_name == "output_view" || tool_name == "output_grep"
    }
    fn handle_stash_call(
        &self,
        tool_name: &str,
        _arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, String> {
        // Mock has no actual stash content — return "not found" error
        // so tool results carry is_error: true (matching test expectations).
        Err(format!("no stashed output for tool_use_id in {tool_name}"))
    }
    fn render_result(
        &self,
        _tool_use_id: &str,
        _tool_name: &str,
        value: &serde_json::Value,
    ) -> String {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    }
    fn externalize_rendered_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        rendered: &str,
        preview_chars: usize,
    ) -> String {
        mock_externalize_rendered_result(tool_use_id, tool_name, rendered, preview_chars)
    }
    fn dispatch_extension_tool<'a>(
        &'a self,
        tool_name: &'a str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
        _worktree_path: &'a std::path::Path,
        _task_id: &'a str,
        _role_name: &'a str,
    ) -> Pin<
        Box<dyn std::future::Future<Output = djinn_core::tool_call::ToolCallOutcome> + Send + 'a>,
    > {
        Box::pin(async move {
            djinn_core::tool_call::ToolCallOutcome::from_result(match tool_name {
                // Return a successful stub for common test tools.
                "shell" => Ok(serde_json::json!({
                    "ok": true,
                    "exit_code": 0,
                    "stdout": "mock shell output\n",
                    "stderr": "",
                    "workdir": "/tmp"
                })),
                "read" | "code_search" | "write" | "edit" | "apply_patch" => {
                    Ok(serde_json::json!({"ok": true}))
                }
                _ => Err(format!(
                    "MockToolDispatcher: extension tool '{tool_name}' not implemented in test"
                )),
            })
        })
    }
    fn is_mcp_tool(&self, _tool_name: &str) -> bool {
        false
    }
    fn dispatch_mcp_tool<'a>(
        &'a self,
        tool_name: &'a str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(format!(
                "MockToolDispatcher: MCP tool '{tool_name}' not implemented in test"
            ))
        })
    }
    fn mcp_server_for_tool(&self, _tool_name: &str) -> Option<String> {
        None
    }
    fn is_resource_tool(&self, _tool_name: &str) -> bool {
        false
    }
    fn dispatch_resource_tool<'a>(
        &'a self,
        tool_name: &'a str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            Err(format!(
                "MockToolDispatcher: resource tool '{tool_name}' not implemented in test"
            ))
        })
    }
    fn clear_stash(&self) {}
}

use std::collections::HashMap;

/// Handler function pointer for `ConfigurableToolDispatcher`.
pub type ToolHandlerFn =
    fn(Option<&serde_json::Map<String, serde_json::Value>>) -> Result<serde_json::Value, String>;

/// A `SlotToolDispatcher` that routes specific tool names to closures.
/// Useful for tests that need specific error messages (e.g., "permission denied")
/// to trigger loop guard classifications.
pub struct ConfigurableToolDispatcher {
    /// Tool names that should be treated as MCP tools.
    mcp_tools: Vec<String>,
    /// Native MCP resource tool names and their successful text results.
    resource_results: HashMap<String, String>,
    /// Map from tool name → handler that returns the dispatch result.
    /// Extension tools not in this map get a generic error.
    handlers: HashMap<String, ToolHandlerFn>,
}

impl ConfigurableToolDispatcher {
    pub fn new(mcp_tools: Vec<String>, handlers: HashMap<String, ToolHandlerFn>) -> Self {
        Self {
            mcp_tools,
            resource_results: HashMap::new(),
            handlers,
        }
    }

    /// Configure successful native MCP resource results for dispatch tests.
    /// Resource tools return text directly, unlike extension and MCP tools,
    /// which render JSON values through `render_result`.
    pub fn with_resource_results(mut self, resource_results: HashMap<String, String>) -> Self {
        self.resource_results = resource_results;
        self
    }
}

impl SlotToolDispatcher for ConfigurableToolDispatcher {
    fn is_stash_tool(&self, tool_name: &str) -> bool {
        tool_name == "output_view" || tool_name == "output_grep"
    }
    fn handle_stash_call(
        &self,
        tool_name: &str,
        _arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, String> {
        Ok(format!("[mock stash result for {tool_name}]"))
    }
    fn render_result(
        &self,
        _tool_use_id: &str,
        _tool_name: &str,
        value: &serde_json::Value,
    ) -> String {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    }
    fn externalize_rendered_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        rendered: &str,
        preview_chars: usize,
    ) -> String {
        mock_externalize_rendered_result(tool_use_id, tool_name, rendered, preview_chars)
    }
    fn dispatch_extension_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        _worktree_path: &'a std::path::Path,
        _task_id: &'a str,
        _role_name: &'a str,
    ) -> Pin<
        Box<dyn std::future::Future<Output = djinn_core::tool_call::ToolCallOutcome> + Send + 'a>,
    > {
        let result = if let Some(handler) = self.handlers.get(tool_name) {
            handler(arguments.as_ref())
        } else {
            Err(format!(
                "ConfigurableToolDispatcher: tool '{tool_name}' not configured"
            ))
        };
        Box::pin(async move { djinn_core::tool_call::ToolCallOutcome::from_result(result) })
    }
    fn is_mcp_tool(&self, tool_name: &str) -> bool {
        self.mcp_tools.iter().any(|t| t == tool_name)
    }
    fn dispatch_mcp_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>>
    {
        let result = if let Some(handler) = self.handlers.get(tool_name) {
            handler(arguments.as_ref())
        } else {
            Err(format!(
                "ConfigurableToolDispatcher: MCP tool '{tool_name}' not configured"
            ))
        };
        Box::pin(async move { result })
    }
    fn mcp_server_for_tool(&self, tool_name: &str) -> Option<String> {
        if self.mcp_tools.iter().any(|t| t == tool_name) {
            Some(format!("mock-server-{tool_name}"))
        } else {
            None
        }
    }
    fn is_resource_tool(&self, tool_name: &str) -> bool {
        self.resource_results.contains_key(tool_name)
    }
    fn dispatch_resource_tool<'a>(
        &'a self,
        tool_name: &'a str,
        _arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
        let result = self
            .resource_results
            .get(tool_name)
            .cloned()
            .ok_or_else(|| {
                format!("ConfigurableToolDispatcher: resource tool '{tool_name}' not configured")
            });
        Box::pin(async move { result })
    }
    fn clear_stash(&self) {}
}

/// Extract concise text from a tool result for stashing/display.
/// Returns `None` for non-shell tools.
///
/// This is a slot-local copy of the agent's `output_stash::extract_stash_content`
/// so that reply-loop tests do not depend on `djinn-agent`.
pub fn extract_stash_content(tool_name: &str, value: &serde_json::Value) -> Option<String> {
    if tool_name != "shell" {
        return None;
    }
    let obj = value.as_object()?;
    let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let exit_code = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let mut out = String::with_capacity(stdout.len() + stderr.len() + 64);
    if !stdout.is_empty() {
        out.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("--- stderr ---\n");
        out.push_str(stderr);
    }
    if exit_code != 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("[exit code: {exit_code}]"));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Slot-local copy of the agent's output-stash header escaping so mock
/// dispatchers emit the same canonical, parseable header without depending on
/// `djinn-agent`.
fn escape_stash_header_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Slot-local mock of the agent's `externalize_rendered_tool_result`.
///
/// Test dispatchers (`MockToolDispatcher`, `ConfigurableToolDispatcher`) share
/// this so future reply-loop unit tests can assert turn-budget externalization
/// happened deterministically without depending on `djinn-agent`.
///
/// The mock emits the canonical `[djinn-output-stash ...]` header so tests can
/// grep for it, and truncates the preview to `preview_chars` characters with a
/// simple head cut (not the real `smart_truncate`). When the stub would not be
/// smaller than `rendered`, the original text is returned unchanged — mirroring
/// the real non-shrinking guard so tests observe the same contract.
pub fn mock_externalize_rendered_result(
    tool_use_id: &str,
    tool_name: &str,
    rendered: &str,
    preview_chars: usize,
) -> String {
    let full_chars = rendered.chars().count();
    let preview_chars = preview_chars.max(1);

    // Simple head-cut preview for the mock (not smart_truncate).
    let preview_body: String = rendered.chars().take(preview_chars).collect();
    let preview_body_chars = preview_body.chars().count();

    let header = format!(
        "[djinn-output-stash tool_use_id=\"{}\" tool_name=\"{}\" reason=\"turn_budget\" full_chars=\"{}\" preview_chars=\"{}\"]",
        escape_stash_header_value(tool_use_id),
        escape_stash_header_value(tool_name),
        full_chars,
        preview_body_chars,
    );

    let stub =
        format!("{header}\n{preview_body}\n\n[mock externalized output — {full_chars} chars]");

    // Non-shrinking guard (same contract as the real helper).
    if stub.chars().count() >= full_chars {
        return rendered.to_string();
    }

    stub
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod externalization_seam_tests {
    use super::*;

    #[test]
    fn mock_dispatcher_exposes_turn_budget_externalization_through_slot_trait() {
        let dispatcher: Arc<dyn SlotToolDispatcher> = Arc::new(MockToolDispatcher);
        let rendered = "0123456789".repeat(80);

        let stub =
            dispatcher.externalize_rendered_result("call-turn-budget-1", "shell", &rendered, 12);

        assert!(stub.len() < rendered.len());
        assert!(stub.starts_with(
            "[djinn-output-stash tool_use_id=\"call-turn-budget-1\" tool_name=\"shell\" reason=\"turn_budget\""
        ));
        assert!(stub.contains("full_chars=\"800\""));
        assert!(stub.contains("preview_chars=\"12\""));
        assert!(stub.contains("\n012345678901\n"));
    }

    #[test]
    fn configurable_dispatcher_uses_same_non_shrinking_contract() {
        let dispatcher: Arc<dyn SlotToolDispatcher> =
            Arc::new(ConfigurableToolDispatcher::new(Vec::new(), HashMap::new()));
        let rendered = "short result";

        let output = dispatcher.externalize_rendered_result("call-small-1", "read", rendered, 4);

        assert_eq!(output, rendered);
    }

    #[test]
    fn mock_externalize_escapes_header_values() {
        let dispatcher: Arc<dyn SlotToolDispatcher> = Arc::new(MockToolDispatcher);
        let rendered = "0123456789".repeat(80);

        let stub =
            dispatcher.externalize_rendered_result("call-\\\"é", "tool-\\\"name", &rendered, 12);

        assert!(stub.starts_with(
            "[djinn-output-stash tool_use_id=\"call-\\\\\\\"é\" tool_name=\"tool-\\\\\\\"name\" reason=\"turn_budget\""
        ));
    }
}

/// Map a `StageOutcome` to `(SessionStatus, Option<park_reason>)`.
///
/// Slot-local copy of the agent's `session_settlement_for_stage_outcome` so
/// reply-loop tests can exercise budget-park settlement without depending on
/// `djinn-agent`.
pub fn test_session_settlement_for_stage_outcome(
    stage_outcome: &djinn_supervisor::StageOutcome,
    final_result_ok: bool,
) -> (djinn_core::models::SessionStatus, Option<String>) {
    use djinn_core::models::SessionStatus;
    use djinn_supervisor::{ParkReason, StageOutcome};
    match stage_outcome {
        StageOutcome::Parked {
            reason: ParkReason::Budget,
            ..
        } => (SessionStatus::Completed, Some("budget".to_string())),
        _ if final_result_ok => (SessionStatus::Completed, None),
        _ => (SessionStatus::Failed, None),
    }
}

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("open in-memory test database")
}

/// Cheap `SupervisorServices` stub for tests that exercise non-host-bound tool
/// paths. This mirrors the agent test helper without pulling in any
/// `djinn-agent`-only state.
pub fn test_services() -> djinn_supervisor::services::rpc::UnimplementedRpcServices {
    djinn_supervisor::services::rpc::UnimplementedRpcServices::new()
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
    agent_context_from_db_with_dispatcher(db, _cancel, Some(Arc::new(MockToolDispatcher)))
}

/// Build a `SlotContext` from an in-memory DB with custom host callbacks.
/// This lets tests override final-verification resolution and lease behavior
/// without going through the production `AgentHostCallbacks`.
pub fn agent_context_from_db_with_callbacks(
    db: Database,
    callbacks: Arc<dyn crate::host::SlotHostCallbacks>,
) -> SlotContext {
    let event_bus = test_events();
    let catalog = djinn_provider::catalog::CatalogService::new();
    let health_tracker = djinn_provider::catalog::HealthTracker::default();
    let background_work =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
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
        clock: std::sync::Arc::new(djinn_core::clock::SystemClock::new()),
        callbacks,
        tool_dispatcher: Some(Arc::new(MockToolDispatcher)),
        compaction_cs: CompactionCriticalSection::new(),
    }
}

/// Build a `SlotContext` from an in-memory DB with an explicit tool dispatcher.
/// Pass `None` for `tool_dispatcher` to test the "no dispatcher" error path.
pub fn agent_context_from_db_with_dispatcher(
    db: Database,
    _cancel: CancellationToken,
    tool_dispatcher: Option<Arc<dyn SlotToolDispatcher>>,
) -> SlotContext {
    let event_bus = test_events();
    let catalog = djinn_provider::catalog::CatalogService::new();
    let health_tracker = djinn_provider::catalog::HealthTracker::default();
    let background_work =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // No-op host callbacks for tests
    struct NoopCallbacks;
    impl crate::host::SlotHostCallbacks for NoopCallbacks {
        fn final_verification_outcome_for_test(
            &self,
            _request: &crate::final_verification::FinalVerificationCoordinatorRequest,
        ) -> Option<crate::final_verification::FinalVerificationRecordingOutcome> {
            Some(
                crate::final_verification::FinalVerificationRecordingOutcome::Stored {
                    verification_attempt_id: uuid::Uuid::now_v7().to_string(),
                    verify_run_id: uuid::Uuid::now_v7().to_string(),
                    evidence: Box::new(
                        crate::final_verification::FinalVerificationSuccessEvidence {
                            persisted_run_id: uuid::Uuid::now_v7().to_string(),
                            completed_at: "2025-01-01T00:00:00Z".to_owned(),
                            ordered_commands: serde_json::json!([]),
                            covered_checks: serde_json::json!([]),
                            required_checks: vec![],
                            verification_input_fingerprint: "test-fingerprint".to_owned(),
                            manifest_version: "manifest-v1".to_owned(),
                            environment_identity_digest: "test-identity".to_owned(),
                        },
                    ),
                },
            )
        }

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
            _resume_lifecycle_metadata: Option<serde_json::Value>,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn touch_activity_rpc<'a>(
            &'a self,
            _task_id: String,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn flush_session_tokens_rpc<'a>(
            &'a self,
            _session_id: String,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
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
        clock: std::sync::Arc::new(djinn_core::clock::SystemClock::new()),
        callbacks: std::sync::Arc::new(NoopCallbacks),
        tool_dispatcher,
        compaction_cs: CompactionCriticalSection::new(),
    }
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let event_bus = test_events();
    let repo = djinn_db::ProjectRepository::new(db.clone(), event_bus);
    let uuid = uuid::Uuid::now_v7().simple();
    repo.create(
        &format!("test-project-{uuid}"),
        &format!("owner-{uuid}"),
        &format!("repo-{uuid}"),
    )
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

/// Pre-built test fixture: DB + project + epic + task.
/// Returns `(db, project, epic, task)` to reduce the 4-call setup pattern
/// repeated across nearly every test module.
pub struct FullFixture {
    pub db: Database,
    pub project: djinn_core::models::Project,
    pub epic: djinn_core::models::Epic,
    pub task: djinn_core::models::Task,
}

/// Pre-built test fixture: DB + SlotContext + project + epic + task.
/// Returns the complete fixture including the SlotContext so tests don't
/// need to construct it separately.
pub struct ContextFixture {
    pub db: Database,
    pub ctx: SlotContext,
    pub project: djinn_core::models::Project,
    pub epic: djinn_core::models::Epic,
    pub task: djinn_core::models::Task,
}

pub async fn seed_context_fixture() -> ContextFixture {
    let db = create_test_db();
    let ctx = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    ContextFixture {
        db,
        ctx,
        project,
        epic,
        task,
    }
}

pub async fn seed_full_fixture() -> FullFixture {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    FullFixture {
        db,
        project,
        epic,
        task,
    }
}

/// Generic tool schema with `concurrent_safe: false` (default for most tests).
pub fn dummy_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "concurrent_safe": false
    })
}

/// Tool schema with explicit `concurrent_safe` flag.
pub fn dummy_tool_schema_with_safety(name: &str, concurrent_safe: bool) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "readOnly": concurrent_safe,
        "destructive": false,
        "idempotent": concurrent_safe,
        "openWorld": false,
        "concurrent_safe": concurrent_safe
    })
}

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
    /// Create a provider whose scripted turns are followed by an explicit
    /// terminal stream error. This lets reply-loop tests exercise recoverable
    /// turns before terminating without relying on script-exhaustion panic.
    pub fn script_with_terminal_error(
        turns: Vec<Vec<StreamEvent>>,
        message: impl Into<String>,
    ) -> Self {
        let mut scripted_turns: VecDeque<Vec<anyhow::Result<StreamEvent>>> = turns
            .into_iter()
            .map(|turn| turn.into_iter().map(Ok).collect())
            .collect();
        scripted_turns.push_back(vec![Err(anyhow::anyhow!(message.into()))]);
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
            let events = scripted_turns
                .lock()
                .expect("scripted_turns mutex")
                .pop_front()
                .unwrap_or_else(|| {
                    panic!(
                        "FakeProvider script exhausted: stream() called with no scripted turns remaining"
                    )
                });
            Ok(Box::pin(stream::iter(events)) as Pin<Box<dyn futures::Stream<Item = _> + Send>>)
        })
    }
}

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
