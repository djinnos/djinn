//! Test utilities for djinn-agent tests.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Once};

use futures::stream;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use djinn_core::events::EventBus;
use djinn_core::models::Project;
use djinn_core::models::{Epic, Task};
use djinn_db::{
    Database, EffectiveCreatorProvenance, EpicCreateInput, EpicRepository, ProjectRepository,
    TaskRepository, UserRepository,
};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};

use crate::context::AgentContext;
use crate::file_time::FileTime;
use crate::lsp::LspManager;
use crate::roles::RoleRegistry;

/// Ensure the djinn-roles tool schema registry is initialized for tests.
///
/// Safe to call multiple times — uses `Once` internally.
pub fn ensure_tool_schemas_registered() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        crate::init_tool_schema_registry();
    });
}

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = test_tmp_base();
    std::fs::create_dir_all(&base).expect("create test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create test tempdir")
}

fn test_tmp_base() -> PathBuf {
    if let Ok(base) = std::env::var("CARGO_TARGET_TMPDIR") {
        let base = PathBuf::from(base).join("djinn-agent");
        if base.is_relative() {
            std::env::current_dir().expect("current dir").join(base)
        } else {
            base
        }
    } else {
        std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("test-tmp")
    }
}

pub fn test_persistent_dir(prefix: &str) -> std::path::PathBuf {
    test_tempdir(prefix).keep()
}

pub fn test_path(prefix: &str) -> std::path::PathBuf {
    test_persistent_dir(prefix)
}
/// Invoke the agent dispatcher production pre-compaction durability boundary.
pub fn persist_tool_results_before_compaction_for_test(
    stash: &mut crate::output_stash::OutputStash,
    results: &[djinn_slot::host::PreCompactionToolResult],
) -> Result<Vec<djinn_compaction::ToolOutputPointer>, String> {
    crate::actors::slot::reply_loop::persist_tool_results_before_compaction_for_test(stash, results)
}

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

pub fn test_events() -> EventBus {
    EventBus::noop()
}

static NEXT_FIXTURE_GITHUB_ID: AtomicI64 = AtomicI64::new(9_100_000_000);

/// Persist a real, collision-free user for task attribution fixtures.
pub(crate) async fn create_test_creator(db: &Database) -> djinn_db::User {
    let github_id = NEXT_FIXTURE_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("djinn-agent-fixture-{github_id}"),
            Some("Djinn Agent Fixture"),
            None,
        )
        .await
        .expect("failed to create test task creator")
}

pub fn agent_context_from_db(db: Database, _cancel: CancellationToken) -> AgentContext {
    AgentContext {
        db,
        event_bus: EventBus::noop(),
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        background_work_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        role_registry: Arc::new(RoleRegistry::new()),
        health_tracker: HealthTracker::new(),
        file_time: Arc::new(FileTime::new()),
        lsp: LspManager::new(),
        catalog: CatalogService::new(),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: crate::context::ActivityTracker::default(),
        task_ops_project_path_override: None,
        working_root: None,
        graph_warmer: None,
        repo_graph_ops: None,
        runtime_ops: None,
        cargo_target_runs_root: Some(test_path("cargo-target-runs-")),
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        read_source_authorization: crate::context::ReadSourceAuthorization::default(),
        memory_intent_planner: crate::context::MemoryIntentPlannerConfig::default(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        reconciliation_sweep: crate::context::ReconciliationSweepConfig::default(),
        shell_launch: None,
        compaction_cs: djinn_slot::reply_loop::CompactionCriticalSection::default(),
    }
}

/// Invoke the real production `load_knowledge_context` path for integration tests.
///
/// This is the same orchestration used by dispatch: it runs the production
/// scope-overlap query, the capped trace-candidate query, deterministic
/// classification, prompt packing, and fail-open trace persistence.  Exposed
/// under `test-support` so the MCP tool tests in `djinn-control-plane` can
/// prove the tracedrop outcomes without reimplementing the classifier.
pub async fn run_load_knowledge_context_for_test(
    task: &Task,
    epic_context: Option<&str>,
    app_state: &AgentContext,
) -> Option<String> {
    let _knowledge_context_env =
        crate::actors::slot::lifecycle::prompt_context::knowledge_context_test_env_guard();
    crate::actors::slot::lifecycle::prompt_context::load_knowledge_context(
        task,
        epic_context,
        app_state,
    )
    .await
}

/// Render a CI artifact ZIP wholly in memory using the production
/// implementation. Exposed under `test-support` so integration tests
/// can exercise the bounded renderer without duplicating the logic.
pub fn render_ci_artifact_zip_for_test(bytes: &[u8]) -> Result<String, String> {
    crate::extension::handlers::ci_artifact::render_ci_artifact_zip(bytes)
}

/// List artifacts for a resolved run using the production implementation.
/// Takes an explicit `run_id` to avoid exposing internal resolution types.
pub async fn list_artifacts_for_test(
    client: &djinn_provider::github_api::GitHubApiClient,
    owner: &str,
    repo: &str,
    run_id: u64,
) -> Result<crate::extension::handlers::ci_artifact::ArtifactListReport, String> {
    let request = crate::extension::handlers::ci::WorkflowRunResolutionRequest {
        explicit_run_id: Some(run_id),
        ..Default::default()
    };
    crate::extension::handlers::ci_artifact::list_artifacts(client, owner, repo, request).await
}

/// Fetch and render one artifact using the production implementation.
/// Takes an explicit `run_id` to avoid exposing internal resolution types.
pub async fn fetch_artifact_for_test(
    client: &djinn_provider::github_api::GitHubApiClient,
    owner: &str,
    repo: &str,
    run_id: u64,
    name: &str,
) -> Result<String, String> {
    let request = crate::extension::handlers::ci::WorkflowRunResolutionRequest {
        explicit_run_id: Some(run_id),
        ..Default::default()
    };
    crate::extension::handlers::ci_artifact::fetch_artifact(client, owner, repo, request, name)
        .await
}

/// Cheap `SupervisorServices` stub for tests that exercise `call_tool`
/// against the non-host-bound tool subset (lsp, memory, code_graph, …).
/// Panics if the test ends up invoking any trait method; the three
/// host-only tools (`github_search`, `github_fetch_file`, `ci_job_log`)
/// would route through it, but no test in this crate exercises those
/// today.
pub fn test_services() -> djinn_supervisor::services::rpc::UnimplementedRpcServices {
    djinn_supervisor::services::rpc::UnimplementedRpcServices::new()
}

pub async fn create_test_project(db: &Database) -> Project {
    let repo = ProjectRepository::new(db.clone(), test_events());
    let id = uuid::Uuid::now_v7();
    // Keep the persistent tempdir creation for tests that still need a
    // writable workspace, but the project row no longer stores a path.
    let _ = test_persistent_dir("djinn-test-project-");
    let compact_id = id.simple().to_string();
    let name = format!("test-project-{compact_id}");
    let repo_slug = format!("test-project-{}", &compact_id[..23]);
    let project = repo
        .create(&name, "test", &repo_slug)
        .await
        .expect("failed to create test project");
    // Satisfy the coordinator's readiness gate so existing tests can dispatch
    // without threading a full devcontainer pipeline. Keep both readiness
    // representations populated: legacy project image columns for older
    // callers, catalog-image selection for dispatch, and graph freshness rows
    // for both repo-level and per-workspace checks.
    let image = djinn_db::ProjectImage {
        tag: Some(format!(
            "test-registry/djinn-project-{}:testhash",
            project.id
        )),
        hash: Some("testhash".into()),
        status: djinn_db::ProjectImageStatus::READY.into(),
        last_error: None,
    };
    let _ = repo.set_project_image(&project.id, &image).await;
    // Also satisfy the catalog-image readiness path used by dispatch.  The
    // legacy `projects.image_status` columns are still populated above for
    // older call sites, but `get_dispatch_readiness` resolves the selected
    // image from the catalog first. Use a compact id/name so this helper also
    // stays inside older CI test-template varchar limits.
    let image_repo = djinn_db::ImageRepository::new(db.clone());
    let image_id = format!(
        "ci-ready-{}",
        &uuid::Uuid::now_v7().simple().to_string()[..16]
    );
    let image_name = format!("ci-ready-{}", &image_id[..8]);
    let _ = image_repo
        .create(
            &image_id,
            &image_name,
            Some("ready test image"),
            r#"{"schema_version":1}"#,
        )
        .await;
    let _ = image_repo
        .mark_ready(
            &image_id,
            image
                .tag
                .as_deref()
                .unwrap_or("test-registry/djinn-test:testhash"),
            Some("sha256:testhash"),
        )
        .await;
    let _ = image_repo
        .set_project_image(&project.id, Some(&image_id))
        .await;
    let cache_repo = djinn_db::RepoGraphCacheRepository::new(db.clone());
    let _ = cache_repo
        .upsert(djinn_db::RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: "test-commit",
            graph_blob: b"test-graph",
        })
        .await;
    let _ = djinn_db::ProjectWorkspaceGraphRepository::new(db.clone())
        .upsert(djinn_db::ProjectWorkspaceGraphUpsert {
            project_id: &project.id,
            workspace_slug: "root",
            commit_sha: "test-commit",
            status: "ready",
        })
        .await;
    project
}

pub async fn create_test_epic(db: &Database, project_id: &str) -> Epic {
    let repo = EpicRepository::new(db.clone(), test_events());
    repo.create_for_project(
        project_id,
        EpicCreateInput {
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
    .expect("failed to create test epic")
}

pub async fn create_test_task(db: &Database, project_id: &str, epic_id: &str) -> Task {
    let repo = TaskRepository::new(db.clone(), test_events());
    let creator = create_test_creator(db).await;
    let task = repo
        .create_in_project_with_provenance(
            project_id,
            Some(epic_id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
            "test-task",
            "test task description",
            "test task design",
            "task",
            2,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("failed to create test task");
    repo.update(
        &task.id,
        &task.title,
        &task.description,
        &task.design,
        task.priority,
        &task.owner,
        &task.labels,
        r#"[{"description":"default test criterion","met":false}]"#,
    )
    .await
    .expect("failed to set test task acceptance criteria")
}

pub struct FakeProvider {
    scripted_turns: Arc<StdMutex<VecDeque<Vec<anyhow::Result<StreamEvent>>>>>,
}

impl FakeProvider {
    pub fn text(text: impl Into<String>) -> Self {
        Self::script(vec![vec![
            StreamEvent::Delta(ContentBlock::Text { text: text.into() }),
            StreamEvent::Done,
        ]])
    }

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

    pub fn script(turns: Vec<Vec<StreamEvent>>) -> Self {
        let scripted_turns = turns
            .into_iter()
            .map(|turn| turn.into_iter().map(Ok).collect())
            .collect();
        Self {
            scripted_turns: Arc::new(StdMutex::new(scripted_turns)),
        }
    }

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
            let turn = scripted_turns.lock().unwrap().pop_front().unwrap_or_else(|| {
                panic!(
                    "FakeProvider script exhausted: stream() called with no scripted turns remaining"
                )
            });
            Ok(Box::pin(stream::iter(turn))
                as Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use futures::StreamExt;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn fake_provider_convenience_constructors_work_and_track_remaining() {
        let text_provider = FakeProvider::text("hello");
        assert_eq!(text_provider.remaining(), 1);

        let text_stream = text_provider
            .stream(&Conversation::new(), &[], None)
            .await
            .expect("text provider stream should succeed");
        let text_events = text_stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
            .expect("text events should be ok");

        assert_eq!(text_provider.remaining(), 0);
        assert!(matches!(
            &text_events[..],
            [
                StreamEvent::Delta(ContentBlock::Text { text }),
                StreamEvent::Done,
            ] if text == "hello"
        ));

        let tool_provider =
            FakeProvider::tool_call("tool-1", "submit_work", json!({"summary": "done"}));
        assert_eq!(tool_provider.remaining(), 1);

        let tool_stream = tool_provider
            .stream(&Conversation::new(), &[], Some(ToolChoice::Required))
            .await
            .expect("tool provider stream should succeed");
        let tool_events = tool_stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
            .expect("tool events should be ok");

        assert_eq!(tool_provider.remaining(), 0);
        assert!(matches!(
            &tool_events[..],
            [
                StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }),
                StreamEvent::Done,
            ] if id == "tool-1"
                && name == "submit_work"
                && input == &json!({"summary": "done"})
        ));
    }

    #[tokio::test]
    async fn fake_provider_streams_scripted_turn_and_tracks_remaining() {
        let provider = FakeProvider::script(vec![
            vec![
                StreamEvent::Delta(ContentBlock::Text {
                    text: "hello".to_string(),
                }),
                StreamEvent::Usage(djinn_provider::provider::TokenUsage {
                    input: 3,
                    output: 5,
                    ..Default::default()
                }),
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::Delta(ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "submit_work".to_string(),
                    input: json!({"summary": "done"}),
                }),
                StreamEvent::Done,
            ],
        ]);

        assert_eq!(provider.remaining(), 2);

        let first_stream = provider
            .stream(&Conversation::new(), &[], None)
            .await
            .expect("first scripted stream should succeed");
        let events = first_stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
            .expect("scripted events should be ok");

        assert_eq!(provider.remaining(), 1);
        assert_eq!(events.len(), 3);
        match &events[0] {
            StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "hello"),
            _ => panic!("expected first event to be text delta"),
        }
        match &events[1] {
            StreamEvent::Usage(usage) => {
                assert_eq!(usage.input, 3);
                assert_eq!(usage.output, 5);
            }
            _ => panic!("expected second event to be usage"),
        }
        assert!(matches!(events[2], StreamEvent::Done));

        let second_stream = provider
            .stream(&Conversation::new(), &[], Some(ToolChoice::Required))
            .await
            .expect("second scripted stream should succeed");
        let second_events = second_stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()
            .expect("second scripted events should be ok");

        assert_eq!(provider.remaining(), 0);
        match &second_events[0] {
            StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
                assert_eq!(id, "tool-1");
                assert_eq!(name, "submit_work");
                assert_eq!(input, &json!({"summary": "done"}));
            }
            _ => panic!("expected tool use delta"),
        }
        assert!(matches!(second_events[1], StreamEvent::Done));
    }

    #[test]
    fn fake_provider_panics_clearly_when_script_is_exhausted() {
        let provider = FakeProvider::text("done");

        futures::executor::block_on(async {
            provider
                .stream(&Conversation::new(), &[], None)
                .await
                .expect("first stream should succeed")
                .collect::<Vec<_>>()
                .await;
        });

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _stream =
                futures::executor::block_on(provider.stream(&Conversation::new(), &[], None))
                    .expect("second stream should panic before returning");
        }))
        .expect_err("expected script exhaustion panic");

        let message = if let Some(message) = panic.downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = panic.downcast_ref::<String>() {
            message.clone()
        } else {
            panic!("unexpected panic payload type");
        };

        assert_eq!(
            message,
            "FakeProvider script exhausted: stream() called with no scripted turns remaining"
        );
    }

    #[tokio::test]
    async fn failing_provider_returns_error_from_stream() {
        let provider = FailingProvider::new("boom");

        let result = provider.stream(&Conversation::new(), &[], None).await;
        let error = match result {
            Ok(_) => panic!("failing provider should return error"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("boom"));
    }
}
