// Test-only: Instant::now is used for timing assertions and deadlined poll
// loops in these integration tests.
#![allow(clippy::disallowed_methods)]
//! Concurrency / non-blocking regression for `memory_run_enrichment`.
//!
//! Asserts the contract documented in
//! `bridge::memory_enrichment_bridge::MemoryEnrichmentOps`: the
//! enrichment pass must yield cooperatively to the runtime so that
//! `memory_graph` (and the rest of the MCP surface) keep serving while
//! the pass runs.
//!
//! Test design: a stub `MemoryEnrichmentOps` whose `run_enrichment`
//! yields repeatedly for a bounded amount of simulated work. We drive
//! `memory_graph` and `memory_run_enrichment` concurrently with a
//! timeout; the graph call must return well before the simulated
//! enrichment work completes. If `memory_graph` ever blocks on
//! enrichment, the graph call observes the timeout.
//!
//! This is the regression the task's acceptance criteria ask for
//! ("A test runs `memory_graph` and `memory_run_enrichment`
//! concurrently with a timeout and proves the graph response is not
//! blocked by enrichment work"). The test does not exercise the
//! real `djinn-agent` enrichment algorithm — that's b29n's job —
//! it proves the wiring contract.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_db::{Database, ProjectRepository, UserRepository};
use rmcp::handler::server::wrapper::Parameters;

use crate::bridge::{EnrichmentEdge, EnrichmentReport, EnrichmentStatus, MemoryEnrichmentOps};
use crate::state::stubs::test_mcp_state;
use crate::tools::memory_tools::{
    GraphParams, MemoryGraphResponse, MemoryRunEnrichmentResponse, RunEnrichmentParams,
};
use crate::{server::DjinnMcpServer, tools::memory_tools::MemoryNoteView};

/// Stub bridge that simulates a slow enrichment pass by yielding for
/// `simulated_work` total across the call. Yields in small chunks so
/// the runtime can interleave other tasks (the property we want to
/// prove). Records the elapsed wall time so the test can assert the
/// stub actually ran for the full simulated duration.
struct SlowEnrichmentBridge {
    simulated_work: Duration,
    elapsed: tokio::sync::Mutex<Option<Duration>>,
}

impl SlowEnrichmentBridge {
    fn new(simulated_work: Duration) -> Self {
        Self {
            simulated_work,
            elapsed: tokio::sync::Mutex::new(None),
        }
    }

    async fn elapsed(&self) -> Option<Duration> {
        *self.elapsed.lock().await
    }
}

#[async_trait]
impl MemoryEnrichmentOps for SlowEnrichmentBridge {
    async fn run_enrichment(&self, project_id: &str) -> Result<EnrichmentReport, String> {
        let start = Instant::now();
        // Yield in 5ms chunks so the runtime can interleave other work.
        // Total wall time should approach `simulated_work`.
        let chunk = Duration::from_millis(5);
        let mut remaining = self.simulated_work;
        while remaining > Duration::ZERO {
            tokio::time::sleep(chunk.min(remaining)).await;
            remaining = remaining.saturating_sub(chunk);
        }
        *self.elapsed.lock().await = Some(start.elapsed());
        Ok(EnrichmentReport {
            project_id: project_id.to_string(),
            entities: vec![],
            claims: vec![],
            edges: vec![EnrichmentEdge {
                kind: "builds_on".into(),
                confidence: 0.8,
                ..EnrichmentEdge::default()
            }],
            notes_processed: 5,
            batches_sent: 1,
            entity_merges: 0,
            edges_dropped_wikilink_dup: 0,
            warnings: vec![],
        })
    }
}

struct FailingEnrichmentBridge;

#[async_trait]
impl MemoryEnrichmentOps for FailingEnrichmentBridge {
    async fn run_enrichment(&self, _project_id: &str) -> Result<EnrichmentReport, String> {
        Err("simulated enrichment failure".to_string())
    }
}

fn workspace_tempdir() -> tempfile::TempDir {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
    tempfile::tempdir_in(base).expect("create server crate tempdir")
}

async fn make_server_with_enrichment(
    ops: Arc<dyn MemoryEnrichmentOps>,
) -> (DjinnMcpServer, Database, String) {
    let _tmp = workspace_tempdir();
    let db = Database::open_in_memory().unwrap();
    let mut state = test_mcp_state(db.clone());
    state.set_enrichment_ops(ops);
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("enrichment-project", "test", "enrichment-project")
        .await
        .unwrap();
    (DjinnMcpServer::new(state), db, project.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_graph_concurrent_with_enrichment_does_not_block() {
    // Simulated enrichment work is well above the graph-call budget so
    // any blocking would trip the timeout.
    const SIMULATED_WORK: Duration = Duration::from_millis(400);
    const GRAPH_BUDGET: Duration = Duration::from_millis(200);

    let bridge = Arc::new(SlowEnrichmentBridge::new(SIMULATED_WORK));
    let (server, _db, project_id) = make_server_with_enrichment(bridge.clone()).await;

    // Fire enrichment in the background — the trigger returns
    // immediately with status="queued" and the actual pass runs on the
    // tokio task. This matches the production contract: the tool
    // itself never blocks the caller on the LLM provider.
    let server_bg = server.clone();
    let project_id_bg = project_id.clone();
    let enrich_task = tokio::spawn(async move {
        // The MCP wrapper returns `Json<T>`; strip it before crossing the
        // JoinHandle boundary so the test code doesn't have to thread
        // `.0` everywhere.
        server_bg
            .memory_run_enrichment(Parameters(RunEnrichmentParams {
                project: project_id_bg.clone(),
                background: Some(true),
            }))
            .await
            .0
    });

    // Give the enrichment task a moment to actually start so we know
    // the simulated work is running concurrently with the graph call.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Drive memory_graph. The test asserts it returns inside
    // GRAPH_BUDGET regardless of the still-running enrichment pass.
    let graph_start = Instant::now();
    let graph_result = tokio::time::timeout(
        GRAPH_BUDGET,
        server.memory_graph(Parameters(GraphParams {
            project: project_id.clone(),
            statuses: None,
            lifecycle_limit: None,
        })),
    )
    .await;
    let graph_elapsed = graph_start.elapsed();

    let _response: MemoryGraphResponse = graph_result
        .expect("memory_graph should not be blocked by in-flight enrichment pass")
        .0;
    assert!(
        graph_elapsed < GRAPH_BUDGET,
        "memory_graph took {graph_elapsed:?} — exceeded the {GRAPH_BUDGET:?} budget while enrichment was in flight"
    );

    // Drain the background enrichment task and assert the stub ran
    // for the full simulated duration (i.e. it wasn't cancelled by
    // the graph call returning first).
    let enrich_response = tokio::time::timeout(SIMULATED_WORK * 2, enrich_task)
        .await
        .expect("background enrichment did not complete in time")
        .expect("background enrichment task panicked");
    let MemoryRunEnrichmentResponse { status, report, .. } = enrich_response;
    assert_eq!(status, "queued", "background=true should return queued");
    assert!(
        report.is_none(),
        "queued responses don't embed the report — it lands in the pass's own INFO log"
    );

    // The queued response only confirms scheduling; the spawned enrichment
    // pass may still be finishing. Wait for the stub to record completion so
    // this assertion is deterministic instead of racing the fire-and-forget
    // task.
    let elapsed = tokio::time::timeout(SIMULATED_WORK * 2, async {
        loop {
            if let Some(elapsed) = bridge.elapsed().await {
                break elapsed;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background enrichment pass did not finish in time");
    assert!(
        elapsed >= SIMULATED_WORK.saturating_sub(Duration::from_millis(50)),
        "enrichment ran for {elapsed:?} but should have run for ~{SIMULATED_WORK:?} — \
         the graph call may have pre-empted it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_run_enrichment_returns_structured_report_when_not_queued() {
    // Foreground path: the tool returns the structured report inline.
    let bridge = Arc::new(SlowEnrichmentBridge::new(Duration::from_millis(20)));
    let (server, _db, project_id) = make_server_with_enrichment(bridge).await;

    let response = server
        .memory_run_enrichment(Parameters(RunEnrichmentParams {
            project: project_id,
            background: Some(false),
        }))
        .await
        .0;
    assert_eq!(response.status, EnrichmentStatus::Completed.as_str());
    let report = response.report.expect("foreground path embeds the report");
    assert_eq!(report.entities.len(), 0);
    assert_eq!(report.claims.len(), 0);
    assert_eq!(report.edges.len(), 1);
    assert_eq!(report.edges[0].kind, "builds_on");
    assert!(response.error.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_run_enrichment_unknown_project_returns_clean_error() {
    let bridge = Arc::new(SlowEnrichmentBridge::new(Duration::from_millis(10)));
    let (server, _db, _project_id) = make_server_with_enrichment(bridge).await;

    let response = server
        .memory_run_enrichment(Parameters(RunEnrichmentParams {
            project: "/nonexistent/project".to_string(),
            background: Some(false),
        }))
        .await
        .0;
    assert!(
        response
            .error
            .as_deref()
            .unwrap_or("")
            .contains("project not found"),
        "expected project-not-found error, got {:?}",
        response.error
    );
    assert!(response.report.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_run_enrichment_bridge_failure_returns_clean_error() {
    // If the bridge itself fails before it can return the agent's best-effort
    // report, the admin trigger must still return a structured response rather
    // than panic or hang. `run_enrichment_inner` also emits the required INFO
    // finish line with `status="failed"` for this path.
    let bridge = Arc::new(FailingEnrichmentBridge);
    let (server, _db, project_id) = make_server_with_enrichment(bridge).await;

    let response = server
        .memory_run_enrichment(Parameters(RunEnrichmentParams {
            project: project_id.clone(),
            background: Some(false),
        }))
        .await
        .0;

    assert_eq!(response.status, EnrichmentStatus::Completed.as_str());
    assert_eq!(response.project_id.as_deref(), Some(project_id.as_str()));
    assert!(
        response
            .error
            .as_deref()
            .unwrap_or("")
            .contains("simulated enrichment failure"),
        "expected bridge failure to be surfaced, got {:?}",
        response.error
    );
    assert!(response.report.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_run_enrichment_without_bridge_surfaces_clean_error() {
    // No enrichment bridge wired → the tool surfaces a clear
    // "not configured" error rather than panicking or running
    // without the bridge.
    let _tmp = workspace_tempdir();
    let db = Database::open_in_memory().unwrap();
    let state = test_mcp_state(db.clone());
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("no-bridge-project", "test", "no-bridge-project")
        .await
        .unwrap();
    let server = DjinnMcpServer::new(state);

    let response = server
        .memory_run_enrichment(Parameters(RunEnrichmentParams {
            project: project.id,
            background: Some(false),
        }))
        .await
        .0;
    assert!(
        response
            .error
            .as_deref()
            .unwrap_or("")
            .contains("not configured"),
        "expected not-configured error, got {:?}",
        response.error
    );
    assert!(response.report.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_run_enrichment_rejects_authenticated_non_admin() {
    // The trigger is an admin/operator maintenance tool because it can spend
    // LLM budget and write derived memory rows. Trusted internal/no-user calls
    // are still allowed by `require_admin`, but an authenticated non-admin
    // session must fail before project resolution or bridge invocation.
    let bridge = Arc::new(SlowEnrichmentBridge::new(Duration::from_millis(10)));
    let (server, _db, project_id) = make_server_with_enrichment(bridge.clone()).await;
    let user = UserRepository::new(server.state.db().clone())
        .upsert_from_github(999_777, "non-admin-enrichment", None, None)
        .await
        .unwrap();
    assert!(!user.is_admin);

    let response = djinn_core::auth_context::SESSION_USER_ID
        .scope(
            Some(user.id),
            server.memory_run_enrichment(Parameters(RunEnrichmentParams {
                project: project_id,
                background: Some(false),
            })),
        )
        .await
        .0;

    assert!(
        response.error.as_deref().unwrap_or("").contains("admin"),
        "expected admin-gate error, got {:?}",
        response.error
    );
    assert!(response.report.is_none());
    assert!(
        bridge.elapsed().await.is_none(),
        "non-admin call must be rejected before invoking enrichment"
    );
}

// Exercise the MemoryNoteView `From` impl just to ensure the module
// compiles when this test file is the only consumer — there's no
// test for that conversion elsewhere in this crate.
#[allow(dead_code)]
fn _ensure_note_view_compiles(note: &djinn_memory::Note) -> MemoryNoteView {
    MemoryNoteView::from(note)
}
