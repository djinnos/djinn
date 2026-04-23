//! Test utilities for djinn-agent tests.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use futures::stream;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use djinn_core::events::EventBus;
use djinn_core::models::Project;
use djinn_core::models::{Epic, Task};
use djinn_db::{Database, EpicCreateInput, EpicRepository, ProjectRepository, TaskRepository};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};

use crate::context::AgentContext;
use crate::file_time::FileTime;
use crate::lsp::LspManager;
use crate::roles::RoleRegistry;

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

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

pub fn test_events() -> EventBus {
    EventBus::noop()
}

pub fn agent_context_from_db(db: Database, _cancel: CancellationToken) -> AgentContext {
    AgentContext {
        db,
        event_bus: EventBus::noop(),
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        verifying_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
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
        mirror: None,
        rpc_registry: None,
    }
}

pub async fn create_test_project(db: &Database) -> Project {
    let repo = ProjectRepository::new(db.clone(), test_events());
    let id = uuid::Uuid::now_v7();
    let path = test_persistent_dir("djinn-test-project-")
        .to_string_lossy()
        .to_string();
    let name = format!("test-project-{id}");
    let project = repo
        .create(&name, &path)
        .await
        .expect("failed to create test project");
    // Satisfy the coordinator's readiness gate so existing tests can dispatch
    // without threading a full devcontainer pipeline: mark the image as ready
    // and stamp `graph_warmed_at` via a cache row with a synthetic commit SHA.
    let image = djinn_db::ProjectImage {
        tag: Some(format!("test-registry/djinn-project-{}:testhash", &project.id)),
        hash: Some("testhash".into()),
        status: djinn_db::ProjectImageStatus::READY.into(),
        last_error: None,
    };
    let _ = repo.set_project_image(&project.id, &image).await;
    let cache_repo = djinn_db::RepoGraphCacheRepository::new(db.clone());
    let _ = cache_repo
        .upsert(djinn_db::RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: "test-commit",
            graph_blob: b"test-graph",
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
        },
    )
    .await
    .expect("failed to create test epic")
}

pub async fn create_test_task(db: &Database, project_id: &str, epic_id: &str) -> Task {
    let repo = TaskRepository::new(db.clone(), test_events());
    let task = repo
        .create_in_project(
            project_id,
            Some(epic_id),
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
