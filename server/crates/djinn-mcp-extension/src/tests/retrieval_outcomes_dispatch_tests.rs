//! Importer: `tests/mod.rs`; exercises the production extension dispatch entry point.
//! Public APIs under test: `dispatch_tool_call`, `DispatchResult`, and `ExtensionContext`.
//! Data shapes: report JSON request/response plus project and retrieval-trace fixtures.
//! Task: add cross-surface dispatch and retention-boundary tests for
//! `memory_retrieval_outcomes_report`.

use std::path::{Path, PathBuf};

use djinn_control_plane::{McpState, state::stubs::test_mcp_state};
use djinn_core::{events::EventBus, tool_call::ToolCallOutcome};
use djinn_db::{
    Database, ProjectRepository,
    repositories::retrieval_trace::{
        CreateRetrievalTraceParams, RetrievalTraceEntryPoint, RetrievalTraceRepository,
    },
};

use crate::{DispatchResult, ExtensionContext, dispatch::dispatch_tool_call};

struct TestContext {
    db: Database,
}

#[async_trait::async_trait]
impl ExtensionContext for TestContext {
    fn db(&self) -> Database {
        self.db.clone()
    }

    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }

    fn mcp_state(&self) -> McpState {
        test_mcp_state(self.db.clone())
    }

    fn lsp(&self) -> djinn_lsp::LspManager {
        djinn_lsp::LspManager::new()
    }

    fn working_root_for(&self, fallback: &Path) -> PathBuf {
        fallback.to_path_buf()
    }

    fn default_project_id(&self) -> Option<&str> {
        None
    }
}

fn interval_from_created_at(created_at: &str) -> (String, String) {
    let date = created_at.get(..10).expect("project date");
    let clock = created_at
        .get(11..19)
        .expect("project creation time with seconds");
    (
        format!("{date}T00:00:00+00:00"),
        format!("{date}T{clock}+00:00"),
    )
}

#[tokio::test]
async fn extension_dispatch_routes_report_and_forces_resolved_worktree_project() {
    let db = Database::ephemeral().await.expect("ephemeral database");
    let projects = ProjectRepository::new(db.clone(), EventBus::noop());
    let current = projects
        .create("current", "current-owner", "current-repo")
        .await
        .expect("create current project");
    RetrievalTraceRepository::new(db.clone())
        .insert(CreateRetrievalTraceParams {
            project_id: &current.id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &serde_json::json!([]),
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &serde_json::json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .expect("seed current-project diagnostic trace");

    // The report interval is half-open. Create the caller-selected project in
    // a later second and use its timestamp as a deterministic exclusive end.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let foreign = projects
        .create("foreign", "foreign-owner", "foreign-repo")
        .await
        .expect("create caller-selected project");
    let (start, end) = interval_from_created_at(&foreign.created_at);
    let call = serde_json::json!({
        "name": "memory_retrieval_outcomes_report",
        "arguments": {
            "project": foreign.slug(),
            "project_id": foreign.id,
            "start": start,
            "end": end,
            "timezone": "Etc/UTC"
        }
    });
    let worktree = Path::new("/workspace/current-owner/current-repo");
    let services = djinn_supervisor::UnimplementedRpcServices::new();

    let outcome = dispatch_tool_call(
        &TestContext { db: db.clone() },
        &services,
        &call,
        worktree,
        None,
        Some("must-not-be-used-as-project-fallback"),
        Some("architect"),
    )
    .await;

    let DispatchResult::Handled(ToolCallOutcome::Success { value, .. }) = outcome else {
        panic!("report operation was not successfully handled by extension dispatch");
    };
    assert_eq!(value["error"], serde_json::Value::Null);
    assert_eq!(value["report"]["start"], start);
    assert_eq!(value["report"]["end"], end);
    assert_eq!(value["report"]["timezone"], "Etc/UTC");
    assert_eq!(
        value["report"]["diagnostics"]["unattributed_trace_count"], 1,
        "the worktree-scoped current project must win over caller project selectors"
    );

    for (rejected_start, rejected_end) in [
        (end.as_str(), start.as_str()),
        ("2000-01-01T00:00:00+00:00", "2000-01-02T00:00:00+00:00"),
    ] {
        let rejected_call = serde_json::json!({
            "name": "memory_retrieval_outcomes_report",
            "arguments": {
                "project": foreign.slug(),
                "start": rejected_start,
                "end": rejected_end,
                "timezone": "Etc/UTC"
            }
        });
        let rejected = dispatch_tool_call(
            &TestContext { db: db.clone() },
            &services,
            &rejected_call,
            worktree,
            None,
            None,
            Some("architect"),
        )
        .await;
        let DispatchResult::Handled(ToolCallOutcome::Success { value, .. }) = rejected else {
            panic!("rejected interval was not handled by extension dispatch");
        };
        assert_eq!(value["report"], serde_json::Value::Null);
        assert_eq!(value["error"], "invalid data: unsupported report interval");
        assert!(value.get("start").is_none() && value.get("end").is_none());
    }
}
