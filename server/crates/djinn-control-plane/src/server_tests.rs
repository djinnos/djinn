// djinn:allow-oversize — test module grows with DoR/lifecycle regression coverage; split when touched substantively.
#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::DispatchPause;
    use djinn_db::repositories::dispatch_pause::{DispatchPauseRepository, DispatchPauseTarget};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::{Json, ServerHandler, handler::server::wrapper::Parameters};
    use serde_json::json;
    use tokio::time::sleep;

    use crate::{
        server::{DjinnMcpServer, SessionEndHookSessionManager},
        state::stubs::test_mcp_state,
        tools::memory_tools::{EditParams, ReadParams, WriteParams},
    };

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }

    /// Removes a directory tree on drop. Used by tests that write into the
    /// synthesized `project_dir(owner, repo)` location (under `$DJINN_HOME`
    /// or `~/.djinn/projects`) since those paths are outside any `TempDir`
    /// and would otherwise accumulate forever.
    struct PathCleanupGuard {
        path: std::path::PathBuf,
    }

    impl PathCleanupGuard {
        fn new(path: std::path::PathBuf) -> Self {
            Self { path }
        }
    }

    impl Drop for PathCleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    async fn create_project(db: &Database, _root: &std::path::Path) -> djinn_core::models::Project {
        // Generate a unique owner/repo per test to isolate the derived
        // `project_dir({owner}/{repo})` paths that the MCP tools scan —
        // multiple tests sharing "test/test-project" would otherwise race
        // on the same `~/.djinn/projects/test/test-project/.djinn` tree.
        let id = uuid::Uuid::now_v7();
        let repo_name = format!("test-project-{id}");
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create(&repo_name, "test", &repo_name)
            .await
            .unwrap()
    }

    async fn wait_for_summaries_change(
        repo: &NoteRepository,
        note_id: &str,
        previous_overview: Option<String>,
    ) -> djinn_memory::Note {
        // 5s budget (200 x 25ms). Summary regeneration runs as a background
        // task; the prior 1s budget (40 x 25ms) timed out under full-suite
        // CPU/DB contention, returning a still-empty note and flaking the
        // assertion. Widened per the repo's poll-budget flake-fix convention.
        for _ in 0..200 {
            let note = repo.get(note_id).await.unwrap().unwrap();
            if note
                .abstract_
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
                && note
                    .overview
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty())
                && note.overview != previous_overview
            {
                return note;
            }
            sleep(Duration::from_millis(25)).await;
        }
        repo.get(note_id).await.unwrap().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_and_edit_regenerate_summaries_without_blocking_ack() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let canonical = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        let _guard = PathCleanupGuard::new(canonical);
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Summary Note".to_string(),
                content: "Sentence one. Sentence two.\n\nMore context follows here.".to_string(),
                reason: "test mutation".to_string(),
                note_type: "reference".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
                retrieval_anchor: None,
            }))
            .await;

        assert!(created.error.is_none());
        let note_id = created.id.clone().expect("memory_write returns note id");

        // The non-blocking guarantee is structural: `memory_write` returns a
        // `MemoryNoteResponse` that carries no summary fields and merely spawns
        // `schedule_summary_regeneration`. We intentionally do NOT assert the
        // DB row's summaries are still `None` right after the write returns:
        // the spawned fallback runs deterministic first-sentence extraction in
        // microseconds, so on a multi-thread runtime it can populate the row
        // before this code observes it — and crucially, a *blocking* write
        // would leave the row in the exact same populated state, so the check
        // has no power to distinguish blocking from non-blocking. It only
        // flakes. The async path is instead proven below by the summaries
        // appearing (and re-appearing after an edit) without the write/edit
        // calls themselves having awaited them.
        let generated = wait_for_summaries_change(&repo, &note_id, None).await;
        assert!(
            generated
                .abstract_
                .as_deref()
                .is_some_and(|v| v.contains("Sentence one"))
        );
        assert!(
            generated
                .overview
                .as_deref()
                .is_some_and(|v| v.contains("Sentence one"))
        );

        let previous_overview = generated.overview.clone();

        let Json(edited) = server
            .memory_edit(Parameters(EditParams {
                project: project.slug(),
                identifier: note_id.clone(),
                operation: "append".to_string(),
                content: "Fresh closing details.".to_string(),
                find_text: None,
                section: None,
                note_type: None,
                retrieval_anchor: None,
            }))
            .await;

        assert!(edited.error.is_none());
        let regenerated = wait_for_summaries_change(&repo, &note_id, previous_overview).await;
        assert!(
            regenerated
                .overview
                .as_deref()
                .is_some_and(|v| v.contains("Fresh closing details."))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_access_backfills_missing_summaries() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let canonical = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&canonical).expect("create canonical project dir");
        let _guard = PathCleanupGuard::new(canonical.clone());
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let legacy = repo
            .create(
                &project.id,
                "Legacy Note",
                "Legacy note body. It has enough content for summaries.\n\nSecond paragraph here.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let server = DjinnMcpServer::new(state);

        let Json(response) = server
            .memory_read(Parameters(ReadParams {
                project: project.slug(),
                identifier: legacy.permalink.clone(),
            }))
            .await;

        assert!(response.error.is_none());
        let updated = wait_for_summaries_change(&repo, &legacy.id, None).await;
        assert!(
            updated
                .abstract_
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
        );
        assert!(
            updated
                .overview
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
        );
        assert_ne!(updated.abstract_.as_deref(), Some(""));
        assert_ne!(updated.overview.as_deref(), Some(""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_session_flushes_reads_from_same_session_server() {
        let _tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note_a = repo
            .create(&project.id, "Note A", "alpha", "reference", "[]")
            .await
            .unwrap();
        let note_b = repo
            .create(&project.id, "Note B", "beta", "reference", "[]")
            .await
            .unwrap();

        let manager = Arc::new(SessionEndHookSessionManager::new(state));
        let (session_id, _transport) =
            rmcp::transport::streamable_http_server::SessionManager::create_session(&*manager)
                .await
                .unwrap();

        let server = manager.server_for_session(&session_id).await.unwrap();
        server.record_memory_read(&note_a.id).await;
        server.record_memory_read(&note_b.id).await;
        assert_eq!(
            server.recorded_note_ids().await,
            vec![note_a.id.clone(), note_b.id.clone()]
        );

        rmcp::transport::streamable_http_server::SessionManager::close_session(
            &*manager,
            &session_id,
        )
        .await
        .unwrap();

        let associations = repo.get_associations_for_note(&note_a.id).await.unwrap();
        assert_eq!(associations.len(), 1);
        let assoc = &associations[0];
        let pair = [assoc.note_a_id.as_str(), assoc.note_b_id.as_str()];
        assert!(pair.contains(&note_a.id.as_str()));
        assert!(pair.contains(&note_b.id.as_str()));
        assert!(manager.server_for_session(&session_id).await.is_none());
    }

    #[tokio::test]
    async fn graph_schema_resource_is_advertised_and_readable() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db);
        let server = DjinnMcpServer::new(state);

        let info = server.get_info();
        assert!(
            info.capabilities.resources.is_some(),
            "server should advertise MCP resources capability"
        );

        let templates = server.all_resource_templates();
        let graph_template = templates
            .resource_templates
            .iter()
            .find(|template| template.uri_template == "djinn://project/{id}/graph-schema")
            .expect("graph schema resource template is advertised");
        assert_eq!(graph_template.name, "project_graph_schema");
        assert_eq!(
            graph_template.mime_type.as_deref(),
            Some("application/json")
        );

        let result = server
            .read_resource_uri("djinn://project/test-project/graph-schema".to_string())
            .expect("read graph schema resource");
        assert_eq!(result.contents.len(), 1);
        let text = match &result.contents[0] {
            rmcp::model::ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => {
                assert_eq!(uri, "djinn://project/test-project/graph-schema");
                assert_eq!(mime_type.as_deref(), Some("application/json"));
                text
            }
            other => panic!("expected text graph schema resource, got {other:?}"),
        };
        let payload: serde_json::Value = serde_json::from_str(text).expect("schema JSON");
        assert_eq!(payload["tool"]["name"], "code_graph");
        assert_eq!(payload["resource"]["project_id_or_slug"], "test-project");

        let operations = payload["operations"].as_array().expect("operations array");
        for expected in [
            "search",
            "describe",
            "neighbors",
            "impact",
            "context",
            "query_subgraph",
        ] {
            assert!(
                operations
                    .iter()
                    .any(|operation| operation["name"] == expected),
                "graph schema should include operation {expected}"
            );
        }
        let node_concepts = payload["node_concepts"]
            .as_array()
            .expect("node concepts array");
        assert!(
            node_concepts
                .iter()
                .any(|concept| concept["name"] == "symbol")
        );
        assert!(
            node_concepts
                .iter()
                .any(|concept| concept["name"] == "file")
        );
        let edge_concepts = payload["edge_concepts"]
            .as_array()
            .expect("edge concepts array");
        assert!(
            edge_concepts
                .iter()
                .any(|concept| concept["name"] == "calls")
        );
        assert!(
            edge_concepts
                .iter()
                .any(|concept| concept["name"] == "imports")
        );
    }

    // The legacy propose_adr_* dispatch tests were removed with the old
    // proposal pipeline. The global Proposals layer (project-independent
    // `proposals` entity) replaces it — the next tests exercise its dispatch
    // routing end to end.

    #[tokio::test]
    async fn dispatch_tool_routes_proposal_create_show_and_target() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        // Create a global proposal targeting the project.
        let created = server
            .dispatch_tool(
                "proposal_create",
                json!({
                    "title": "Block invoice payments during collection",
                    "body": "## Problem\nGateways are the wrong boundary.",
                    "acceptance_criteria": ["Enforce centrally", "Fail open"],
                    "target_projects": [project.slug()],
                }),
            )
            .await
            .expect("dispatch proposal_create");
        assert_eq!(created.get("error"), None);
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id")
            .to_string();
        assert_eq!(
            created.get("status").and_then(|v| v.as_str()),
            Some("draft")
        );
        assert_eq!(
            created
                .get("short_id")
                .and_then(|v| v.as_str())
                .map(str::len),
            Some(4)
        );

        // Show bundles the proposal, its targets, and (empty) feedback.
        let shown = server
            .dispatch_tool("proposal_show", json!({ "id": id }))
            .await
            .expect("dispatch proposal_show");
        assert_eq!(shown.get("error"), None);
        let targets = shown
            .get("targets")
            .and_then(|v| v.as_array())
            .expect("targets array");
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].get("role").and_then(|v| v.as_str()),
            Some("primary")
        );

        // Re-target: remove the original and the list reflects it (Sam case).
        let removed = server
            .dispatch_tool(
                "proposal_remove_target",
                json!({ "id": id, "project": project.slug() }),
            )
            .await
            .expect("dispatch proposal_remove_target");
        assert_eq!(removed.get("error"), None);
        assert_eq!(
            removed
                .get("targets")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0)
        );
    }

    #[tokio::test]
    async fn dispatch_tool_routes_proposal_list_and_feedback() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        let created = server
            .dispatch_tool("proposal_create", json!({ "title": "Feedback proposal" }))
            .await
            .expect("dispatch proposal_create");
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id")
            .to_string();

        // Global list (no project arg) returns the proposal.
        let listed = server
            .dispatch_tool("proposal_list", json!({}))
            .await
            .expect("dispatch proposal_list");
        assert_eq!(listed.get("total_count").and_then(|v| v.as_i64()), Some(1));

        // Two feedback entries: a human comment and an AI-authored one.
        server
            .dispatch_tool(
                "proposal_feedback_add",
                json!({ "proposal_id": id, "body": "what about X?" }),
            )
            .await
            .expect("dispatch proposal_feedback_add (comment)");
        let ai = server
            .dispatch_tool(
                "proposal_feedback_add",
                json!({
                    "proposal_id": id,
                    "body": "enforce in svc-invoice not the gateway",
                    "author_kind": "ai",
                    "author_model": "claude-opus-4-8",
                }),
            )
            .await
            .expect("dispatch proposal_feedback_add (ai)");
        let feedback_id = ai
            .get("feedback")
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_str())
            .expect("feedback id")
            .to_string();

        // Resolve it as addressed in revision 2.
        let resolved = server
            .dispatch_tool(
                "proposal_feedback_resolve",
                json!({ "id": feedback_id, "resolved_revision_seq": 2 }),
            )
            .await
            .expect("dispatch proposal_feedback_resolve");
        assert!(
            resolved
                .get("feedback")
                .and_then(|f| f.get("resolved_at"))
                .and_then(|v| v.as_str())
                .is_some()
        );
        assert_eq!(
            resolved
                .get("feedback")
                .and_then(|f| f.get("resolved_revision_seq"))
                .and_then(|v| v.as_i64()),
            Some(2)
        );

        let shown = server
            .dispatch_tool("proposal_show", json!({ "id": id }))
            .await
            .expect("dispatch proposal_show");
        assert_eq!(
            shown
                .get("feedback")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(2)
        );
    }
    #[tokio::test]
    async fn proposal_show_includes_manual_done_status_history_event() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        // Create a proposal with a ready body so the readiness gate allows
        // the `in_review` transition.
        let created = server
            .dispatch_tool(
                "proposal_create",
                json!({
                    "title": "External implementation",
                    "body": "# Problem\nUsers cannot do X.\n\n# Scope\nIn scope: Y.\n\n# Objectives\nDeliver A.\n\n# Dependencies\nNone.\n\n# Open Questions\nWhat if D fails?\n\n## File map\n```file-map\nsrc/main.rs\n```\n",
                    "acceptance_criteria": ["API returns 200"],
                    "target_projects": [project.slug()],
                }),
            )
            .await
            .expect("dispatch proposal_create");
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id")
            .to_string();

        server
            .dispatch_tool(
                "proposal_update",
                json!({ "id": id, "status": "in_review" }),
            )
            .await
            .expect("dispatch proposal_update in_review");
        server
            .dispatch_tool("proposal_update", json!({ "id": id, "status": "done" }))
            .await
            .expect("dispatch proposal_update done");

        let shown = server
            .dispatch_tool("proposal_show", json!({ "id": id }))
            .await
            .expect("dispatch proposal_show");
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("revisions array");
        assert_eq!(revisions.len(), 2);
        assert_eq!(
            revisions[0].get("event_kind").and_then(|v| v.as_str()),
            Some("spec_revision")
        );
        let event = &revisions[1];
        assert_eq!(
            event.get("event_kind").and_then(|v| v.as_str()),
            Some("status_change")
        );
        assert_eq!(
            event.get("status_from").and_then(|v| v.as_str()),
            Some("in_review")
        );
        assert_eq!(
            event.get("status_to").and_then(|v| v.as_str()),
            Some("done")
        );
        assert_eq!(event.get("seq").and_then(|v| v.as_i64()), Some(1));
        assert!(event.get("created_at").and_then(|v| v.as_str()).is_some());
    }

    /// `proposal_update` and `proposal_create` must keep the spec revision's
    /// `event_metadata` column NULL — the proposal MCP surface is an
    /// ordinary-edit path, not the targeted-patch primitive. Only the future
    /// patch tool (and its sibling callers) should ever populate metadata;
    /// every other surface must stay metadata-free so the `event_metadata`
    /// column is a precise signal of "this revision was authored by the
    /// targeted-patch primitive".
    #[tokio::test]
    async fn proposal_show_revisions_have_null_event_metadata_for_ordinary_updates() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        let created = server
            .dispatch_tool("proposal_create", json!({ "title": "Metadata Compat" }))
            .await
            .expect("dispatch proposal_create");
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id")
            .to_string();

        // A material ordinary edit — must stay metadata-free.
        server
            .dispatch_tool(
                "proposal_update",
                json!({
                    "id": id,
                    "title": "Metadata Compat v2",
                    "body": "v2 body",
                }),
            )
            .await
            .expect("dispatch proposal_update");

        let shown = server
            .dispatch_tool("proposal_show", json!({ "id": id }))
            .await
            .expect("dispatch proposal_show");
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("revisions array");
        // create seed + material update = 2 revisions, all spec_revision kind
        assert_eq!(revisions.len(), 2);
        for rev in revisions {
            assert_eq!(
                rev.get("event_kind").and_then(|v| v.as_str()),
                Some("spec_revision")
            );
            assert!(
                rev.get("event_metadata").is_none_or(|v| v.is_null()),
                "ordinary proposal_update/create revisions must surface null event_metadata, got {:?}",
                rev.get("event_metadata")
            );
        }
    }

    /// Parity check between the two MCP dispatch paths (see
    /// `dispatch.rs` vs the `#[tool_router]`-generated router).
    ///
    /// rmcp 0.16's `ToolRouter::call` needs a `Peer<RoleServer>` whose
    /// constructor is `pub(crate)`, so we can't invoke the router directly
    /// from our HTTP handler. Instead, we keep a hand-written match in
    /// `dispatch_tool` — which historically drifts out of sync every time
    /// someone adds a `#[tool]` and forgets the arm (users then see
    /// runtime "unknown MCP tool" errors).
    ///
    /// This test fails CI the moment a tool is added to the router but
    /// not to `dispatch_tool`. It doesn't care whether the tool actually
    /// executes — an arg-decode failure or a real runtime error means
    /// the match found the arm, which is all we need to prove parity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_registered_tool_has_a_dispatch_arm() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db);
        let server = DjinnMcpServer::new(state);

        let tool_names: Vec<String> = server
            .all_tool_schemas()
            .into_iter()
            .filter_map(|schema| {
                schema
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(
            !tool_names.is_empty(),
            "tool_router returned zero tools — check rmcp wiring"
        );

        let mut missing: Vec<String> = Vec::new();
        for name in &tool_names {
            // Empty args — we only care whether the dispatcher's match
            // recognizes the name, not whether the tool actually runs.
            // Run in a spawned task so a tool panicking on unrelated state
            // (e.g. "mysql pool requested from sqlite runtime") is caught
            // as a JoinError rather than failing the whole test. Any
            // panic means the arm exists; we only fail on the specific
            // "unknown MCP tool" error.
            let server = server.clone();
            let name_clone = name.clone();
            let handle =
                tokio::spawn(async move { server.dispatch_tool(&name_clone, json!({})).await });
            match handle.await {
                Ok(Ok(_)) => {} // arm exists, tool ran
                Ok(Err(msg)) if msg.starts_with(&format!("unknown MCP tool: '{name}'")) => {
                    missing.push(name.clone());
                }
                Ok(Err(_)) => {} // arm exists, tool failed for other reasons
                Err(e) if e.is_panic() => {} // arm exists, tool panicked (unrelated state)
                Err(e) => panic!("spawned task failed for {name}: {e}"),
            }
        }

        assert!(
            missing.is_empty(),
            "tools registered via #[tool] but missing from dispatch_tool match in dispatch.rs: {missing:#?}\n\
             Add a match arm for each in server/crates/djinn-control-plane/src/dispatch.rs"
        );
    }

    /// Extract the tool-name string literals that head the top-level match
    /// arms of `dispatch_tool` in `dispatch.rs`.
    ///
    /// The dispatcher is a flat `match name { "tool_name" => ... }` whose arms
    /// are each written as a single `            "tool_name" => ...` line (no
    /// `|`-combined or multi-line patterns — enforced by this test's exact
    /// set-equality with the registered tools). We parse the source rather
    /// than reflect because the arm names live only as literals in the match;
    /// there is no runtime list to introspect.
    fn dispatch_arm_tool_names() -> std::collections::BTreeSet<String> {
        // `include_str!` resolves relative to this source file (src/), so the
        // dispatcher is its sibling `dispatch.rs`.
        const DISPATCH_SRC: &str = include_str!("dispatch.rs");
        DISPATCH_SRC
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim_start();
                // Match-arm heads look like: "tool_name" => ...
                let rest = trimmed.strip_prefix('"')?;
                let (name, after) = rest.split_once('"')?;
                if !after.trim_start().starts_with("=>") {
                    return None;
                }
                // Tool names are snake_case identifiers; this filters out any
                // unrelated string-literal-then-`=>` constructs.
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    return None;
                }
                Some(name.to_string())
            })
            .collect()
    }

    /// G4 — reverse-direction (and exact-equality) drift guard.
    ///
    /// `every_registered_tool_has_a_dispatch_arm` proves
    /// `registered ⊆ dispatch_arms` (no tool is unroutable). It does NOT catch
    /// the other drift direction: an **orphan** arm in `dispatch.rs` that no
    /// longer corresponds to a registered `#[tool]` (e.g. a tool renamed/removed
    /// in the router but left behind — or fat-fingered — in the match). Such an
    /// arm is dead code that silently masks the real bug and can shadow intent.
    ///
    /// This test asserts the dispatcher's set of routable tool names is exactly
    /// equal to the set of `#[tool]`-registered names — failing loudly in either
    /// direction. Together with the sibling test it closes the full
    /// register↔dispatch sync hazard without the risky "generate one from the
    /// other" codegen rewrite.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_arms_exactly_match_registered_tools() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db);
        let server = DjinnMcpServer::new(state);

        let registered: std::collections::BTreeSet<String> = server
            .all_tool_schemas()
            .into_iter()
            .filter_map(|schema| {
                schema
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(
            !registered.is_empty(),
            "tool_router returned zero tools — check rmcp wiring"
        );

        let arms = dispatch_arm_tool_names();
        assert!(
            !arms.is_empty(),
            "parsed zero match arms from dispatch.rs — the parser in \
             dispatch_arm_tool_names() is out of sync with the file's shape"
        );

        // Orphan arms: routed by dispatch.rs but not registered via #[tool].
        let orphan: Vec<&String> = arms.difference(&registered).collect();
        // Unrouted tools: registered via #[tool] but no dispatch arm.
        // (Also covered by every_registered_tool_has_a_dispatch_arm; asserted
        //  here too so this one test fails on drift in EITHER direction.)
        let unrouted: Vec<&String> = registered.difference(&arms).collect();

        assert!(
            orphan.is_empty() && unrouted.is_empty(),
            "dispatch.rs match arms and #[tool]-registered tools have drifted.\n\
             Orphan arms (in dispatch.rs, NOT a registered tool — remove or fix the name): {orphan:#?}\n\
             Unrouted tools (registered via #[tool], NO dispatch arm — add one in dispatch.rs): {unrouted:#?}"
        );
    }

    // ── DjinnMcpServer corpus snapshot for tool-schema projection tests ──
    //
    // The `djinn-provider` crate cannot depend on `djinn-control-plane`
    // (cycle), so the `DjinnMcpServer::all_tool_schemas()` corpus is
    // committed as a JSON fixture under
    //   djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json
    //
    // This test either **asserts** the committed fixture matches the live
    // output (normal `cargo test`), or **writes** the fixture when the
    // environment variable `UPDATE_DJINN_MCP_SERVER_FIXTURE=1` is set.
    //
    // Regeneration:
    //   UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane --lib \
    //     server_tests::tests::djinn_mcp_server_corpus_fixture_is_current -- --nocapture

    /// Canonical fixture path relative to `CARGO_MANIFEST_DIR`.
    const DJINN_MCP_SERVER_FIXTURE_REL: &str =
        "../djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn djinn_mcp_server_corpus_fixture_is_current() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db);
        let server = DjinnMcpServer::new(state);

        let schemas = server.all_tool_schemas();
        assert!(
            !schemas.is_empty(),
            "DjinnMcpServer::all_tool_schemas() returned empty — check rmcp tool router wiring"
        );

        // Sort by tool name for deterministic ordering.
        let mut sorted = schemas;
        sorted.sort_by(|a, b| {
            let name_a = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let name_b = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            name_a.cmp(name_b)
        });

        let pretty =
            serde_json::to_string_pretty(&sorted).expect("all_tool_schemas must serialize to JSON");

        let fixture_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(DJINN_MCP_SERVER_FIXTURE_REL);

        if std::env::var("UPDATE_DJINN_MCP_SERVER_FIXTURE").as_deref() == Ok("1") {
            // Write mode: overwrite the committed fixture.
            if let Some(parent) = fixture_path.parent() {
                std::fs::create_dir_all(parent)
                    .unwrap_or_else(|e| panic!("create fixture dir {}: {e}", parent.display()));
            }
            std::fs::write(&fixture_path, &pretty)
                .unwrap_or_else(|e| panic!("write fixture {}: {e}", fixture_path.display()));
            // Fixture updated successfully
        } else {
            // Assert mode: the committed fixture must match.
            let committed = std::fs::read_to_string(&fixture_path).unwrap_or_else(|e| {
                panic!(
                    "read committed fixture {}: {e}\n\
                     Run with UPDATE_DJINN_MCP_SERVER_FIXTURE=1 to generate it.",
                    fixture_path.display()
                )
            });
            assert_eq!(
                pretty, committed,
                "DjinnMcpServer::all_tool_schemas() has drifted from the committed fixture.\n\
                 If this is intentional, regenerate with:\n\
                 UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane --lib \n\
                   server_tests::tests::djinn_mcp_server_corpus_fixture_is_current -- --nocapture"
            );
        }
    }

    // ── Dispatch-pause contract: strict-argument decoding + read-only status ─
    //
    // The dispatch-pause epic ships three MCP tools whose parameter structs
    // (DispatchPauseParams, DispatchResumeParams, DispatchPauseStatusParams)
    // are decorated with `#[serde(deny_unknown_fields)]`. That decoration is
    // the structural reason stray / mutation-looking fields fail the
    // dispatcher's `decode_args` step before the handler ever runs — which
    // is the only safe place to reject them (silent acceptance would let a
    // typo or a stale client fudge a "paused: false" field onto a write
    // and have it ignored, or sneak a mutation field onto the read-only
    // status endpoint).
    //
    // The `dispatch_tool` entry point in `dispatch.rs` centralizes decode
    // for every MCP tool and surfaces the failure as a `Result::Err` whose
    // message starts with `"invalid arguments for tool '{name}': "`. The
    // tests below exercise that path so a future change to the parameter
    // structs (or the decode surface) cannot silently weaken the contract.

    /// Build a one-shot `DjinnMcpServer` whose `EventBus` records every
    /// envelope it receives. The handler itself never reads the bus — it
    /// only sends — so this is the canonical way to prove a read-only call
    /// did not emit an event the handler shouldn't have.
    fn server_with_recording_event_bus(
        db: &Database,
    ) -> (
        DjinnMcpServer,
        Arc<std::sync::Mutex<Vec<DjinnEventEnvelope>>>,
    ) {
        let recorded: Arc<std::sync::Mutex<Vec<DjinnEventEnvelope>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_for_bus = Arc::clone(&recorded);
        let bus = EventBus::new(move |envelope| {
            recorded_for_bus
                .lock()
                .expect("recording event bus mutex poisoned")
                .push(envelope);
        });
        let state = crate::state::stubs::test_mcp_state_with_event_bus(db.clone(), bus);
        let server = DjinnMcpServer::new(state);
        (server, recorded)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_pause_tool_rejects_unknown_fields_via_decode() {
        let db = Database::open_in_memory().unwrap();
        let (server, _events) = server_with_recording_event_bus(&db);

        // `paused: false` is the canonical "stray mutation-looking field"
        // the proposal warns about: a stale or buggy client could ship it
        // on a pause and expect the tool to refuse instead of silently
        // ignoring it. With `#[serde(deny_unknown_fields)]` on
        // DispatchPauseParams, the decoder surfaces an invalid-arguments
        // error *before* the admin gate or repository run, so the
        // mutation never gets a chance to be partially applied.
        let result = server
            .dispatch_tool(
                "dispatch_pause",
                json!({
                    "scope": "global",
                    "reason": "x",
                    "paused": false,
                }),
            )
            .await;
        let err = result.expect_err("dispatch_pause with stray `paused` field must fail decode");
        assert!(
            err.contains("invalid arguments for tool 'dispatch_pause'"),
            "decode error must use the standard `invalid arguments for tool '...'` \
             shape so clients can branch on it, got: {err}"
        );
        assert!(
            err.contains("unknown field"),
            "decode error must mention the unknown field name, got: {err}"
        );
        assert!(
            err.contains("paused"),
            "decode error must name the offending field `paused`, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_resume_tool_rejects_unknown_fields_via_decode() {
        let db = Database::open_in_memory().unwrap();
        let (server, _events) = server_with_recording_event_bus(&db);

        // `paused: true` mirrors the pause-side test: a stale client should
        // not be able to sneak a mutation-looking field onto a resume. The
        // contract is symmetric — both `dispatch_pause` and
        // `dispatch_resume` carry `#[serde(deny_unknown_fields)]`.
        let result = server
            .dispatch_tool(
                "dispatch_resume",
                json!({
                    "scope": "global",
                    "paused": true,
                }),
            )
            .await;
        let err = result.expect_err("dispatch_resume with stray `paused` field must fail decode");
        assert!(
            err.contains("invalid arguments for tool 'dispatch_resume'"),
            "decode error must use the standard `invalid arguments for tool '...'` shape, got: {err}"
        );
        assert!(
            err.contains("unknown field") && err.contains("paused"),
            "decode error must mention the unknown `paused` field, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_pause_status_tool_rejects_unknown_fields_via_decode() {
        let db = Database::open_in_memory().unwrap();
        let (server, _events) = server_with_recording_event_bus(&db);

        // Status is the read-only side; mutation-looking fields on it are
        // the most dangerous kind of silent contract drift (a buggy
        // client could attempt a write that the host UI marked
        // "read-only" because the schema says so, and have the call
        // succeed silently). The `deny_unknown_fields` decoder catches
        // the attempt before the handler runs.
        for stray in ["paused", "reason", "paused_by"] {
            let args = match stray {
                "reason" => json!({ "scope": "global", "reason": "should be rejected" }),
                "paused" => json!({ "scope": "global", "paused": false }),
                "paused_by" => json!({ "scope": "global", "paused_by": "attacker" }),
                _ => unreachable!("test stray field list is exhaustive"),
            };
            let result = server.dispatch_tool("dispatch_pause_status", args).await;
            let err = result
                .expect_err("dispatch_pause_status with stray `{stray}` field must fail decode");
            assert!(
                err.contains("invalid arguments for tool 'dispatch_pause_status'"),
                "decode error must use the standard shape, got: {err}"
            );
            assert!(
                err.contains("unknown field") && err.contains(stray),
                "decode error must mention the offending `{stray}` field, got: {err}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_pause_status_does_not_mutate_state_or_emit_events() {
        // The flagship read-only regression for the dispatch-pause epic.
        // Three guarantees pinned in one test so a future refactor of the
        // status handler cannot violate any of them silently:
        //
        // 1. Setting a real global pause (via the repository directly —
        //    bypasses the admin gate so the test is hermetic) populates
        //    persisted state.
        // 2. Calling `dispatch_pause_status` with valid args returns the
        //    expected `ok: true, scope: global, current.paused_by = "admin"`
        //    payload without altering the stored `DispatchPause` value.
        // 3. The recording event bus records ZERO `dispatch_pause` events
        //    for the status call — only the direct repository pause that
        //    seeded the test fixture is allowed to appear in the log.
        //
        // If a future change wires a `EventBus::send` into the status
        // handler (e.g. as a "last_polled" telemetry side-effect), the
        // event-count assertion below will catch it before it ships to
        // production and starts polluting SSE streams.
        let db = Database::open_in_memory().unwrap();
        let (server, events) = server_with_recording_event_bus(&db);

        // Seed a global pause directly via the repository so the test
        // doesn't depend on the admin gate. The repository itself emits
        // a `dispatch_pause.changed` event; that single event is the
        // baseline the status-call event-count delta is measured against.
        let seed_repo = DispatchPauseRepository::new(db.clone(), EventBus::noop());
        seed_repo
            .pause(
                DispatchPauseTarget::global(),
                DispatchPause {
                    paused_by: "admin".to_string(),
                    paused_at: "2026-06-12T00:00:00.000Z".to_string(),
                    reason: "seeded by test".to_string(),
                    expires_at: None,
                },
            )
            .await
            .expect("seed pause should succeed");

        // The seed pause runs through `EventBus::noop()` (constructed
        // above), so the recording bus should see zero events from the
        // seed step. Snapshot the baseline count now.
        let baseline_event_count = events.lock().unwrap().len();

        // Snapshot the stored pause so we can assert the status call
        // did not touch it. The state struct is `Clone + PartialEq`, so
        // we can compare directly after the status call.
        let stored_before = seed_repo
            .get_status()
            .await
            .expect("read stored pause state before status call");

        // Exercise the status tool with valid args. We deliberately pass
        // a scope filter (`global`) so the call takes the single-scope
        // branch (returns `current`) rather than the all-scopes branch
        // (returns `state`) — both branches must be non-mutating, and
        // testing the more-specific branch first gives the most direct
        // contract check.
        let result = server
            .dispatch_tool("dispatch_pause_status", json!({ "scope": "global" }))
            .await
            .expect("dispatch_pause_status with valid args must succeed");
        assert_eq!(result["ok"], true, "status call must return ok=true");
        assert_eq!(
            result["scope"], "global",
            "status call must echo the scope filter it was given"
        );
        assert_eq!(
            result["current"]["paused_by"], "admin",
            "status call must surface the seeded pause's paused_by"
        );
        assert_eq!(
            result["current"]["reason"], "seeded by test",
            "status call must surface the seeded pause's reason"
        );

        // Re-read the stored pause and assert it is byte-identical to the
        // snapshot taken before the status call. If the handler ever
        // evolves a side effect (e.g. a `paused_at: now()` touch on
        // status read), the `paused_at` field would shift and this
        // assertion would fail.
        let stored_after = seed_repo
            .get_status()
            .await
            .expect("read stored pause state after status call");
        assert_eq!(
            stored_before, stored_after,
            "dispatch_pause_status must not mutate persisted pause state"
        );

        // No new events from the status call. The seed pause emitted
        // through `EventBus::noop()`, so the only acceptable delta is
        // zero. A future change that adds a status-side `EventBus::send`
        // (e.g. telemetry) would push the count above the baseline.
        let post_status_event_count = events.lock().unwrap().len();
        let recorded_entity_types: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.entity_type().to_owned())
            .collect();
        assert_eq!(
            post_status_event_count,
            baseline_event_count,
            "dispatch_pause_status must not emit any events; \
             recorded {} new event(s) (entity_types: {recorded_entity_types:?})",
            post_status_event_count - baseline_event_count,
        );

        // Belt-and-braces: also assert the recorded event list contains
        // no `dispatch_pause.changed` envelope tagged as a status read.
        // The seed step's `EventBus::noop()` is the structural reason the
        // list is empty in this revision; this assertion is the
        // forward-compatible shape of the contract — a future test that
        // seeds through the recording bus would still pass it.
        let recorded_events = events.lock().unwrap();
        let dispatch_pause_events: Vec<_> = recorded_events
            .iter()
            .filter(|e| e.entity_type() == "dispatch_pause" && e.action() == "changed")
            .collect();
        assert!(
            dispatch_pause_events.is_empty(),
            "dispatch_pause_status must not emit dispatch_pause.changed events; \
             recorded: {dispatch_pause_events:#?}"
        );
    }

    // ── Deterministic DoR gate regression tests ──────────────────────────

    /// A draft proposal with a complete body, target, and concrete AC
    /// successfully transitions to `in_review`.
    #[tokio::test]
    async fn proposal_update_draft_to_in_review_with_ready_body_succeeds() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        let created = server
            .dispatch_tool(
                "proposal_create",
                json!({
                    "title": "Ready proposal",
                    "body": "# Problem\nUsers cannot do X.\n\n# Scope\nIn scope: Y.\n\n# Objectives\nDeliver A.\n\n# Dependencies\nNone.\n\n# Open Questions\nWhat if D fails?\n\nEntry points: src/main.rs.\n",
                    "acceptance_criteria": ["API returns 200"],
                    "target_projects": [project.slug()],
                }),
            )
            .await
            .expect("dispatch proposal_create");
        assert_eq!(created.get("error"), None, "create should succeed");
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id");

        let updated = server
            .dispatch_tool(
                "proposal_update",
                json!({ "id": id, "status": "in_review" }),
            )
            .await
            .expect("dispatch proposal_update");
        assert_eq!(
            updated.get("error"),
            None,
            "in_review transition should succeed for a ready proposal"
        );
        assert_eq!(
            updated.get("status").and_then(|v| v.as_str()),
            Some("in_review")
        );
    }

    /// Updating a draft with missing targets/sections to `in_review` fails
    /// and does not bump `latest_revision_seq`.
    #[tokio::test]
    async fn proposal_update_draft_to_in_review_fails_with_missing_readiness() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        // Minimal proposal: title only, no body sections or targets.
        let created = server
            .dispatch_tool("proposal_create", json!({ "title": "Minimal proposal" }))
            .await
            .expect("dispatch proposal_create");
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id");

        let result = server
            .dispatch_tool(
                "proposal_update",
                json!({ "id": id, "status": "in_review" }),
            )
            .await
            .expect("dispatch proposal_update");
        let error = result.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "should fail readiness check");
        let error = error.unwrap();
        assert!(
            error.contains("proposal not ready for review"),
            "error should mention readiness: {error}"
        );
        // Should mention specific missing sections.
        assert!(
            error.contains("problem"),
            "error should mention missing problem: {error}"
        );
        assert!(
            error.contains("target"),
            "error should mention missing targets: {error}"
        );

        // The proposal status should NOT have changed.
        let shown = server
            .dispatch_tool("proposal_show", json!({ "id": id }))
            .await
            .expect("dispatch proposal_show");
        assert_eq!(
            shown
                .get("proposal")
                .and_then(|p| p.get("status"))
                .and_then(|v| v.as_str()),
            Some("draft"),
            "status should remain draft after failed readiness gate"
        );
    }

    /// Existing MDX validation errors still win over readiness errors for
    /// malformed MDX.
    #[tokio::test]
    async fn proposal_update_mdx_validation_wins_over_readiness() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let server = DjinnMcpServer::new(state);

        // Create a valid draft proposal with markdown body_format.
        let created = server
            .dispatch_tool(
                "proposal_create",
                json!({
                    "title": "MDX proposal",
                    "body": "# Problem\nNeeds MDX.\n\n# Scope\nX.\n\n# Objectives\nY.\n\n# Dependencies\nNone.\n\n# Open Questions\nN/A.\n\nEntry points: src/lib.rs.\n",
                    "target_projects": [project.slug()],
                    "acceptance_criteria": ["API returns 200"],
                }),
            )
            .await
            .expect("dispatch proposal_create");
        assert!(
            created.get("error").is_none(),
            "proposal_create should succeed: {created:#?}"
        );
        let id = created
            .get("id")
            .and_then(|v| v.as_str())
            .expect("proposal id");

        // Update with malformed MDX body that would also fail readiness.
        // The MDX validation error should take precedence.
        let result = server
            .dispatch_tool(
                "proposal_update",
                json!({
                    "id": id,
                    "body": "<UnknownBlock>bad</UnknownBlock>",
                    "body_format": "mdx",
                    "status": "in_review",
                }),
            )
            .await
            .expect("dispatch proposal_update");
        let error = result.get("error").and_then(|v| v.as_str());
        assert!(error.is_some(), "should fail validation");
        let error = error.unwrap();
        // MDX validation runs before readiness, so the error should be about
        // MDX, not readiness.
        assert!(
            !error.contains("proposal not ready for review"),
            "MDX validation error should win over readiness: {error}"
        );
    }
}
