#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use djinn_core::events::EventBus;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use rmcp::{Json, handler::server::wrapper::Parameters};
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
        for _ in 0..40 {
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
        let canonical =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        let _guard = PathCleanupGuard::new(canonical);
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let Json(created) = server
            .memory_write(Parameters(WriteParams {
                project: project.slug(),
                title: "Summary Note".to_string(),
                content: "Sentence one. Sentence two.\n\nMore context follows here.".to_string(),
                note_type: "reference".to_string(),
                status: None,
                tags: None,
                scope_paths: None,
            }))
            .await;

        assert!(created.error.is_none());
        let note_id = created.id.clone().expect("memory_write returns note id");
        let created_note = repo.get(&note_id).await.unwrap().unwrap();
        assert!(created_note.abstract_.is_none());
        assert!(created_note.overview.is_none());

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
        let canonical =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&canonical).expect("create canonical project dir");
        let _guard = PathCleanupGuard::new(canonical.clone());
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let legacy = repo
            .create(
                &project.id,
                &canonical,
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
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note_a = repo
            .create(
                &project.id,
                tmp.path(),
                "Note A",
                "alpha",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let note_b = repo
            .create(&project.id, tmp.path(), "Note B", "beta", "reference", "[]")
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
    async fn proposal_pipeline_regression_worktree_draft_survives_sync_and_is_listed() {
        let worktree_tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        // `propose_adr_list` scans `project_dir(owner, repo)`, not the note's
        // worktree path. Point the canonical NoteRepository writes at the same
        // derived location so what's synced is what's listed.
        let canonical_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&canonical_path).expect("create canonical project dir");
        let _canonical_guard = PathCleanupGuard::new(canonical_path.clone());
        let server = DjinnMcpServer::new(state);
        let canonical_repo = NoteRepository::new(db.clone(), EventBus::noop());
        let worktree_repo = NoteRepository::new(db.clone(), EventBus::noop())
            .with_worktree_root(Some(worktree_tmp.path().to_path_buf()));

        let created = worktree_repo
            .create(
                &project.id,
                &canonical_path,
                "Pipeline Draft",
                "---\ntitle: Pipeline Draft\nwork_shape: epic\noriginating_spike_id: ih6u-regression\n---\n\n# Pipeline Draft\n\nSurvives task completion.\n",
                "adr",
                "[]",
            )
            .await
            .expect("create worktree adr draft");

        let moved = worktree_repo
            .move_note(
                &created.id,
                &canonical_path,
                "Pipeline Draft",
                "proposed_adr",
            )
            .await
            .expect("move draft into proposed folder");
        assert_eq!(moved.folder, "decisions/proposed");
        let proposed_path = worktree_tmp
            .path()
            .join(".djinn/decisions/proposed/pipeline-draft.md");
        assert!(
            proposed_path.exists(),
            "draft should exist in worktree proposed folder before close sync"
        );
        std::fs::write(
            &proposed_path,
            "---\ntitle: Pipeline Draft\nwork_shape: epic\noriginating_spike_id: ih6u-regression\n---\n\n# Pipeline Draft\n\nSurvives task completion.\n",
        )
        .expect("overwrite moved draft with proposal frontmatter");

        let synced = worktree_repo
            .sync_worktree_notes_to_canonical(&project.id, &canonical_path, worktree_tmp.path())
            .await
            .expect("sync worktree notes to canonical memory");
        assert_eq!(synced, 1);

        let canonical = canonical_repo
            .get_by_permalink(&project.id, "decisions/proposed/pipeline-draft")
            .await
            .expect("canonical lookup succeeds")
            .expect("canonical proposed ADR exists after sync");
        assert_eq!(canonical.note_type, "proposed_adr");
        assert!(
            canonical
                .file_path
                .ends_with(".djinn/decisions/proposed/pipeline-draft.md")
        );

        let response = server
            .dispatch_tool(
                "propose_adr_list",
                json!({ "project": project.slug() }),
            )
            .await
            .expect("dispatch propose_adr_list after sync");

        assert_eq!(response.get("error"), None);
        let items = response
            .get("items")
            .and_then(|value| value.as_array())
            .expect("items array");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item.get("id").and_then(|value| value.as_str()),
            Some("pipeline-draft")
        );
        assert_eq!(
            item.get("title").and_then(|value| value.as_str()),
            Some("Pipeline Draft")
        );
        assert!(item.get("mtime").and_then(|value| value.as_str()).is_some());
    }

    #[tokio::test]
    async fn dispatch_tool_routes_propose_adr_list() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let canonical =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        let _guard = PathCleanupGuard::new(canonical.clone());
        let server = DjinnMcpServer::new(state);

        let proposed_dir = canonical.join(".djinn/decisions/proposed");
        std::fs::create_dir_all(&proposed_dir).unwrap();
        std::fs::write(
            proposed_dir.join("adr-999-routing.md"),
            "---\ntitle: Routed ADR\nwork_shape: epic\noriginating_spike_id: spk1\n---\n\n# Routed ADR\n",
        )
        .unwrap();

        let response = server
            .dispatch_tool(
                "propose_adr_list",
                json!({ "project": project.slug() }),
            )
            .await
            .expect("dispatch propose_adr_list");

        assert_eq!(response.get("error"), None);
        let items = response
            .get("items")
            .and_then(|value| value.as_array())
            .expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("id").and_then(|value| value.as_str()),
            Some("adr-999-routing")
        );
        assert!(
            items[0]
                .get("mtime")
                .and_then(|value| value.as_str())
                .is_some()
        );
    }

    #[tokio::test]
    async fn dispatch_tool_routes_propose_adr_show() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let canonical =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        let _guard = PathCleanupGuard::new(canonical.clone());
        let server = DjinnMcpServer::new(state);

        let proposed_dir = canonical.join(".djinn/decisions/proposed");
        std::fs::create_dir_all(&proposed_dir).unwrap();
        std::fs::write(
            proposed_dir.join("adr-999-routing.md"),
            "---\ntitle: Routed ADR\nwork_shape: epic\n---\n\n# Routed ADR\n\nBody text\n",
        )
        .unwrap();

        let response = server
            .dispatch_tool(
                "propose_adr_show",
                json!({ "project": project.slug(), "id": "adr-999-routing" }),
            )
            .await
            .expect("dispatch propose_adr_show");

        assert_eq!(response.get("error"), None);
        let adr = response
            .get("adr")
            .and_then(|value| value.as_object())
            .expect("adr object");
        assert_eq!(
            adr.get("id").and_then(|value| value.as_str()),
            Some("adr-999-routing")
        );
        assert!(
            adr.get("body")
                .and_then(|value| value.as_str())
                .is_some_and(|body| body.contains("Body text"))
        );
    }

    #[tokio::test]
    async fn dispatch_tool_routes_propose_adr_accept() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let canonical =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        let _guard = PathCleanupGuard::new(canonical.clone());
        let server = DjinnMcpServer::new(state);

        let proposed_dir = canonical.join(".djinn/decisions/proposed");
        std::fs::create_dir_all(&proposed_dir).unwrap();
        std::fs::write(
            proposed_dir.join("adr-999-routing.md"),
            "---\ntitle: Routed ADR\nwork_shape: architectural\n---\n\n# Routed ADR\n",
        )
        .unwrap();

        let response = server
            .dispatch_tool(
                "propose_adr_accept",
                json!({
                    "project": project.slug(),
                    "id": "adr-999-routing",
                    "create_epic": false
                }),
            )
            .await
            .expect("dispatch propose_adr_accept");

        assert_eq!(response.get("error"), None);
        let accepted_path = response
            .get("accepted_path")
            .and_then(|value| value.as_str())
            .expect("accepted path");
        assert!(accepted_path.ends_with(".djinn/decisions/adr-999-routing.md"));
        assert!(
            canonical
                .join(".djinn/decisions/adr-999-routing.md")
                .exists()
        );
        assert!(
            !canonical
                .join(".djinn/decisions/proposed/adr-999-routing.md")
                .exists()
        );
    }

    #[tokio::test]
    async fn dispatch_tool_routes_propose_adr_reject() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, std::path::Path::new("")).await;
        let canonical =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        let _guard = PathCleanupGuard::new(canonical.clone());
        let server = DjinnMcpServer::new(state);

        let proposed_dir = canonical.join(".djinn/decisions/proposed");
        std::fs::create_dir_all(&proposed_dir).unwrap();
        std::fs::write(
            proposed_dir.join("adr-999-routing.md"),
            "---\ntitle: Routed ADR\n---\n\n# Routed ADR\n",
        )
        .unwrap();

        let response = server
            .dispatch_tool(
                "propose_adr_reject",
                json!({
                    "project": project.slug(),
                    "id": "adr-999-routing",
                    "reason": "Not aligned with current direction"
                }),
            )
            .await
            .expect("dispatch propose_adr_reject");

        assert_eq!(response.get("error"), None);
        assert_eq!(
            response.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert!(
            !canonical
                .join(".djinn/decisions/proposed/adr-999-routing.md")
                .exists()
        );
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
}
