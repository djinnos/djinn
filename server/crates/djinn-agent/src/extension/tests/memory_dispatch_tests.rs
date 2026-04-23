use super::*;

#[tokio::test]
async fn call_tool_dispatches_memory_ops_through_shared_memory_seam() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let seed = note_repo
        .create(
            &project.id,
            Path::new(&project_path),
            "Shared Memory Seed",
            "Architecture guidance with [[Shared Memory Related]] references.",
            "adr",
            "[]",
        )
        .await
        .expect("create seed note");
    note_repo
        .create(
            &project.id,
            Path::new(&project_path),
            "Shared Memory Related",
            "Related architecture context.",
            "reference",
            "[]",
        )
        .await
        .expect("create related note");

    let search_response = call_tool(
        &state,
        "memory_search",
        Some(
            serde_json::json!({
                "project": project_path.clone(),
                "query": "architecture",
                "limit": 5
            })
            .as_object()
            .expect("memory_search args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_search dispatch should succeed");
    assert!(
        search_response.get("error").is_none()
            || search_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert!(
        search_response
            .get("results")
            .and_then(|value| value.as_array())
            .is_some_and(|results| !results.is_empty())
    );

    let read_response = call_tool(
        &state,
        "memory_read",
        Some(
            serde_json::json!({
                "project": project_path.clone(),
                "identifier": seed.permalink
            })
            .as_object()
            .expect("memory_read args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_read dispatch should succeed");
    assert!(
        read_response.get("error").is_none()
            || read_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert_eq!(
        read_response
            .get("permalink")
            .and_then(|value| value.as_str()),
        Some(seed.permalink.as_str())
    );

    let list_response = call_tool(
        &state,
        "memory_list",
        Some(
            serde_json::json!({
                "project": project_path.clone(),
                "folder": "decisions",
                "depth": 1
            })
            .as_object()
            .expect("memory_list args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_list dispatch should succeed");
    assert!(
        list_response.get("error").is_none()
            || list_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert!(
        list_response
            .get("notes")
            .and_then(|value| value.as_array())
            .is_some_and(|notes| !notes.is_empty())
    );

    let context_response = call_tool(
        &state,
        "memory_build_context",
        Some(
            serde_json::json!({
                "project": project_path.clone(),
                "url": format!("memory://{}", seed.permalink),
                "budget": 512,
                "max_related": 5
            })
            .as_object()
            .expect("memory_build_context args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_build_context dispatch should succeed");
    assert!(
        context_response.get("error").is_none()
            || context_response
                .get("error")
                .is_some_and(|value| value.is_null())
    );
    assert_eq!(
        context_response
            .get("primary")
            .and_then(|value| value.as_array())
            .map(|items| items.len()),
        Some(1)
    );
}

#[tokio::test]
async fn call_tool_architect_dispatches_memory_move_for_proposed_adr_recovery() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let note_repo = NoteRepository::new(db, EventBus::noop());
    let note = note_repo
        .create(
            &project.id,
            Path::new(&project_path),
            "Draft To Recover",
            "# Draft To Recover",
            "adr",
            "[]",
        )
        .await
        .expect("create seed adr");

    let moved = call_tool(
        &state,
        "memory_move",
        Some(
            serde_json::json!({
                "identifier": note.permalink,
                "type": "proposed_adr"
            })
            .as_object()
            .expect("memory_move args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("memory_move dispatch should succeed");

    assert_eq!(moved["folder"], "decisions/proposed");
}

#[tokio::test]
async fn call_tool_memory_detail_ops_treat_missing_or_empty_folder_as_project_wide() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    note_repo
        .create(
            &project.id,
            Path::new(&project_path),
            "Broken Source",
            "Broken reference to [[Missing Note]].",
            "research",
            "[]",
        )
        .await
        .expect("create broken source note");
    note_repo
        .create(
            &project.id,
            Path::new(&project_path),
            "Standalone Orphan",
            "No inbound links here.",
            "pattern",
            "[]",
        )
        .await
        .expect("create orphan note");

    let health = note_repo.health(&project.id).await.expect("health report");

    let broken_links_no_arg = call_tool(
        &state,
        "memory_broken_links",
        Some(
            serde_json::json!({})
                .as_object()
                .expect("empty args object")
                .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("planner"),
        None,
    )
    .await
    .expect("memory_broken_links dispatch should succeed");
    assert_eq!(
        broken_links_no_arg["broken_links"]
            .as_array()
            .map(|items| items.len()),
        Some(health.broken_link_count as usize)
    );

    let broken_links_empty_folder = call_tool(
        &state,
        "memory_broken_links",
        Some(
            serde_json::json!({ "folder": "" })
                .as_object()
                .expect("broken_links args object")
                .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("planner"),
        None,
    )
    .await
    .expect("memory_broken_links empty-folder dispatch should succeed");
    assert_eq!(
        broken_links_empty_folder["broken_links"]
            .as_array()
            .map(|items| items.len()),
        Some(health.broken_link_count as usize)
    );

    let orphans_no_arg = call_tool(
        &state,
        "memory_orphans",
        Some(
            serde_json::json!({})
                .as_object()
                .expect("empty args object")
                .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("planner"),
        None,
    )
    .await
    .expect("memory_orphans dispatch should succeed");
    assert_eq!(
        orphans_no_arg["orphans"]
            .as_array()
            .map(|items| items.len()),
        Some(health.orphan_note_count as usize)
    );

    let orphans_empty_folder = call_tool(
        &state,
        "memory_orphans",
        Some(
            serde_json::json!({ "folder": "" })
                .as_object()
                .expect("orphans args object")
                .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("planner"),
        None,
    )
    .await
    .expect("memory_orphans empty-folder dispatch should succeed");
    assert_eq!(
        orphans_empty_folder["orphans"]
            .as_array()
            .map(|items| items.len()),
        Some(health.orphan_note_count as usize)
    );
}

#[tokio::test]
async fn call_tool_memory_singletons_target_canonical_project_root_from_worktree() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    std::fs::create_dir_all(&project_path).expect("create project dir");
    let worktree = Path::new(&project_path).join(".djinn/worktrees/test-singleton-worktree");
    std::fs::create_dir_all(worktree.join(".git")).expect("create worktree dir");

    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let created = call_tool(
        &state,
        "memory_write",
        Some(
            serde_json::json!({
                "project": worktree.display().to_string(),
                "title": "Project Roadmap",
                "content": "tracks [[ADR-043 Repo Graph]]",
                "type": "roadmap"
            })
            .as_object()
            .expect("memory_write args object")
            .clone(),
        ),
        &worktree,
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("memory_write dispatch should succeed");

    assert_eq!(
        created.get("permalink").and_then(|v| v.as_str()),
        Some("roadmap")
    );

    let edited = call_tool(
        &state,
        "memory_edit",
        Some(
            serde_json::json!({
                "project": worktree.display().to_string(),
                "identifier": "roadmap",
                "operation": "append",
                "content": "next wave"
            })
            .as_object()
            .expect("memory_edit args object")
            .clone(),
        ),
        &worktree,
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("memory_edit dispatch should succeed");

    assert!(
        edited
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("next wave")
    );

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&project.id, "roadmap")
        .await
        .expect("load roadmap")
        .expect("roadmap note exists");

    assert_eq!(note.note_type, "roadmap");
    assert_eq!(note.permalink, "roadmap");
    assert_eq!(
        Path::new(&note.file_path),
        Path::new(&project_path).join(".djinn/roadmap.md")
    );

    let canonical_contents =
        std::fs::read_to_string(Path::new(&project_path).join(".djinn/roadmap.md"))
            .expect("read canonical roadmap");
    let worktree_contents =
        std::fs::read_to_string(worktree.join(".djinn/roadmap.md")).expect("read worktree roadmap");
    assert!(canonical_contents.contains("ADR-043 Repo Graph"));
    assert!(canonical_contents.contains("next wave"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(
                &project.id,
                "reference/adr-043-roadmap-active-decomposition-status"
            )
            .await
            .expect("check duplicate roadmap note")
            .is_none()
    );
    assert!(
        !Path::new(&project_path)
            .join(".djinn/reference/adr-043-roadmap-active-decomposition-status.md")
            .exists()
    );
}

#[tokio::test]
async fn call_tool_memory_brief_singleton_targets_canonical_project_root_from_worktree() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    std::fs::create_dir_all(&project_path).expect("create project dir");
    let worktree = Path::new(&project_path).join(".djinn/worktrees/test-brief-singleton-worktree");
    std::fs::create_dir_all(worktree.join(".git")).expect("create worktree dir");

    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let created = call_tool(
        &state,
        "memory_write",
        Some(
            serde_json::json!({
                "project": worktree.display().to_string(),
                "title": "Project Brief",
                "content": "tracks [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning]]",
                "type": "brief"
            })
            .as_object()
            .expect("memory_write args object")
            .clone(),
        ),
        &worktree,
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("memory_write dispatch should succeed");

    assert_eq!(
        created.get("permalink").and_then(|v| v.as_str()),
        Some("brief")
    );

    let edited = call_tool(
        &state,
        "memory_edit",
        Some(
            serde_json::json!({
                "project": worktree.display().to_string(),
                "identifier": "brief",
                "operation": "append",
                "content": "next wave"
            })
            .as_object()
            .expect("memory_edit args object")
            .clone(),
        ),
        &worktree,
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("memory_edit dispatch should succeed");

    assert!(
        edited
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("next wave")
    );

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&project.id, "brief")
        .await
        .expect("load brief")
        .expect("brief note exists");

    assert_eq!(note.note_type, "brief");
    assert_eq!(note.permalink, "brief");
    assert_eq!(
        Path::new(&note.file_path),
        Path::new(&project_path).join(".djinn/brief.md")
    );

    let canonical_contents =
        std::fs::read_to_string(Path::new(&project_path).join(".djinn/brief.md"))
            .expect("read canonical brief");
    let worktree_contents =
        std::fs::read_to_string(worktree.join(".djinn/brief.md")).expect("read worktree brief");
    assert!(canonical_contents.contains("adr-008-agent-harness"));
    assert!(canonical_contents.contains("next wave"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(&project.id, "reference/project-brief")
            .await
            .expect("check duplicate brief note")
            .is_none()
    );
    assert!(
        !Path::new(&project_path)
            .join(".djinn/reference/project-brief.md")
            .exists()
    );
}

#[tokio::test]
async fn call_tool_dispatches_registered_mcp_tool_success() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let state = agent_context_from_db(db, CancellationToken::new());
    let registry = crate::mcp_client::McpToolRegistry::with_dispatch(
        [("web_search".to_string(), "search-server".to_string())],
        vec![serde_json::json!({"name": "web_search"})],
        move |tool_name, arguments| {
            assert_eq!(tool_name, "web_search");
            assert_eq!(
                arguments.as_ref().and_then(|args| args.get("query")),
                Some(&serde_json::json!("djinn"))
            );
            Ok(serde_json::json!({
                "items": [{"title": "Djinn", "url": "https://example.com/djinn"}]
            }))
        },
    );

    let response = call_tool(
        &state,
        "web_search",
        Some(
            serde_json::json!({
                "query": "djinn"
            })
            .as_object()
            .expect("mcp args object")
            .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("worker"),
        Some(&registry),
    )
    .await
    .expect("registered MCP tool should dispatch");

    assert_eq!(
        response
            .get("items")
            .and_then(|value| value.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("title"))
            .and_then(|value| value.as_str()),
        Some("Djinn")
    );
}

#[tokio::test]
async fn call_tool_memory_current_requirement_targets_canonical_project_root_from_worktree() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    std::fs::create_dir_all(&project_path).expect("create project dir");
    let worktree =
        Path::new(&project_path).join(".djinn/worktrees/test-current-requirement-worktree");
    std::fs::create_dir_all(worktree.join(".git")).expect("create worktree dir");

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    note_repo
        .create(
            &project.id,
            Path::new(&project_path),
            "V1 Requirements",
            "tracks [[Cognitive Memory Scope]]",
            "requirement",
            "[]",
        )
        .await
        .expect("seed requirements note");

    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let edited = call_tool(
        &state,
        "memory_edit",
        Some(
            serde_json::json!({
                "project": worktree.display().to_string(),
                "identifier": "requirements/v1-requirements",
                "operation": "find_replace",
                "find_text": "[[Cognitive Memory Scope]]",
                "content": "[[reference/cognitive-memory-scope]]"
            })
            .as_object()
            .expect("memory_edit args object")
            .clone(),
        ),
        &worktree,
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("memory_edit dispatch should succeed");

    assert!(
        edited
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("[[reference/cognitive-memory-scope]]")
    );

    let note = note_repo
        .get_by_permalink(&project.id, "requirements/v1-requirements")
        .await
        .expect("load requirements")
        .expect("requirements note exists");

    assert_eq!(note.note_type, "requirement");
    assert_eq!(note.permalink, "requirements/v1-requirements");
    assert_eq!(
        Path::new(&note.file_path),
        Path::new(&project_path).join(".djinn/requirements/v1-requirements.md")
    );

    let canonical_contents = std::fs::read_to_string(
        Path::new(&project_path).join(".djinn/requirements/v1-requirements.md"),
    )
    .expect("read canonical requirements");
    let worktree_contents =
        std::fs::read_to_string(worktree.join(".djinn/requirements/v1-requirements.md"))
            .expect("read worktree requirements");
    assert!(canonical_contents.contains("[[reference/cognitive-memory-scope]]"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(&project.id, "reference/v1-requirements")
            .await
            .expect("check duplicate requirements note")
            .is_none()
    );
    assert!(
        !Path::new(&project_path)
            .join(".djinn/reference/v1-requirements.md")
            .exists()
    );
}

#[tokio::test]
async fn call_tool_dispatches_registered_mcp_tool_error() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let state = agent_context_from_db(db, CancellationToken::new());
    let registry = crate::mcp_client::McpToolRegistry::with_dispatch(
        [("web_fetch".to_string(), "fetch-server".to_string())],
        vec![serde_json::json!({"name": "web_fetch"})],
        move |tool_name, arguments| {
            assert_eq!(tool_name, "web_fetch");
            assert_eq!(
                arguments.as_ref().and_then(|args| args.get("url")),
                Some(&serde_json::json!("https://example.com/fail"))
            );
            Err("upstream MCP error".to_string())
        },
    );

    let error = call_tool(
        &state,
        "web_fetch",
        Some(
            serde_json::json!({
                "url": "https://example.com/fail"
            })
            .as_object()
            .expect("mcp args object")
            .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("worker"),
        Some(&registry),
    )
    .await
    .expect_err("MCP errors should flow through the normal tool error path");

    assert!(error.contains("upstream MCP error"));
}
