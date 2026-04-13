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
async fn mcp_memory_write_and_move_accept_case_and_pitfall_types() {
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
            "title": "Recovered Incident",
            "content": "body",
            "type": "case"
        }),
    )
    .await;

    assert_eq!(created["note_type"], "case");
    assert_eq!(created["folder"], "cases");
    assert_eq!(created["permalink"], "cases/recovered-incident");

    let moved = mcp_call_tool(
        &app,
        &session_id,
        "memory_move",
        json!({
            "project": project,
            "identifier": created["permalink"],
            "type": "pitfall"
        }),
    )
    .await;

    assert_eq!(moved["note_type"], "pitfall");
    assert_eq!(moved["folder"], "pitfalls");
    assert_eq!(moved["permalink"], "pitfalls/recovered-incident");
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
async fn mcp_memory_move_can_recover_proposed_adr_and_make_it_visible_to_proposal_list() {
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
            "title": "Recover Me",
            "content": "---\nwork_shape: epic\n---\n\n# Recover Me\n",
            "type": "adr"
        }),
    )
    .await;

    let moved = mcp_call_tool(
        &app,
        &session_id,
        "memory_move",
        json!({
            "project": project,
            "identifier": created["permalink"],
            "type": "proposed_adr"
        }),
    )
    .await;

    assert_eq!(moved["note_type"], "proposed_adr");
    assert_eq!(moved["folder"], "decisions/proposed");
    assert!(Path::new(moved["file_path"].as_str().unwrap()).exists());

    let proposals = mcp_call_tool(
        &app,
        &session_id,
        "propose_adr_list",
        json!({"project": project}),
    )
    .await;

    assert!(proposals["items"].as_array().is_some_and(|items| {
        items.iter().any(|item| item["title"] == "Recover Me")
    }));
}

#[tokio::test]
async fn mcp_proposal_pipeline_regression_recovers_worktree_draft_survives_sync_and_lists() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let project = &proj.path;
    let worktree = Path::new(project).join(".djinn/worktrees/proposal-pipeline-regression");
    std::fs::create_dir_all(worktree.join(".git")).expect("create synthetic .git dir");
    let app = create_test_app_with_db(db.clone());
    let worktree_header = worktree.to_string_lossy().to_string();
    let session_id = initialize_mcp_session_with_headers(
        &app,
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;

    let created = mcp_call_tool_with_headers(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": project,
            "title": "Pipeline Regression Draft",
            "content": "---\nwork_shape: epic\noriginating_spike_id: ih6u-regression\n---\n\n# Pipeline Regression Draft\n\nRecovered draft survives close.\n",
            "type": "adr"
        }),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;

    assert_eq!(created["folder"], "decisions");
    assert!(
        worktree
            .join(".djinn/decisions/pipeline-regression-draft.md")
            .exists(),
        "initial draft should exist in the worktree decisions folder"
    );

    let moved = mcp_call_tool_with_headers(
        &app,
        &session_id,
        "memory_move",
        json!({
            "project": project,
            "identifier": created["permalink"],
            "type": "proposed_adr"
        }),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;

    assert_eq!(moved["note_type"], "proposed_adr");
    assert_eq!(moved["folder"], "decisions/proposed");
    assert!(
        worktree
            .join(".djinn/decisions/proposed/pipeline-regression-draft.md")
            .exists(),
        "recovered draft should exist in worktree proposed folder before close sync"
    );
    std::fs::write(
        worktree.join(".djinn/decisions/proposed/pipeline-regression-draft.md"),
        "---\ntitle: Pipeline Regression Draft\nwork_shape: epic\noriginating_spike_id: ih6u-regression\n---\n\n# Pipeline Regression Draft\n\nRecovered draft survives close.\n",
    )
    .expect("overwrite recovered draft with proposal frontmatter");

    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project_id: String = project_repo.resolve_or_create(project).await.unwrap();
    let worktree_repo = NoteRepository::new(db.clone(), EventBus::noop())
        .with_worktree_root(Some(worktree.clone()));
    let synced = worktree_repo
        .sync_worktree_notes_to_canonical(&project_id, Path::new(project), &worktree)
        .await
        .expect("sync worktree notes to canonical memory");
    assert_eq!(synced, 1);

    let canonical_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let canonical = canonical_repo
        .get_by_permalink(&project_id, "decisions/proposed/pipeline-regression-draft")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical.note_type, "proposed_adr");
    assert!(Path::new(&canonical.file_path).exists());

    let proposals = mcp_call_tool(
        &app,
        &session_id,
        "propose_adr_list",
        json!({"project": project}),
    )
    .await;

    assert!(proposals["items"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["id"] == "pipeline-regression-draft"
                && item["title"] == "Pipeline Regression Draft"
        })
    }));
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
    let worktree = workspace_tempdir("mcp-memory-worktree-");
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
async fn mcp_singleton_memory_writes_use_canonical_project_root_and_mirror_worktree() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let project = &proj.path;
    let worktree = workspace_tempdir("mcp-memory-worktree-");
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
        json!({"project": project, "title": "Project Brief", "content": "alpha", "type": "brief"}),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;
    assert_eq!(created["permalink"], "brief");

    let edited = mcp_call_tool_with_headers(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project, "identifier": "brief", "operation": "append", "content": "beta"}),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;
    assert!(edited["content"].as_str().unwrap().contains("beta"));

    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project_id: String = project_repo.resolve_or_create(project).await.unwrap();
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&project_id, "brief")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(note.permalink, "brief");
    assert_eq!(note.note_type, "brief");
    let canonical_path = Path::new(&note.file_path).to_path_buf();
    let worktree_path = worktree.path().join(".djinn/brief.md");
    assert_eq!(canonical_path, Path::new(project).join(".djinn/brief.md"));
    assert!(
        canonical_path.exists(),
        "singleton canonical file should exist"
    );
    assert!(
        worktree_path.exists(),
        "singleton worktree mirror should exist"
    );

    let canonical_contents =
        std::fs::read_to_string(&canonical_path).expect("read canonical brief");
    let worktree_contents =
        std::fs::read_to_string(&worktree_path).expect("read worktree brief");
    assert!(canonical_contents.contains("alpha"));
    assert!(canonical_contents.contains("beta"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(&project_id, "reference/project-brief")
            .await
            .unwrap()
            .is_none(),
        "singleton write should not retarget to reference note"
    );
    assert!(
        !Path::new(project)
            .join(".djinn/reference/project-brief.md")
            .exists(),
        "singleton write should not create duplicate typed note"
    );
}

#[tokio::test]
async fn mcp_current_requirement_edits_use_canonical_project_root_and_mirror_worktree() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let project = &proj.path;
    let worktree = workspace_tempdir("mcp-current-requirement-worktree-");
    std::fs::create_dir_all(worktree.path().join(".git")).expect("create synthetic .git dir");
    let app = create_test_app_with_db(db.clone());
    let worktree_header = worktree.path().to_string_lossy().to_string();
    let session_id = initialize_mcp_session_with_headers(
        &app,
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;

    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project_id: String = project_repo.resolve_or_create(project).await.unwrap();
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    note_repo
        .create(
            &project_id,
            Path::new(project),
            "V1 Requirements",
            "alpha [[Cognitive Memory Scope]]",
            "requirement",
            "[]",
        )
        .await
        .unwrap();

    let edited = mcp_call_tool_with_headers(
        &app,
        &session_id,
        "memory_edit",
        json!({
            "project": project,
            "identifier": "requirements/v1-requirements",
            "operation": "find_replace",
            "find_text": "[[Cognitive Memory Scope]]",
            "content": "[[reference/cognitive-memory-scope]]"
        }),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;
    assert!(edited["content"].as_str().unwrap().contains("[[reference/cognitive-memory-scope]]"));

    let note = note_repo
        .get_by_permalink(&project_id, "requirements/v1-requirements")
        .await
        .unwrap()
        .unwrap();

    let canonical_path = Path::new(&note.file_path).to_path_buf();
    let worktree_path = worktree.path().join(".djinn/requirements/v1-requirements.md");
    assert_eq!(
        canonical_path,
        Path::new(project).join(".djinn/requirements/v1-requirements.md")
    );
    assert!(canonical_path.exists(), "current-note canonical file should exist");
    assert!(worktree_path.exists(), "current-note worktree mirror should exist");

    let canonical_contents =
        std::fs::read_to_string(&canonical_path).expect("read canonical requirements");
    let worktree_contents =
        std::fs::read_to_string(&worktree_path).expect("read worktree requirements");
    assert!(canonical_contents.contains("[[reference/cognitive-memory-scope]]"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(&project_id, "reference/v1-requirements")
            .await
            .unwrap()
            .is_none(),
        "current-note edit should not retarget to reference note"
    );
    assert!(
        !Path::new(project)
            .join(".djinn/reference/v1-requirements.md")
            .exists(),
        "current-note edit should not create duplicate typed note"
    );
}

#[tokio::test]
async fn mcp_memory_list_all_and_filters_by_folder_and_type() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let project = &proj.path;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let adr = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "A", "content": "x", "type": "adr"}),
    )
    .await;
    assert_eq!(adr["deduplicated"], false);
    let reference = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "B", "content": "different content", "type": "reference"}),
    )
    .await;
    assert_eq!(reference["deduplicated"], false);

    let all = mcp_call_tool(
        &app,
        &session_id,
        "memory_list",
        json!({"project": project}),
    )
    .await;
    assert_eq!(all["notes"].as_array().unwrap().len(), 2);

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
    let (project_row, _dir) = create_test_project_with_dir(&db).await;
    let epic = create_test_epic(&db, &project_row.id).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;
    let project = project_row.path.clone();

    let note = mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Task Ref Note", "content": "task refs seed", "type": "reference"})).await;

    let task = mcp_call_tool(&app, &session_id, "task_create", json!({"project": project, "epic_id": epic.id, "title": "Task referencing memory note", "issue_type": "task", "priority": 2, "status": "open", "memory_refs": [note["permalink"]], "acceptance_criteria": ["note is attached to task"]})).await;
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
