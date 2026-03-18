//! Consolidated MCP contract and integration tests.
//! These tests exercise MCP tool handlers via the full HTTP stack.

#[cfg(test)]
mod board_tools {
    use serde_json::json;

    use crate::test_helpers::{
        create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
        create_test_task, initialize_mcp_session, mcp_call_tool,
    };

    #[tokio::test]
    async fn board_health_with_no_pool_returns_response_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let response = mcp_call_tool(
            &app,
            &session_id,
            "board_health",
            json!({ "project": project.path }),
        )
        .await;

        assert!(response.get("stale_tasks").is_some());
        assert!(response.get("epic_stats").is_some());
        assert!(response.get("review_queue").is_some());
        assert!(response.get("stale_threshold_hours").is_some());
    }

    #[tokio::test]
    async fn board_reconcile_releases_stuck_in_progress_without_active_session() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = ?1")
            .bind(&task.id)
            .execute(db.pool())
            .await
            .expect("set task in_progress");
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
            .bind(&task.id)
            .execute(db.pool())
            .await
            .expect("age task beyond stale threshold");

        let state =
            crate::server::AppState::new(db.clone(), tokio_util::sync::CancellationToken::new());
        state.initialize_agents().await;
        let app = crate::server::router(state);
        let session_id = initialize_mcp_session(&app).await;

        let response = mcp_call_tool(
            &app,
            &session_id,
            "board_reconcile",
            json!({ "project": project.path }),
        )
        .await;

        assert!(response.get("healed_tasks").is_some());

        let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?1")
            .bind(&task.id)
            .fetch_one(db.pool())
            .await
            .expect("fetch task status");
        assert_eq!(status, "open");
    }
}

// ── execution_tools ───────────────────────────────────────────────────────────

mod execution_tools {
    use serde_json::json;

    use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

    #[tokio::test]
    async fn execution_start_without_pool_or_coordinator_returns_error_shape() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let response = mcp_call_tool(&app, &session_id, "execution_start", json!({})).await;

        assert_eq!(response["ok"], false);
        assert!(response.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn execution_status_without_pool_or_coordinator_returns_error_shape() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let response = mcp_call_tool(&app, &session_id, "execution_status", json!({})).await;

        assert_eq!(response["ok"], false);
        assert!(response.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn execution_pause_and_resume_without_pool_or_coordinator_return_error_shapes() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let pause = mcp_call_tool(
            &app,
            &session_id,
            "execution_pause",
            json!({"mode":"graceful"}),
        )
        .await;
        assert_eq!(pause["ok"], false);
        assert!(pause.get("error").and_then(|v| v.as_str()).is_some());

        let resume = mcp_call_tool(&app, &session_id, "execution_resume", json!({})).await;
        assert_eq!(resume["ok"], false);
        assert!(resume.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn execution_kill_task_with_nonexistent_task_returns_error_shape() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let response = mcp_call_tool(
            &app,
            &session_id,
            "execution_kill_task",
            json!({"task_id":"nonexistent-task-id"}),
        )
        .await;

        assert_eq!(response["ok"], false);
        assert!(response.get("error").and_then(|v| v.as_str()).is_some());
    }
}

// ── credential_tools ──────────────────────────────────────────────────────────

mod credential_tools {
    use serde_json::json;

    use crate::events::EventBus;
    use crate::test_helpers::{
        create_test_app_with_db, create_test_db, initialize_mcp_session, mcp_call_tool,
    };
    use djinn_provider::repos::CredentialRepository;

    #[tokio::test]
    async fn credential_set_success_shape() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let session_id = initialize_mcp_session(&app).await;

        let res = mcp_call_tool(
            &app,
            &session_id,
            "credential_set",
            json!({"provider_id":"anthropic","key_name":"ANTHROPIC_API_KEY","api_key":"secret-1"}),
        )
        .await;

        assert_eq!(res["ok"], true);
        assert_eq!(res["success"], true);
        assert_eq!(res["key_name"], "ANTHROPIC_API_KEY");
        assert!(res["id"].as_str().unwrap_or_default().len() > 8);

        let row: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT encrypted_value FROM credentials WHERE key_name = ?1")
                .bind("ANTHROPIC_API_KEY")
                .fetch_optional(db.pool())
                .await
                .unwrap();
        let ciphertext = row.expect("missing credential row");
        assert!(!ciphertext.is_empty());
        assert_ne!(ciphertext, b"secret-1");

        let repo = CredentialRepository::new(db.clone(), EventBus::noop());
        let decrypted = repo
            .get_decrypted("ANTHROPIC_API_KEY")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decrypted, "secret-1");
    }

    #[tokio::test]
    async fn credential_list_hides_secrets() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "credential_set",
            json!({"provider_id":"openai","key_name":"OPENAI_API_KEY","api_key":"super-secret"}),
        )
        .await;

        let list = mcp_call_tool(&app, &session_id, "credential_list", json!({})).await;
        let first = list["credentials"].as_array().unwrap().first().unwrap();
        assert_eq!(first["key_name"], "OPENAI_API_KEY");
        assert!(first.get("api_key").is_none());
        assert!(first.get("ciphertext").is_none());
    }

    #[tokio::test]
    async fn credential_delete_removes_credential() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "credential_set",
            json!({"provider_id":"openai","key_name":"OPENAI_API_KEY","api_key":"a"}),
        )
        .await;

        let deleted = mcp_call_tool(
            &app,
            &session_id,
            "credential_delete",
            json!({"key_name":"OPENAI_API_KEY"}),
        )
        .await;
        assert_eq!(deleted["ok"], true);
        assert_eq!(deleted["deleted"], true);
    }
}

// ── memory_tools ──────────────────────────────────────────────────────────────

mod memory_tools {
    use std::path::Path;

    use djinn_db::ProjectRepository;
    use djinn_mcp::tools::memory_tools::*;
    use serde_json::json;

    use crate::events::EventBus;
    use crate::test_helpers::{
        create_test_app, create_test_app_with_db, create_test_db, create_test_epic,
        create_test_project, create_test_project_with_dir, initialize_mcp_session,
        initialize_mcp_session_with_headers, mcp_call_tool, mcp_call_tool_with_headers,
    };
    use djinn_db::NoteRepository;

    #[tokio::test]
    async fn mcp_memory_write_success_shape_and_duplicate_permalink_error() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db.clone());
        let session_id = initialize_mcp_session(&app).await;

        let created = mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({
                "project": project,
                "title": "Write Contract Note",
                "content": "body",
                "type": "adr"
            }),
        )
        .await;

        assert!(created.get("id").and_then(|v| v.as_str()).is_some());
        assert_eq!(created["title"], "Write Contract Note");
        assert_eq!(created["note_type"], "adr");
        assert!(created.get("permalink").and_then(|v| v.as_str()).is_some());

        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project_id: String = project_repo.resolve_or_create(project).await.unwrap();
        let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note = note_repo
            .get_by_permalink(&project_id, created["permalink"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(
            Path::new(&note.file_path).exists(),
            "canonical DB path is stored at {} even though writes may target worktrees",
            note.file_path
        );

        let duplicate = mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({
                "project": project,
                "title": "Write Contract Note",
                "content": "body-2",
                "type": "adr"
            }),
        )
        .await;

        assert!(duplicate.get("error").is_some());
    }

    #[tokio::test]
    async fn mcp_memory_read_by_permalink_by_title_and_not_found_error() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let created = mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({
                "project": project,
                "title": "Read Contract Note",
                "content": "read me",
                "type": "reference"
            }),
        )
        .await;

        let by_permalink = mcp_call_tool(
            &app,
            &session_id,
            "memory_read",
            json!({ "project": project, "identifier": created["permalink"] }),
        )
        .await;
        assert_eq!(by_permalink["title"], "Read Contract Note");

        let by_title = mcp_call_tool(
            &app,
            &session_id,
            "memory_read",
            json!({ "project": project, "identifier": "Read Contract Note" }),
        )
        .await;
        assert_eq!(by_title["permalink"], created["permalink"]);

        let missing = mcp_call_tool(
            &app,
            &session_id,
            "memory_read",
            json!({ "project": project, "identifier": "does-not-exist" }),
        )
        .await;
        assert!(missing.get("error").is_some());
    }

    #[tokio::test]
    async fn mcp_memory_search_returns_ranked_results_with_snippets_and_filters() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Rust Alpha", "content": "rust rust rust memory", "type": "reference"})).await;
        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Rust Beta", "content": "rust memory", "type": "reference"})).await;
        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "ADR Gamma", "content": "rust decision", "type": "adr"})).await;

        let searched = mcp_call_tool(
            &app,
            &session_id,
            "memory_search",
            json!({"project": project, "query": "rust", "limit": 10}),
        )
        .await;
        let results = searched["results"].as_array().unwrap();
        assert!(results.len() >= 2);
        assert!(results[0].get("snippet").is_some());

        let by_folder = mcp_call_tool(
            &app,
            &session_id,
            "memory_search",
            json!({"project": project, "query": "rust", "folder": "decisions"}),
        )
        .await;
        for r in by_folder["results"].as_array().unwrap() {
            assert_eq!(r["folder"], "decisions");
        }

        let by_type = mcp_call_tool(
            &app,
            &session_id,
            "memory_search",
            json!({"project": project, "query": "rust", "type": "adr"}),
        )
        .await;
        for r in by_type["results"].as_array().unwrap() {
            assert_eq!(r["note_type"], "adr");
        }
    }

    #[tokio::test]
    async fn mcp_memory_edit_append_prepend_replace_and_missing_note_error() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Edit Note", "content": "middle", "type": "reference"})).await;

        let appended = mcp_call_tool(&app, &session_id, "memory_edit", json!({"project": project, "identifier": "Edit Note", "operation": "append", "content": "tail"})).await;
        assert!(appended["content"].as_str().unwrap().contains("tail"));

        let prepended = mcp_call_tool(&app, &session_id, "memory_edit", json!({"project": project, "identifier": "Edit Note", "operation": "prepend", "content": "head"})).await;
        assert!(prepended["content"].as_str().unwrap().starts_with("head"));

        let replaced = mcp_call_tool(&app, &session_id, "memory_edit", json!({"project": project, "identifier": "Edit Note", "operation": "find_replace", "find_text": "middle", "content": "center"})).await;
        assert!(replaced["content"].as_str().unwrap().contains("center"));

        let missing = mcp_call_tool(&app, &session_id, "memory_edit", json!({"project": project, "identifier": "Missing", "operation": "append", "content": "x"})).await;
        assert!(missing.get("error").is_some());
    }

    #[tokio::test]
    async fn mcp_memory_move_changes_folder_title_and_permalink() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let created = mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Move Me", "content": "content", "type": "reference"})).await;

        let moved = mcp_call_tool(
            &app,
            &session_id,
            "memory_move",
            json!({"project": project, "identifier": created["permalink"], "title": "Moved Title", "type": "research"}),
        )
        .await;
        assert_eq!(moved["title"], "Moved Title");
        assert_eq!(moved["folder"], "research");
        assert_ne!(moved["permalink"], created["permalink"]);
    }

    #[tokio::test]
    async fn mcp_memory_delete_success_and_missing_note_error() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Delete Me", "content": "bye", "type": "reference"})).await;

        let deleted = mcp_call_tool(
            &app,
            &session_id,
            "memory_delete",
            json!({"project": project, "identifier": "Delete Me"}),
        )
        .await;
        assert_eq!(deleted["ok"], true);

        let missing = mcp_call_tool(
            &app,
            &session_id,
            "memory_delete",
            json!({"project": project, "identifier": "Delete Me"}),
        )
        .await;
        assert_eq!(missing["ok"], false);
        assert!(missing.get("error").is_some());
    }

    #[tokio::test]
    async fn mcp_memory_write_edit_delete_use_worktree_root_header_for_file_ops() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let worktree = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(worktree.path().join(".git")).expect("create synthetic .git dir");
        let app = create_test_app_with_db(db.clone());
        let worktree_header = worktree.path().to_string_lossy().to_string();
        let session_id = initialize_mcp_session_with_headers(
            &app,
            &[("x-djinn-worktree-root", &worktree_header)],
        )
        .await;

        let created = mcp_call_tool_with_headers(
            &app,
            &session_id,
            "memory_write",
            json!({"project": project, "title": "Worktree Note", "content": "alpha", "type": "reference"}),
            &[("x-djinn-worktree-root", &worktree_header)],
        )
        .await;

        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project_id: String = project_repo.resolve_or_create(project).await.unwrap();
        let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note = note_repo
            .get_by_permalink(&project_id, created["permalink"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();

        let canonical_path = Path::new(&note.file_path).to_path_buf();
        let worktree_path = worktree.path().join(".djinn/reference/worktree-note.md");
        assert_eq!(
            canonical_path,
            Path::new(project).join(".djinn/reference/worktree-note.md")
        );
        assert!(
            worktree_path.exists(),
            "note file should be created in worktree .djinn"
        );
        assert!(
            !canonical_path.exists(),
            "canonical .djinn path should remain untouched during worktree session writes"
        );

        let edited = mcp_call_tool_with_headers(
            &app,
            &session_id,
            "memory_edit",
            json!({"project": project, "identifier": created["permalink"], "operation": "append", "content": "beta"}),
            &[("x-djinn-worktree-root", &worktree_header)],
        )
        .await;
        assert!(edited["content"].as_str().unwrap().contains("beta"));
        let worktree_contents =
            std::fs::read_to_string(&worktree_path).expect("read worktree note");
        assert!(worktree_contents.contains("beta"));

        let deleted = mcp_call_tool_with_headers(
            &app,
            &session_id,
            "memory_delete",
            json!({"project": project, "identifier": created["permalink"]}),
            &[("x-djinn-worktree-root", &worktree_header)],
        )
        .await;
        assert_eq!(deleted["ok"], true);
        assert!(
            !worktree_path.exists(),
            "delete should remove worktree note file"
        );
    }

    #[tokio::test]
    async fn mcp_memory_list_all_and_filters_by_folder_and_type() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({"project": project, "title": "A", "content": "x", "type": "adr"}),
        )
        .await;
        mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({"project": project, "title": "B", "content": "x", "type": "reference"}),
        )
        .await;

        let all = mcp_call_tool(
            &app,
            &session_id,
            "memory_list",
            json!({"project": project}),
        )
        .await;
        assert!(all["notes"].as_array().unwrap().len() >= 2);

        let folder = mcp_call_tool(
            &app,
            &session_id,
            "memory_list",
            json!({"project": project, "folder": "decisions"}),
        )
        .await;
        for n in folder["notes"].as_array().unwrap() {
            assert_eq!(n["folder"], "decisions");
        }

        let typed = mcp_call_tool(
            &app,
            &session_id,
            "memory_list",
            json!({"project": project, "type": "reference"}),
        )
        .await;
        for n in typed["notes"].as_array().unwrap() {
            assert_eq!(n["note_type"], "reference");
        }
    }

    #[tokio::test]
    async fn mcp_memory_graph_returns_wikilink_edges() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({"project": project, "title": "Node B", "content": "b", "type": "reference"}),
        )
        .await;
        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Node A", "content": "links [[Node B]]", "type": "reference"})).await;

        let graph = mcp_call_tool(
            &app,
            &session_id,
            "memory_graph",
            json!({"project": project}),
        )
        .await;
        assert!(!graph["edges"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mcp_memory_recent_orders_by_last_accessed() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({"project": project, "title": "Older", "content": "o", "type": "reference"}),
        )
        .await;
        mcp_call_tool(
            &app,
            &session_id,
            "memory_write",
            json!({"project": project, "title": "Newer", "content": "n", "type": "reference"}),
        )
        .await;
        mcp_call_tool(
            &app,
            &session_id,
            "memory_read",
            json!({"project": project, "identifier": "Older"}),
        )
        .await;
        mcp_call_tool(
            &app,
            &session_id,
            "memory_read",
            json!({"project": project, "identifier": "Newer"}),
        )
        .await;

        let recent = mcp_call_tool(
            &app,
            &session_id,
            "memory_recent",
            json!({"project": project, "timeframe": "7d", "limit": 2}),
        )
        .await;
        assert_eq!(recent["notes"].as_array().unwrap()[0]["title"], "Newer");
    }

    #[tokio::test]
    async fn mcp_memory_catalog_returns_structured_catalog() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Catalog Item", "content": "c", "type": "reference"})).await;
        let catalog = mcp_call_tool(
            &app,
            &session_id,
            "memory_catalog",
            json!({"project": project}),
        )
        .await;
        assert!(
            catalog["catalog"]
                .as_str()
                .unwrap()
                .contains("Catalog Item")
        );
    }

    #[tokio::test]
    async fn mcp_memory_health_orphans_and_broken_links_shapes() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let project = "/tmp/mcp-memory-health";

        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Source", "content": "[[Missing Target]]", "type": "reference"})).await;

        let health = mcp_call_tool(
            &app,
            &session_id,
            "memory_health",
            json!({"project": project}),
        )
        .await;
        assert!(health.get("orphan_note_count").is_some());
        assert!(health.get("broken_link_count").is_some());

        let orphans = mcp_call_tool(
            &app,
            &session_id,
            "memory_orphans",
            json!({"project": project}),
        )
        .await;
        assert!(orphans["orphans"].is_array());

        let broken = mcp_call_tool(
            &app,
            &session_id,
            "memory_broken_links",
            json!({"project": project}),
        )
        .await;
        assert!(broken["broken_links"].is_array());
    }

    #[tokio::test]
    async fn mcp_memory_history_and_diff_round_trip() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let created = mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "History Diff", "content": "line one", "type": "reference"})).await;
        let permalink = created["permalink"].as_str().unwrap().to_string();

        let edited = mcp_call_tool(&app, &session_id, "memory_edit", json!({"project": project, "identifier": permalink, "operation": "append", "content": "line two"})).await;
        assert!(edited.get("error").is_none() || edited["error"].is_null());

        let history = mcp_call_tool(
            &app,
            &session_id,
            "memory_history",
            json!({"project": project, "permalink": created["permalink"], "limit": 10}),
        )
        .await;
        assert!(history.get("error").is_none() || history["error"].is_null());

        let entries = history["history"]
            .as_array()
            .or_else(|| history["entries"].as_array())
            .expect("memory_history should return history/entries array");

        if entries.is_empty() {
            let diff = mcp_call_tool(
                &app,
                &session_id,
                "memory_diff",
                json!({"project": project, "permalink": created["permalink"]}),
            )
            .await;
            assert!(diff.get("error").is_none() || diff["error"].is_null());
            let d = diff["diff"].as_str().unwrap();
            assert!(d.contains("@@") || d.contains("diff --git") || d.is_empty());
            return;
        }

        let latest_sha = entries
            .first()
            .and_then(|e| e["sha"].as_str())
            .unwrap()
            .to_string();
        let diff = mcp_call_tool(
            &app,
            &session_id,
            "memory_diff",
            json!({"project": project, "permalink": created["permalink"], "sha": latest_sha}),
        )
        .await;
        assert!(diff.get("error").is_none() || diff["error"].is_null());
        let d = diff["diff"].as_str().unwrap();
        assert!(d.contains("@@") || d.contains("diff --git") || !d.is_empty());
    }

    #[tokio::test]
    async fn mcp_memory_reindex_returns_expected_contract_shape() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Reindex Seed", "content": "seed", "type": "reference"})).await;

        let reindex = mcp_call_tool(
            &app,
            &session_id,
            "memory_reindex",
            json!({"project": project}),
        )
        .await;
        assert!(reindex.get("error").is_none() || reindex["error"].is_null());
        assert!(reindex.get("updated").and_then(|v| v.as_i64()).is_some());
        assert!(reindex.get("created").and_then(|v| v.as_i64()).is_some());
        assert!(reindex.get("deleted").and_then(|v| v.as_i64()).is_some());
        assert!(reindex.get("unchanged").and_then(|v| v.as_i64()).is_some());
    }

    #[tokio::test]
    async fn mcp_memory_build_context_follows_wikilinks() {
        let db = create_test_db();
        let (proj, _dir) = create_test_project_with_dir(&db).await;
        let project = &proj.path;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let target = mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Context Target", "content": "target body", "type": "reference"})).await;
        let seed = mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Context Seed", "content": "see [[Context Target]]", "type": "reference"})).await;

        let built = mcp_call_tool(
            &app,
            &session_id,
            "memory_build_context",
            json!({"project": project, "url": seed["permalink"], "depth": 1, "max_related": 5}),
        )
        .await;
        assert!(built.get("error").is_none() || built["error"].is_null());
        let primary = built["primary"].as_array().unwrap();
        let related_l1 = built["related_l1"].as_array().unwrap();
        let related_l0 = built["related_l0"].as_array().unwrap();
        assert_eq!(primary[0]["permalink"], seed["permalink"]);
        // Check both L1 and L0 tiered fields for the target note
        let in_l1 = related_l1
            .iter()
            .any(|n| n["permalink"] == target["permalink"]);
        let in_l0 = related_l0
            .iter()
            .any(|n| n["permalink"] == target["permalink"]);
        assert!(
            in_l1 || in_l0,
            "target permalink should be in related_l1 or related_l0"
        );
    }

    #[tokio::test]
    async fn mcp_memory_task_refs_returns_tasks_for_permalink() {
        let db = create_test_db();
        let project_row = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project_row.id).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;
        let project = project_row.path.clone();

        let note = mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Task Ref Note", "content": "task refs seed", "type": "reference"})).await;

        let task = mcp_call_tool(&app, &session_id, "task_create", json!({"project": project, "epic_id": epic.id, "title": "Task referencing memory note", "issue_type": "task", "priority": 2, "status": "open", "memory_refs": [note["permalink"]]})).await;
        assert!(task.get("error").is_none() || task["error"].is_null());

        let refs = mcp_call_tool(
            &app,
            &session_id,
            "memory_task_refs",
            json!({"project": project, "permalink": note["permalink"]}),
        )
        .await;
        assert!(refs.get("error").is_none() || refs["error"].is_null());
        let tasks = refs["tasks"].as_array().unwrap();
        assert!(
            tasks
                .iter()
                .any(|t| t["id"] == task["id"] && t["title"] == "Task referencing memory note")
        );
    }

    // ── Param deserialization ─────────────────────────────────────────────────

    #[test]
    fn write_params_deserialize() {
        let p: WriteParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","title":"T","content":"C","type":"adr"}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.title, "T");
        assert_eq!(p.content, "C");
        assert_eq!(p.note_type, "adr");
        assert!(p.tags.is_none());
    }

    #[test]
    fn read_params_deserialize() {
        let p: ReadParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p","identifier":"abc"}))
                .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.identifier, "abc");
    }

    #[test]
    fn search_params_deserialize() {
        let p: SearchParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p","query":"rust"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.query, "rust");
        assert!(p.limit.is_none());
        assert!(p.folder.is_none());
        assert!(p.note_type.is_none());
    }

    #[test]
    fn edit_params_deserialize() {
        let p: EditParams = serde_json::from_value(serde_json::json!({"project":"/tmp/p","identifier":"a","operation":"append","content":"x"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.identifier, "a");
        assert_eq!(p.operation, "append");
        assert_eq!(p.content, "x");
    }

    #[test]
    fn move_params_deserialize() {
        let p: MoveParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","identifier":"a","type":"adr","title":"new"}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.identifier, "a");
        assert_eq!(p.title.as_deref(), Some("new"));
        assert_eq!(p.note_type, "adr");
    }

    #[test]
    fn delete_params_deserialize() {
        let p: DeleteParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p","identifier":"a"}))
                .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.identifier, "a");
    }

    #[test]
    fn list_params_deserialize() {
        let p: ListParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","folder":"decisions","type":"adr","depth":2}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.folder.as_deref(), Some("decisions"));
        assert_eq!(p.note_type.as_deref(), Some("adr"));
        assert_eq!(p.depth, Some(2));
    }

    #[test]
    fn list_params_deserialize_minimal() {
        let p: ListParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert!(p.folder.is_none());
        assert!(p.note_type.is_none());
        assert!(p.depth.is_none());
    }

    #[test]
    fn graph_params_deserialize() {
        let p: GraphParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
    }

    #[test]
    fn recent_params_deserialize() {
        let p: RecentParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","timeframe":"7d","limit":5}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.timeframe.as_deref(), Some("7d"));
        assert_eq!(p.limit, Some(5));
    }

    #[test]
    fn catalog_params_deserialize() {
        let p: CatalogParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
    }

    #[test]
    fn health_params_deserialize() {
        let p: HealthParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project.as_deref(), Some("/tmp/p"));
    }

    #[test]
    fn orphans_params_deserialize() {
        let p: OrphansParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
    }

    #[test]
    fn broken_links_params_deserialize() {
        let p: BrokenLinksParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
    }

    #[test]
    fn history_params_deserialize() {
        let p: HistoryParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","permalink":"decisions/a","limit":10}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.permalink, "decisions/a");
        assert_eq!(p.limit, Some(10));
    }

    #[test]
    fn diff_params_deserialize() {
        let p: DiffParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","permalink":"decisions/a","sha":"abc"}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.permalink, "decisions/a");
        assert_eq!(p.sha.as_deref(), Some("abc"));
    }

    #[test]
    fn build_context_params_deserialize() {
        let p: BuildContextParams = serde_json::from_value(serde_json::json!({"project":"/tmp/p","url":"memory://references/note","depth":2,"max_related":3})).unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.url, "memory://references/note");
        assert_eq!(p.depth, Some(2));
        assert_eq!(p.max_related, Some(3));
    }

    #[test]
    fn reindex_params_deserialize() {
        let p: ReindexParams =
            serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
        assert_eq!(p.project, "/tmp/p");
    }

    #[test]
    fn task_refs_params_deserialize() {
        let p: TaskRefsParams = serde_json::from_value(
            serde_json::json!({"project":"/tmp/p","permalink":"references/n"}),
        )
        .unwrap();
        assert_eq!(p.project, "/tmp/p");
        assert_eq!(p.permalink, "references/n");
    }
}

// ── project_tools ─────────────────────────────────────────────────────────────

mod project_tools {
    use serde_json::json;
    use tempfile::tempdir;

    use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

    #[tokio::test]
    async fn project_add_and_list_success_shape() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        let added = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-a", "path": path.clone()}),
        )
        .await;
        assert_eq!(added["status"], "ok");
        assert!(added["project"]["id"].as_str().unwrap_or_default().len() > 8);
        assert_eq!(added["project"]["path"], json!(path));

        let listed = mcp_call_tool(&app, &session_id, "project_list", json!({})).await;
        assert!(
            listed["projects"]
                .as_array()
                .expect("projects array")
                .iter()
                .any(|p| p["path"] == json!(path))
        );
    }

    #[tokio::test]
    async fn project_add_duplicate_path_errors() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-a", "path": path.clone()}),
        )
        .await;
        let dup = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-b", "path": path}),
        )
        .await;
        assert!(
            dup["status"]
                .as_str()
                .unwrap_or_default()
                .starts_with("error:")
        );
    }

    #[tokio::test]
    async fn project_remove_success_and_missing() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-remove", "path": path.clone()}),
        )
        .await;

        let removed = mcp_call_tool(
            &app,
            &session_id,
            "project_remove",
            json!({"name": "proj-remove", "path": path.clone()}),
        )
        .await;
        assert_eq!(removed["status"], "ok");

        let missing = mcp_call_tool(
            &app,
            &session_id,
            "project_remove",
            json!({"name": "proj-remove", "path": path}),
        )
        .await;
        assert!(
            missing["status"]
                .as_str()
                .unwrap_or_default()
                .starts_with("error:")
        );
    }

    #[tokio::test]
    async fn project_remove_wrong_path_is_rejected() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-guard", "path": path.clone()}),
        )
        .await;

        let rejected = mcp_call_tool(
            &app,
            &session_id,
            "project_remove",
            json!({"name": "proj-guard", "path": "/wrong/path"}),
        )
        .await;
        assert!(
            rejected["status"]
                .as_str()
                .unwrap_or_default()
                .starts_with("error:")
        );

        let listed = mcp_call_tool(&app, &session_id, "project_list", json!({})).await;
        assert!(
            listed["projects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "proj-guard")
        );
    }

    #[tokio::test]
    async fn project_config_get_set_round_trip() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-config", "path": path.clone()}),
        )
        .await;

        let set = mcp_call_tool(
            &app,
            &session_id,
            "project_config_set",
            json!({"project": path.clone(), "key": "target_branch", "value": "develop"}),
        )
        .await;
        assert_eq!(set["status"], "ok");

        let got = mcp_call_tool(
            &app,
            &session_id,
            "project_config_get",
            json!({"project": path}),
        )
        .await;
        assert_eq!(got["status"], "ok");
        assert_eq!(got["target_branch"], "develop");
    }

    #[tokio::test]
    async fn project_settings_validate_reports_valid_and_invalid() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let djinn = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn).expect("create .djinn");

        std::fs::write(djinn.join("settings.json"), r#"{"setup":[{"name":"setup","command":"echo ok"}],"verification":[{"name":"verify","command":"echo ok"}],"extra":true}"#).expect("write settings");
        let valid = mcp_call_tool(
            &app,
            &session_id,
            "project_settings_validate",
            json!({"worktree_path": dir.path().to_string_lossy().to_string()}),
        )
        .await;
        assert_eq!(valid["valid"], true);
        assert!(valid["errors"].as_array().expect("errors").iter().any(|e| {
            e.as_str()
                .unwrap_or_default()
                .contains("warning: unknown top-level key 'extra'")
        }));

        std::fs::write(
            djinn.join("settings.json"),
            r#"{"setup":[{"name":"missing-command"}]}"#,
        )
        .expect("write invalid settings");
        let invalid = mcp_call_tool(
            &app,
            &session_id,
            "project_settings_validate",
            json!({"worktree_path": dir.path().to_string_lossy().to_string()}),
        )
        .await;
        assert_eq!(invalid["valid"], false);
        assert!(
            invalid["errors"]
                .as_array()
                .expect("errors")
                .iter()
                .any(|e| e
                    .as_str()
                    .unwrap_or_default()
                    .contains("schema validation failed"))
        );
    }
}

// ── settings_tools ────────────────────────────────────────────────────────────

mod settings_tools {
    use serde_json::json;

    use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

    #[tokio::test]
    async fn settings_get_missing_returns_not_exists() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let res = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
        assert_eq!(res["exists"], false);
        assert!(res["settings"].is_null());
    }

    #[tokio::test]
    async fn settings_set_get_reset_round_trip() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        // Only set dispatch_limit — model_priority requires connected credentials.
        let set = mcp_call_tool(
            &app,
            &session_id,
            "settings_set",
            json!({"dispatch_limit": 7}),
        )
        .await;
        assert_eq!(set["ok"], true);

        let get = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
        assert_eq!(get["exists"], true);
        assert_eq!(get["settings"]["dispatch_limit"], 7);

        let reset = mcp_call_tool(&app, &session_id, "settings_reset", json!({})).await;
        assert_eq!(reset["ok"], true);

        let get2 = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
        assert_eq!(get2["exists"], false);
    }

    #[tokio::test]
    async fn settings_set_rejects_unconnected_model_priority_provider() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        // Validation rejects model_priority referencing providers with no credentials.
        let res = mcp_call_tool(
            &app,
            &session_id,
            "settings_set",
            json!({"model_priority_worker": ["no-such-provider/some-model"]}),
        )
        .await;
        assert_eq!(res["ok"], false);
        assert!(
            res["error"]
                .as_str()
                .unwrap_or_default()
                .contains("disconnected")
        );
    }
}

// ── system_tools ──────────────────────────────────────────────────────────────

mod system_tools {
    use serde_json::json;

    use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

    #[tokio::test]
    async fn system_ping_returns_version() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let res = mcp_call_tool(&app, &session_id, "system_ping", json!({})).await;
        assert_eq!(res["status"], "ok");
        assert_eq!(res["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn system_logs_returns_lines_or_empty() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let res = mcp_call_tool(&app, &session_id, "system_logs", json!({"lines": 10})).await;
        assert!(res.get("lines").and_then(|v| v.as_array()).is_some());
    }
}

// ── task_tools ────────────────────────────────────────────────────────────────

mod task_tools {
    use serde_json::json;

    use crate::events::EventBus;
    use crate::test_helpers::{
        create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
        create_test_task, initialize_mcp_session, mcp_call_tool,
    };
    use djinn_db::TaskRepository;

    #[tokio::test]
    async fn task_create_success_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(&app, &sid, "task_create", json!({"project": project.path, "epic_id": epic.id, "title": "Create task contract test"})).await;
        assert!(payload["id"].as_str().is_some());
        assert!(payload["short_id"].as_str().is_some());
        assert_eq!(payload["status"], "backlog");
        assert_eq!(payload["title"], "Create task contract test");
        assert_eq!(payload["epic_id"], epic.id);

        let repo = TaskRepository::new(db.clone(), EventBus::noop());
        let created = repo
            .get(payload["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.title, "Create task contract test");
        assert_eq!(created.status, "backlog");
        assert_eq!(created.epic_id, Some(epic.id));
    }

    #[tokio::test]
    async fn task_create_error_validation() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &sid,
            "task_create",
            json!({"project": "any/project", "title": ""}),
        )
        .await;
        assert!(payload["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn task_show_found_and_not_found_shapes() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let ok = mcp_call_tool(
            &app,
            &sid,
            "task_show",
            json!({"project": project.path, "id": task.id}),
        )
        .await;
        assert!(ok["id"].as_str().is_some());
        assert!(ok["title"].as_str().is_some());

        let err = mcp_call_tool(
            &app,
            &sid,
            "task_show",
            json!({"project": project.path, "id": "missing-task-id"}),
        )
        .await;
        assert!(err["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn task_list_filters_and_pagination() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic1 = create_test_epic(&db, &project.id).await;
        let epic2 = create_test_epic(&db, &project.id).await;
        let repo = TaskRepository::new(db.clone(), EventBus::noop());

        let t1 = repo
            .create_in_project(
                &project.id,
                Some(&epic1.id),
                "alpha ready",
                "desc",
                "design",
                "task",
                1,
                "owner",
                None,
            )
            .await
            .unwrap();
        let _t2 = repo
            .create_in_project(
                &project.id,
                Some(&epic1.id),
                "beta progress",
                "desc",
                "design",
                "task",
                2,
                "owner",
                None,
            )
            .await
            .unwrap();
        let _t3 = repo
            .create_in_project(
                &project.id,
                Some(&epic2.id),
                "gamma text",
                "desc",
                "design",
                "task",
                3,
                "owner",
                None,
            )
            .await
            .unwrap();
        repo.transition(
            &t1.id,
            djinn_core::models::TransitionAction::Accept,
            "a",
            "user",
            None,
            None,
        )
        .await
        .unwrap();
        repo.update(
            &t1.id,
            "alpha ready",
            "desc",
            "design",
            1,
            "owner",
            "",
            r#"[{"description":"default","met":false}]"#,
        )
        .await
        .unwrap();
        repo.transition(
            &t1.id,
            djinn_core::models::TransitionAction::Start,
            "a",
            "user",
            None,
            None,
        )
        .await
        .unwrap();

        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let by_status = mcp_call_tool(
            &app,
            &sid,
            "task_list",
            json!({"project": project.path, "status": "in_progress"}),
        )
        .await;
        assert!(!by_status["tasks"].as_array().unwrap().is_empty());

        let by_text = mcp_call_tool(
            &app,
            &sid,
            "task_list",
            json!({"project": project.path, "text": "gamma"}),
        )
        .await;
        assert_eq!(by_text["tasks"].as_array().unwrap().len(), 1);

        let paged = mcp_call_tool(
            &app,
            &sid,
            "task_list",
            json!({"project": project.path, "limit": 1, "offset": 0}),
        )
        .await;
        assert_eq!(paged["limit"], 1);
        assert_eq!(paged["offset"], 0);
    }

    #[tokio::test]
    async fn task_update_partial_and_error_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let ok = mcp_call_tool(
            &app,
            &sid,
            "task_update",
            json!({"project": project.path, "id": task.id, "title": "updated"}),
        )
        .await;
        assert_eq!(ok["title"], "updated");

        let repo = TaskRepository::new(db.clone(), EventBus::noop());
        assert_eq!(repo.get(&task.id).await.unwrap().unwrap().title, "updated");

        let err = mcp_call_tool(
            &app,
            &sid,
            "task_update",
            json!({"project": project.path, "id": "missing-id", "title": "x"}),
        )
        .await;
        assert!(err["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn task_transition_valid_and_invalid() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let ok = mcp_call_tool(&app, &sid, "task_transition", json!({"project": project.path, "id": task.id, "action": "accept", "actor_id": "u1", "actor_role": "user"})).await;
        assert_eq!(ok["status"], "open");

        let repo = TaskRepository::new(db.clone(), EventBus::noop());
        assert_eq!(repo.get(&task.id).await.unwrap().unwrap().status, "open");

        let bad = mcp_call_tool(&app, &sid, "task_transition", json!({"project": project.path, "id": task.id, "action": "not_real", "actor_id": "u1", "actor_role": "user"})).await;
        assert!(bad["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn task_count_plain_and_grouped() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let t1 = create_test_task(&db, &project.id, &epic.id).await;
        let repo = TaskRepository::new(db.clone(), EventBus::noop());
        repo.transition(
            &t1.id,
            djinn_core::models::TransitionAction::Accept,
            "u1",
            "user",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &t1.id,
            djinn_core::models::TransitionAction::Start,
            "u1",
            "user",
            None,
            None,
        )
        .await
        .unwrap();
        let _t2 = create_test_task(&db, &project.id, &epic.id).await;

        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let plain = mcp_call_tool(&app, &sid, "task_count", json!({"project": project.path})).await;
        assert!(plain["total_count"].as_i64().unwrap() >= 2);

        let grouped = mcp_call_tool(
            &app,
            &sid,
            "task_count",
            json!({"project": project.path, "group_by": "status"}),
        )
        .await;
        assert!(grouped["groups"].as_array().is_some());
    }

    #[tokio::test]
    async fn task_claim_ready_and_empty() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let _task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let claimed =
            mcp_call_tool(&app, &sid, "task_claim", json!({"project": project.path})).await;
        assert!(claimed["id"].as_str().is_some() || claimed["task"].is_null());

        let db2 = create_test_db();
        let project2 = create_test_project(&db2).await;
        let app2 = create_test_app_with_db(db2);
        let sid2 = initialize_mcp_session(&app2).await;
        let empty = mcp_call_tool(
            &app2,
            &sid2,
            "task_claim",
            json!({"project": project2.path}),
        )
        .await;
        assert!(empty["task"].is_null());
    }

    #[tokio::test]
    async fn task_ready_lists_open_unblocked() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let _ = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let payload =
            mcp_call_tool(&app, &sid, "task_ready", json!({"project": project.path})).await;
        assert!(payload["tasks"].as_array().is_some());
    }

    #[tokio::test]
    async fn task_comment_activity_blockers_blocked_memory_refs_shapes() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let blocker = create_test_task(&db, &project.id, &epic.id).await;
        let blocked = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db.clone());
        let sid = initialize_mcp_session(&app).await;

        let updated = mcp_call_tool(&app, &sid, "task_update", json!({"project": project.path, "id": blocked.id, "blocked_by_add": [blocker.id], "memory_refs_add": ["notes/a"]})).await;
        assert!(updated["id"].as_str().is_some());

        let c = mcp_call_tool(&app, &sid, "task_comment_add", json!({"project": project.path, "id": blocked.id, "actor_id": "u1", "actor_role": "user", "body": "hello"})).await;
        assert_eq!(c["event_type"], "comment");

        let c_err = mcp_call_tool(&app, &sid, "task_comment_add", json!({"project": project.path, "id": "missing", "actor_id": "u1", "actor_role": "user", "body": "hello"})).await;
        assert!(c_err["error"].as_str().is_some());

        let activity = mcp_call_tool(
            &app,
            &sid,
            "task_activity_list",
            json!({"project": project.path, "id": blocked.id}),
        )
        .await;
        assert!(activity["entries"].as_array().is_some());

        let blockers = mcp_call_tool(
            &app,
            &sid,
            "task_blockers_list",
            json!({"project": project.path, "id": blocked.id}),
        )
        .await;
        assert!(blockers["blockers"].as_array().is_some());

        let blocked_list = mcp_call_tool(
            &app,
            &sid,
            "task_blocked_list",
            json!({"project": project.path, "id": blocker.id}),
        )
        .await;
        assert!(blocked_list["tasks"].as_array().is_some());

        let refs = mcp_call_tool(
            &app,
            &sid,
            "task_memory_refs",
            json!({"project": project.path, "id": blocked.id}),
        )
        .await;
        assert!(refs["memory_refs"].as_array().is_some());
    }
}

// ── session_tools ─────────────────────────────────────────────────────────────

mod session_tools {
    use serde_json::json;

    use crate::test_helpers::{
        create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
        create_test_session, create_test_task, initialize_mcp_session, mcp_call_tool,
    };
    use djinn_db::SessionMessageRepository;

    #[tokio::test]
    async fn session_list_returns_empty_for_task_without_sessions() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &session_id,
            "session_list",
            json!({ "task_id": task.id, "project": project.path }),
        )
        .await;
        assert_eq!(payload.get("error"), None);
        assert_eq!(
            payload.get("task_id").and_then(|v| v.as_str()),
            Some(task.id.as_str())
        );
        assert!(
            payload
                .get("sessions")
                .and_then(|v| v.as_array())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn session_list_filters_by_project_and_task() {
        let db = create_test_db();
        let project_a = create_test_project(&db).await;
        let epic_a = create_test_epic(&db, &project_a.id).await;
        let task_a1 = create_test_task(&db, &project_a.id, &epic_a.id).await;
        let task_a2 = create_test_task(&db, &project_a.id, &epic_a.id).await;
        let project_b = create_test_project(&db).await;
        let epic_b = create_test_epic(&db, &project_b.id).await;
        let task_b1 = create_test_task(&db, &project_b.id, &epic_b.id).await;
        let _s_a1_1 = create_test_session(&db, &project_a.id, &task_a1.id).await;
        let _s_a1_2 = create_test_session(&db, &project_a.id, &task_a1.id).await;
        let _s_a2 = create_test_session(&db, &project_a.id, &task_a2.id).await;
        let _s_b1 = create_test_session(&db, &project_b.id, &task_b1.id).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &session_id,
            "session_list",
            json!({ "task_id": task_a1.id, "project": project_a.path }),
        )
        .await;
        assert_eq!(payload.get("error"), None);
        let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .all(|s| s.get("task_id").and_then(|v| v.as_str()) == Some(task_a1.id.as_str()))
        );
        assert!(
            sessions.iter().all(
                |s| s.get("project_id").and_then(|v| v.as_str()) == Some(project_a.id.as_str())
            )
        );
    }

    #[tokio::test]
    async fn session_show_returns_full_shape_with_tokens() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let session = create_test_session(&db, &project.id, &task.id).await;
        let app = create_test_app_with_db(db);
        let mcp_session = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &mcp_session,
            "session_show",
            json!({ "id": session.id, "project": project.path }),
        )
        .await;
        assert_eq!(payload.get("error"), None);
        for key in [
            "id",
            "task_id",
            "model_id",
            "agent_type",
            "status",
            "tokens_in",
            "tokens_out",
        ] {
            assert!(payload.get(key).is_some(), "missing key {key}");
        }
    }

    #[tokio::test]
    async fn session_show_not_found_returns_error_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &session_id,
            "session_show",
            json!({ "id": "missing-session-id", "project": project.path }),
        )
        .await;
        assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
        assert_eq!(payload.get("id"), None);
    }

    #[tokio::test]
    async fn session_active_returns_error_without_pool() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &session_id,
            "session_active",
            json!({ "project": project.path }),
        )
        .await;
        assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn session_for_task_returns_error_without_pool() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let _session = create_test_session(&db, &project.id, &task.id).await;
        let app = create_test_app_with_db(db);
        let mcp_session = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &mcp_session,
            "session_for_task",
            json!({ "task_id": task.id, "project": project.path }),
        )
        .await;
        assert!(result.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn task_timeline_returns_chronological_session_and_message_history() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let s1 = create_test_session(&db, &project.id, &task.id).await;
        let s2 = create_test_session(&db, &project.id, &task.id).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), crate::events::EventBus::noop());
        msg_repo
            .insert_message(
                &s1.id,
                &task.id,
                "user",
                &serde_json::json!([{"type":"text","text":"first"}]).to_string(),
                None,
            )
            .await
            .unwrap();
        msg_repo
            .insert_message(
                &s2.id,
                &task.id,
                "assistant",
                &serde_json::json!([{"type":"text","text":"second"}]).to_string(),
                None,
            )
            .await
            .unwrap();

        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &session_id,
            "task_timeline",
            json!({ "task_id": task.id, "project": project.path }),
        )
        .await;
        assert_eq!(payload.get("error"), None);
        let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
        let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(messages.len(), 2);
        let ts0 = messages[0]
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap();
        let ts1 = messages[1]
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(ts0 <= ts1);
    }

    #[tokio::test]
    async fn task_timeline_not_found_returns_error_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let payload = mcp_call_tool(
            &app,
            &session_id,
            "task_timeline",
            json!({ "task_id": "missing-task", "project": project.path }),
        )
        .await;
        assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
        assert!(payload.get("sessions").is_none());
        assert!(payload.get("messages").is_none());
    }

    #[tokio::test]
    async fn session_messages_returns_messages_for_valid_session_id() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let sess = create_test_session(&db, &project.id, &task.id).await;

        let msg_repo = SessionMessageRepository::new(db.clone(), crate::events::EventBus::noop());
        msg_repo
            .insert_message(
                &sess.id,
                &task.id,
                "user",
                &serde_json::json!([{"type":"text","text":"hello"}]).to_string(),
                None,
            )
            .await
            .unwrap();

        let app = create_test_app_with_db(db);
        let mcp_session = initialize_mcp_session(&app).await;
        let payload = mcp_call_tool(
            &app,
            &mcp_session,
            "session_messages",
            json!({ "id": sess.id, "project": project.path }),
        )
        .await;
        assert_eq!(payload.get("error"), None);
        let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].get("role").and_then(|v| v.as_str()),
            Some("user")
        );
    }
}

// ── provider_tools ────────────────────────────────────────────────────────────

mod provider_tools {
    use crate::test_helpers::{
        create_test_app_with_db, create_test_db, initialize_mcp_session, mcp_call_tool,
    };
    use djinn_provider::repos::CredentialRepository;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_catalog_returns_expected_shape() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let result =
            mcp_call_tool(&app, &session_id, "provider_catalog", serde_json::json!({})).await;
        let providers = result["providers"].as_array().expect("providers array");
        assert!(!providers.is_empty());
        assert!(providers[0].get("id").is_some());
        assert!(providers[0].get("name").is_some());
        assert!(result.get("total").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_models_returns_models_for_valid_provider_and_error_for_unknown() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let valid = mcp_call_tool(
            &app,
            &session_id,
            "provider_models",
            serde_json::json!({"provider_id":"openai"}),
        )
        .await;
        assert_eq!(valid["provider_id"], "openai");
        assert!(
            valid["models"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        );

        let unknown = mcp_call_tool(
            &app,
            &session_id,
            "provider_models",
            serde_json::json!({"provider_id":"no-such-provider"}),
        )
        .await;
        assert_eq!(unknown["total"], 0);
        assert!(unknown["models"].as_array().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_connected_returns_only_seeded_provider() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let session_id = initialize_mcp_session(&app).await;

        CredentialRepository::new(db, crate::events::EventBus::noop())
            .set("openai", "OPENAI_API_KEY", "sk-test")
            .await
            .unwrap();

        let result = mcp_call_tool(
            &app,
            &session_id,
            "provider_connected",
            serde_json::json!({}),
        )
        .await;
        let providers = result["providers"].as_array().expect("providers array");
        assert!(!providers.is_empty());
        assert!(
            providers
                .iter()
                .all(|p| p["connected"].as_bool().unwrap_or(false))
        );
        assert!(providers.iter().any(|p| p["id"] == "openai"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_models_connected_filters_to_connected_provider_models() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let session_id = initialize_mcp_session(&app).await;

        CredentialRepository::new(db, crate::events::EventBus::noop())
            .set("openai", "OPENAI_API_KEY", "sk-test")
            .await
            .unwrap();

        let result = mcp_call_tool(
            &app,
            &session_id,
            "provider_models_connected",
            serde_json::json!({}),
        )
        .await;
        let models = result["models"].as_array().expect("models array");
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m["provider_id"] == "openai"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_model_lookup_returns_found_and_not_found_shapes() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let found = mcp_call_tool(
            &app,
            &session_id,
            "provider_model_lookup",
            serde_json::json!({"model_id":"openai/gpt-4o-mini"}),
        )
        .await;
        assert!(found["found"].as_bool().unwrap_or(false));
        assert!(found.get("model").is_some());

        let not_found = mcp_call_tool(
            &app,
            &session_id,
            "provider_model_lookup",
            serde_json::json!({"model_id":"nope/unknown-model"}),
        )
        .await;
        assert!(!not_found["found"].as_bool().unwrap_or(true));
        assert!(not_found["model"].is_null());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_health_status_and_param_validation_shapes() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let status = mcp_call_tool(
            &app,
            &session_id,
            "model_health",
            serde_json::json!({"action":"status"}),
        )
        .await;
        assert_eq!(status["action"], "status");
        assert!(status["models"].is_array());

        let reset_err = mcp_call_tool(
            &app,
            &session_id,
            "model_health",
            serde_json::json!({"action":"reset"}),
        )
        .await;
        assert!(reset_err["error"].as_str().is_some());

        let enable_err = mcp_call_tool(
            &app,
            &session_id,
            "model_health",
            serde_json::json!({"action":"enable"}),
        )
        .await;
        assert!(enable_err["error"].as_str().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_add_custom_and_remove_custom_work() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let added = mcp_call_tool(&app, &session_id, "provider_add_custom", serde_json::json!({"id":"my-custom","name":"My Custom","base_url":"https://example.invalid/v1","env_var":"MY_CUSTOM_API_KEY","seed_models":[{"id":"my-model","name":"My Model"}]})).await;
        assert!(added["ok"].as_bool().unwrap_or(false));
        assert_eq!(added["id"], "my-custom");

        let removed = mcp_call_tool(
            &app,
            &session_id,
            "provider_remove",
            serde_json::json!({"provider_id":"my-custom"}),
        )
        .await;
        assert!(removed["ok"].as_bool().unwrap_or(false));
        assert!(
            removed["custom_provider_deleted"]
                .as_bool()
                .unwrap_or(false)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_remove_builtin_returns_error_shape() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let removed = mcp_call_tool(
            &app,
            &session_id,
            "provider_remove",
            serde_json::json!({"provider_id":"openai"}),
        )
        .await;
        assert!(removed.get("error").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_validate_returns_error_shape_without_real_key() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(&app, &session_id, "provider_validate", serde_json::json!({"provider_id":"openai","base_url":"https://api.openai.com/v1","api_key":"sk-invalid"})).await;
        assert!(result.get("ok").is_some());
        assert!(result.get("error_kind").is_some());
        assert!(result.get("error").is_some());
        assert!(result.get("http_status").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_oauth_start_returns_error_shape_when_not_configured_or_invalid() {
        let db = create_test_db();
        let app = create_test_app_with_db(db);
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "provider_oauth_start",
            serde_json::json!({"provider_id":"no-such-provider"}),
        )
        .await;
        assert!(!result["ok"].as_bool().unwrap_or(true));
        assert!(result["error"].as_str().is_some());
        assert!(result.get("oauth_supported").is_some());
    }
}
