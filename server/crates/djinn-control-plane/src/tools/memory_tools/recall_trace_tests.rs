// Integration tests for the memory_recall_trace MCP tool.
//
// Importers/callers: declared as `#[cfg(test)] mod recall_trace_tests;` in
// `server/crates/djinn-control-plane/src/tools/memory_tools/mod.rs`, compiled
// only when running `cargo test -p djinn-control-plane`.
//
// Affected public functions/types: tests exercise
// `crate::tools::memory_tools::ops::memory_recall_trace` (the public helper
// backing the `memory_recall_trace` MCP tool) and its request/response DTOs
// from `crate::tools::memory_tools::types` (`RecallTraceParams`,
// `MemoryRecallTraceResponse`, `MemoryRecallTraceSummary`,
// `MemoryRecallTraceDetail`, `MemoryRecallTraceCandidate`). The tests also use
// `NoteRepository`, `ProjectRepository`, `RetrievalTraceRepository`, and
// `CreateRetrievalTraceParams` / `TraceCandidate` from `djinn-db` only to seed
// realistic fixture data.
//
// Data schema touched: writes `retrieval_traces` rows and `notes` rows via the
// existing repository APIs; assertions read the MCP response shapes only.
//
// Verbatim task instruction: "Add integration-style control-plane tests using
// the established `setup_server()` harness and seeded retrieval traces/notes to
// assert each required response and isolation/fallback case."

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, NoteRepository, ProjectRepository,
        TaskRepository,
        repositories::retrieval_trace::{
            CandidateOutcome, CreateRetrievalTraceParams, RetrievalTraceEntryPoint,
            RetrievalTraceRepository, SkippedReason, TraceCandidate,
        },
    };
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubRuntimeOps,
        StubSlotPoolOps,
    };
    use crate::tools::memory_tools::{RecallTraceParams, ops};

    struct SetupResult {
        server: DjinnMcpServer,
        _tmp: tempfile::TempDir,
        project_id: String,
        project_slug: String,
    }

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    fn test_mcp_state(db: Database, tx: &broadcast::Sender<DjinnEventEnvelope>) -> McpState {
        McpState::new(
            db,
            event_bus_for(tx),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(StubRuntimeOps),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        )
    }

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    async fn setup_server() -> SetupResult {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let event_bus = event_bus_for(&tx);
        let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
        let project = project_repo
            .create("test-owner", "test-project", "test-project")
            .await
            .unwrap();
        let server = DjinnMcpServer::new(test_mcp_state(db, &tx));
        let project_id = project.id.clone();
        let project_slug = project.slug();
        SetupResult {
            server,
            _tmp: tmp,
            project_id,
            project_slug,
        }
    }

    fn injected_candidate(note_id: &str, rank: i32, confidence: f64) -> TraceCandidate {
        TraceCandidate {
            note_id: note_id.to_string(),
            permalink: Some(format!("notes/{note_id}")),
            title: Some(format!("Note {note_id}")),
            outcome: CandidateOutcome::Injected,
            rank: Some(rank),
            confidence: Some(confidence),
            skipped_reason: None,
            source: Some("scope_overlap".to_string()),
            scope: Some(serde_json::json!({"scopes": ["backend"]})),
        }
    }

    fn skipped_candidate(
        note_id: &str,
        rank: i32,
        confidence: f64,
        reason: SkippedReason,
    ) -> TraceCandidate {
        TraceCandidate {
            note_id: note_id.to_string(),
            permalink: Some(format!("notes/{note_id}")),
            title: Some(format!("Note {note_id}")),
            outcome: CandidateOutcome::Skipped,
            rank: Some(rank),
            confidence: Some(confidence),
            skipped_reason: Some(reason),
            source: Some("scope_overlap".to_string()),
            scope: Some(serde_json::json!({"scopes": ["backend"]})),
        }
    }

    async fn insert_trace(
        server: &DjinnMcpServer,
        project_id: &str,
        entry_point: RetrievalTraceEntryPoint,
        session_id: Option<&str>,
        task_id: Option<&str>,
        task_run_id: Option<&str>,
        candidates: Vec<TraceCandidate>,
    ) -> String {
        let repo = RetrievalTraceRepository::new(server.state.db().clone());
        let candidates_json = serde_json::to_value(&candidates).unwrap();
        let row = repo
            .insert(CreateRetrievalTraceParams {
                project_id,
                session_id,
                task_run_id,
                task_id,
                entry_point,
                trigger: Some(&serde_json::json!({"query": "test query"})),
                candidates: &candidates_json,
                candidate_cap: 50,
                candidate_cap_exceeded: candidates.len() > 50,
                sampling_metadata: None,
                durations_ms: &serde_json::json!({"retrieval_ms": 12}),
                estimated_injected_tokens: 256,
            })
            .await
            .unwrap();
        row.id
    }

    async fn create_note(
        server: &DjinnMcpServer,
        project_id: &str,
        title: &str,
        content: &str,
    ) -> djinn_memory::Note {
        create_note_with_status(server, project_id, title, content, None).await
    }

    async fn create_note_with_status(
        server: &DjinnMcpServer,
        project_id: &str,
        title: &str,
        content: &str,
        status: Option<&str>,
    ) -> djinn_memory::Note {
        let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
        repo.create_with_status_and_retrieval_anchor(
            project_id,
            title,
            content,
            "reference",
            status,
            "[]",
            None,
        )
        .await
        .unwrap()
    }

    async fn create_scoped_note(
        server: &DjinnMcpServer,
        project_id: &str,
        title: &str,
        content: &str,
        scope_paths: &str,
    ) -> djinn_memory::Note {
        let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
        repo.create_with_scope(
            project_id,
            title,
            content,
            "pattern",
            None,
            "[]",
            scope_paths,
        )
        .await
        .unwrap()
    }

    // Build a task whose description contains a code path so that the real
    // dispatch path derives a scope that overlaps with the seeded notes.
    async fn create_task_with_scope(
        server: &DjinnMcpServer,
        project_id: &str,
        description: &str,
    ) -> djinn_core::models::Task {
        let db = server.state.db().clone();
        let events = server.state.event_bus();
        let epic_repo = EpicRepository::new(db.clone(), events.clone());
        let epic = epic_repo
            .create_for_project(
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
            .unwrap();
        let task_repo = TaskRepository::new(db, events);
        task_repo
            .create_in_project(
                project_id,
                Some(&epic.id),
                "test-task",
                description,
                "test design",
                "task",
                2,
                "test-owner",
                None,
                None,
            )
            .await
            .unwrap()
    }

    // Integration proof for proposal ykkj: seed a bounded scope-overlap
    // universe with one below-threshold note and one above-threshold candidate
    // outside the production limit, exercise the real dispatch
    // `load_knowledge_context` path, then drill into the persisted trace
    // through `memory_recall_trace` detail and list filters.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_detail_classifies_scope_overlap_candidates_with_min_confidence_and_not_top_k()
     {
        let setup = setup_server().await;
        let project_id = &setup.project_id;
        let scope = r#"["server/src/server/state/mod.rs"]"#;

        // Create a task whose description includes a code path that overlaps
        // with the note scope. The real dispatch path derives `server` from
        // `server/src/server/state/mod.rs`.
        let task = create_task_with_scope(
            &setup.server,
            project_id,
            "Touched server/src/server/state/mod.rs during this change.",
        )
        .await;

        // Seed 10 high-confidence notes that will be injected (top-K within the
        // 10-note production limit), one above-threshold note outside the top
        // 10, and one below-threshold note.
        for i in 0..10 {
            let confidence = 0.90 - (i as f64 * 0.01);
            let note = create_scoped_note(
                &setup.server,
                project_id,
                &format!("Injected note {}", i + 1),
                "short content",
                scope,
            )
            .await;
            let repo = NoteRepository::new(
                setup.server.state.db().clone(),
                setup.server.state.event_bus(),
            );
            repo.set_confidence(&note.id, confidence).await.unwrap();
        }

        let over_limit = create_scoped_note(
            &setup.server,
            project_id,
            "Over production limit",
            &(0..1200).map(|i| format!("word{i} ")).collect::<String>(),
            scope,
        )
        .await;
        let repo = NoteRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        );
        repo.set_confidence(&over_limit.id, 0.80).await.unwrap();

        let below_threshold = create_scoped_note(
            &setup.server,
            project_id,
            "Below threshold",
            "low content body",
            scope,
        )
        .await;
        let repo = NoteRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        );
        repo.set_confidence(&below_threshold.id, 0.10)
            .await
            .unwrap();

        // Invoke the real production dispatch path. This runs the production
        // scope-overlap query, the capped trace-candidate query, deterministic
        // classification, prompt packing, and fail-open trace persistence.
        let app_state = djinn_agent::test_helpers::agent_context_from_db(
            setup.server.state.db().clone(),
            CancellationToken::new(),
        );
        let _rendered =
            djinn_agent::test_helpers::run_load_knowledge_context_for_test(&task, None, &app_state)
                .await;

        // List the persisted trace by its entry point; keep the response compact.
        let list_resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: Some("load_knowledge_context".to_string()),
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert!(list_resp.error.is_none(), "{:?}", list_resp.error);
        assert_eq!(list_resp.traces.len(), 1);
        let trace_summary = &list_resp.traces[0];
        assert_eq!(trace_summary.entry_point, "load_knowledge_context");
        assert_eq!(trace_summary.candidate_count, 12);
        assert_eq!(trace_summary.injected_count, 10);
        assert_eq!(trace_summary.skipped_count, 2);
        assert!(!trace_summary.candidate_cap_exceeded);
        // List response must stay compact (no note bodies).
        let summary_json = serde_json::to_string(&trace_summary).unwrap();
        assert!(!summary_json.contains("short content"));
        assert!(!summary_json.contains("low content body"));

        let trace_id = trace_summary.trace_id.clone();

        // Detail mode: hydrate title, permalink, content excerpt, rank/confidence,
        // outcome, skipped reasons, and metadata.
        let detail_response = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "detail".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: Some(trace_id.clone()),
            },
        )
        .await;
        assert!(
            detail_response.error.is_none(),
            "{:?}",
            detail_response.error
        );
        let detail = detail_response.trace.expect("detail expected");
        assert_eq!(detail.trace_id, trace_id);
        assert_eq!(detail.candidates.len(), 12);
        assert_eq!(detail.entry_point, "load_knowledge_context");
        assert_eq!(detail.candidate_cap, 50);
        assert!(!detail.candidate_cap_exceeded);

        let by_id: HashMap<String, &crate::tools::memory_tools::MemoryRecallTraceCandidate> =
            detail
                .candidates
                .iter()
                .map(|c| (c.note_id.clone(), c))
                .collect();
        assert_eq!(by_id.len(), 12);

        // Over-limit candidate is above the minimum confidence but outside the
        // top-10 production set.
        let over_detail = by_id
            .get(&over_limit.id)
            .expect("over-limit candidate in detail");
        assert_eq!(over_detail.title, "Over production limit");
        assert_eq!(over_detail.permalink, over_limit.permalink);
        assert_eq!(over_detail.outcome, "skipped");
        assert_eq!(over_detail.skipped_reason.as_deref(), Some("not_top_k"));
        assert!(over_detail.rank.unwrap() > 10);
        assert!((over_detail.confidence.unwrap() - 0.80).abs() < f64::EPSILON);
        let over_excerpt = over_detail.content_excerpt.as_ref().unwrap();
        assert!(over_excerpt.chars().count() <= 1001);
        assert!(over_excerpt.ends_with('…'));
        assert!(over_excerpt.contains("word0"));
        assert!(over_excerpt.contains("word100"));
        assert!(!over_excerpt.contains("word1199"));

        // Below-threshold candidate is dropped by the production minimum.
        let low_detail = by_id
            .get(&below_threshold.id)
            .expect("below-threshold candidate in detail");
        assert_eq!(low_detail.title, "Below threshold");
        assert_eq!(low_detail.permalink, below_threshold.permalink);
        assert_eq!(low_detail.outcome, "skipped");
        assert_eq!(low_detail.skipped_reason.as_deref(), Some("min_confidence"));
        assert!(low_detail.rank.unwrap() > 10);
        assert!((low_detail.confidence.unwrap() - 0.10).abs() < f64::EPSILON);
        assert_eq!(
            low_detail.content_excerpt.as_deref(),
            Some("low content body")
        );

        // Injected candidates should be exactly the 10 high-confidence notes.
        let injected_count = detail
            .candidates
            .iter()
            .filter(|c| c.outcome == "injected")
            .count();
        assert_eq!(injected_count, 10);

        // List-mode filtering by outcome and skipped reason locates the same
        // trace while returning compact summaries only.
        let list_skipped = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: Some("skipped".to_string()),
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert!(list_skipped.error.is_none(), "{:?}", list_skipped.error);
        assert_eq!(list_skipped.traces.len(), 1);
        assert_eq!(list_skipped.traces[0].trace_id, trace_id);

        let list_min_confidence = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: Some("min_confidence".to_string()),
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert!(
            list_min_confidence.error.is_none(),
            "{:?}",
            list_min_confidence.error
        );
        assert_eq!(list_min_confidence.traces.len(), 1);
        assert_eq!(list_min_confidence.traces[0].trace_id, trace_id);

        let list_not_top_k = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: Some("not_top_k".to_string()),
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert!(list_not_top_k.error.is_none(), "{:?}", list_not_top_k.error);
        assert_eq!(list_not_top_k.traces.len(), 1);
        assert_eq!(list_not_top_k.traces[0].trace_id, trace_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_list_filters_and_paginates() {
        let setup = setup_server().await;
        let c = vec![injected_candidate("n1", 1, 0.9)];
        let id_a = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::Dispatch,
            Some("sess-a"),
            Some("task-a"),
            Some("run-a"),
            c.clone(),
        )
        .await;
        let id_b = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::JitPitfalls,
            Some("sess-b"),
            Some("task-b"),
            Some("run-b"),
            c.clone(),
        )
        .await;

        // Filter by entry_point
        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: Some("dispatch".to_string()),
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_a);

        // Filter by session_id
        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: Some("sess-b".to_string()),
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_b);

        // Filter by task_id
        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: Some("task-a".to_string()),
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_a);

        // Filter by task_run_id
        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: Some("run-b".to_string()),
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_b);

        // Pagination
        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: Some(1),
                offset: Some(1),
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_a);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_list_rejects_oversized_limit_at_tool_boundary() {
        let setup = setup_server().await;
        let response = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: Some(2_147_483_647),
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(response.error.as_deref(), Some("limit must be at most 100"));
        assert!(response.traces.is_empty());
        assert!(response.trace.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_list_filters_by_outcome_and_skipped_reason() {
        let setup = setup_server().await;
        let id_injected = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::Dispatch,
            None,
            None,
            None,
            vec![injected_candidate("n1", 1, 0.9)],
        )
        .await;
        let id_skipped = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::JitPitfalls,
            None,
            None,
            None,
            vec![skipped_candidate("n2", 2, 0.1, SkippedReason::NotTopK)],
        )
        .await;

        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: Some("injected".to_string()),
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_injected);

        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: Some("not_top_k".to_string()),
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_eq!(resp.traces[0].trace_id, id_skipped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_list_cross_project_isolation() {
        let setup = setup_server().await;
        let other_project = ProjectRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        )
        .create("other-owner", "other-project", "other-project")
        .await
        .unwrap();
        let _local_id = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::Dispatch,
            None,
            None,
            None,
            vec![injected_candidate("n1", 1, 0.9)],
        )
        .await;
        let other_id = insert_trace(
            &setup.server,
            &other_project.id,
            RetrievalTraceEntryPoint::Dispatch,
            None,
            None,
            None,
            vec![injected_candidate("n1", 1, 0.9)],
        )
        .await;

        let resp = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "list".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: None,
            },
        )
        .await;
        assert_eq!(resp.traces.len(), 1);
        assert_ne!(resp.traces[0].trace_id, other_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_detail_hydrates_note_content_and_bounds_excerpt() {
        let setup = setup_server().await;
        let long_content = (0..1500).map(|i| format!("word{i} ")).collect::<String>();
        let note = create_note(
            &setup.server,
            &setup.project_id,
            "Hydrated Note",
            &long_content,
        )
        .await;
        let trace_id = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::Dispatch,
            None,
            None,
            None,
            vec![
                injected_candidate(&note.id, 1, 0.95),
                skipped_candidate("missing-note", 2, 0.10, SkippedReason::NotTopK),
            ],
        )
        .await;

        let response = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "detail".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: Some(trace_id.clone()),
            },
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        let detail = response.trace.expect("detail expected");
        assert_eq!(detail.trace_id, trace_id);
        assert_eq!(detail.candidates.len(), 2);

        let hydrated = &detail.candidates[0];
        assert_eq!(hydrated.note_id, note.id);
        assert_eq!(hydrated.title, "Hydrated Note");
        assert_eq!(hydrated.permalink, note.permalink);
        let excerpt = hydrated.content_excerpt.as_ref().unwrap();
        assert!(
            excerpt.chars().count() <= 1001,
            "excerpt should be bounded: {}",
            excerpt.chars().count()
        );
        assert!(excerpt.ends_with('…'));

        let missing = &detail.candidates[1];
        assert_eq!(missing.note_id, "missing-note");
        assert_eq!(missing.title, "Note missing-note");
        assert_eq!(missing.permalink, "notes/missing-note");
        assert!(missing.content_excerpt.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_detail_archived_note_still_hydrates() {
        let setup = setup_server().await;
        let note = create_note_with_status(
            &setup.server,
            &setup.project_id,
            "Archived Note",
            "content of archived note",
            Some("archived"),
        )
        .await;

        let trace_id = insert_trace(
            &setup.server,
            &setup.project_id,
            RetrievalTraceEntryPoint::Dispatch,
            None,
            None,
            None,
            vec![injected_candidate(&note.id, 1, 0.95)],
        )
        .await;

        let response = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "detail".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: Some(trace_id),
            },
        )
        .await;

        assert!(response.error.is_none(), "{:?}", response.error);
        let detail = response.trace.unwrap();
        assert_eq!(
            detail.candidates[0].content_excerpt.as_deref(),
            Some("content of archived note")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_recall_trace_detail_unknown_or_cross_project_trace_returns_error() {
        let setup = setup_server().await;
        let other_project = ProjectRepository::new(
            setup.server.state.db().clone(),
            setup.server.state.event_bus(),
        )
        .create("cross-owner", "cross-project", "cross-project")
        .await
        .unwrap();
        let other_trace = insert_trace(
            &setup.server,
            &other_project.id,
            RetrievalTraceEntryPoint::Dispatch,
            None,
            None,
            None,
            vec![injected_candidate("n1", 1, 0.9)],
        )
        .await;

        let unknown = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "detail".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: Some("019f540a-0000-7000-8000-000000000000".to_string()),
            },
        )
        .await;
        assert!(unknown.error.is_some());

        let cross = ops::memory_recall_trace(
            &setup.server,
            RecallTraceParams {
                mode: "detail".to_string(),
                project: Some(setup.project_slug.clone()),
                project_id: None,
                session_id: None,
                task_id: None,
                task_run_id: None,
                entry_point: None,
                outcome: None,
                skipped_reason: None,
                limit: None,
                offset: None,
                trace_id: Some(other_trace),
            },
        )
        .await;
        assert!(cross.error.is_some());
    }

    #[test]
    fn recall_trace_params_deserialize_filters() {
        let p: RecallTraceParams = serde_json::from_value(serde_json::json!({
            "mode": "list",
            "project_id": "p",
            "outcome": "skipped",
            "skipped_reason": "not_top_k",
            "limit": 10,
            "offset": 2
        }))
        .unwrap();
        assert_eq!(p.project_id.as_deref(), Some("p"));
        assert_eq!(p.offset, Some(2));
    }
}
