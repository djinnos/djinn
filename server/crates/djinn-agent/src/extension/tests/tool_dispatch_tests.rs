use super::*;

#[tokio::test]
async fn write_rejects_symlink_escape_outside_worktree() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
    let outside = crate::test_helpers::test_tempdir("djinn-ext-outside-");
    let link = worktree.path().join("escape-link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

    let args = Some(
        serde_json::json!({"path":"escape-link/pwned.txt","content":"owned"})
            .as_object()
            .expect("obj")
            .clone(),
    );

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = call_write(&state, &args, worktree.path()).await;
    assert!(result.is_err());
    let err = result.err().unwrap_or_default();
    assert!(err.contains("outside worktree"));
    assert!(!outside.path().join("pwned.txt").exists());
}

#[tokio::test]
async fn call_tool_dispatches_task_create_with_public_response_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let response = call_tool(
        &state,
        "task_create",
        Some(
            serde_json::json!({
                "epic_id": epic.short_id,
                "title": "Dispatch-created task",
                "description": "Created through extension dispatch",
                "design": "Keep the response shape stable",
                "priority": 3,
                "owner": "planner",
                "acceptance_criteria": ["first criterion"],
                "memory_refs": ["decisions/adr-041-unified-tool-service-layer-in-djinn-mcp"],
                "agent_type": "rust-expert"
            })
            .as_object()
            .expect("task_create args object")
            .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("planner"),
        None,
    )
    .await
    .expect("task_create dispatch should succeed");

    assert_eq!(
        response.get("title").and_then(|v| v.as_str()),
        Some("Dispatch-created task")
    );
    assert_eq!(
        response.get("description").and_then(|v| v.as_str()),
        Some("Created through extension dispatch")
    );
    assert_eq!(response.get("priority").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(
        response.get("owner").and_then(|v| v.as_str()),
        Some("planner")
    );
    assert_eq!(
        response.get("status").and_then(|v| v.as_str()),
        Some("open")
    );
    // Historical note: the public task response reflects the task as
    // persisted, which includes `agent_type` when the caller specified it.
    assert_eq!(
        response.get("agent_type").and_then(|v| v.as_str()),
        Some("rust-expert")
    );
    assert_eq!(
        response
            .get("acceptance_criteria")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item
                .as_str()
                .or_else(|| item.get("criterion").and_then(|v| v.as_str()))),
        Some("first criterion")
    );
    assert_eq!(
        response
            .get("memory_refs")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|v| v.as_str()),
        Some("decisions/adr-041-unified-tool-service-layer-in-djinn-mcp")
    );
}

#[tokio::test]
async fn call_tool_dispatches_task_update_with_public_response_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let response = call_tool(
        &state,
        "task_update",
        Some(
            serde_json::json!({
                "id": task.short_id,
                "title": "Dispatch-updated task",
                "description": "Updated through extension dispatch",
                "design": "Keep the update response shape stable",
                "priority": 2,
                "owner": "planner",
                "labels_add": ["migration-test"],
                "acceptance_criteria": [{"criterion": "updated criterion", "met": false}],
                "memory_refs_add": ["decisions/adr-041-unified-tool-service-layer-in-djinn-mcp"]
            })
            .as_object()
            .expect("task_update args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("planner"),
        None,
    )
    .await
    .expect("task_update dispatch should succeed");

    assert_eq!(
        response.get("id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        response.get("short_id").and_then(|v| v.as_str()),
        Some(task.short_id.as_str())
    );
    assert_eq!(
        response.get("title").and_then(|v| v.as_str()),
        Some("Dispatch-updated task")
    );
    assert_eq!(
        response.get("description").and_then(|v| v.as_str()),
        Some("Updated through extension dispatch")
    );
    assert_eq!(
        response.get("design").and_then(|v| v.as_str()),
        Some("Keep the update response shape stable")
    );
    assert_eq!(response.get("priority").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(
        response.get("owner").and_then(|v| v.as_str()),
        Some("planner")
    );
    assert_eq!(
        response
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|labels| labels
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()),
        Some(vec!["migration-test"])
    );
    assert_eq!(
        response
            .get("acceptance_criteria")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item
                .as_str()
                .or_else(|| item.get("criterion").and_then(|v| v.as_str()))),
        Some("updated criterion")
    );
    assert_eq!(
        response
            .get("memory_refs")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|v| v.as_str()),
        Some("decisions/adr-041-unified-tool-service-layer-in-djinn-mcp")
    );
}

#[tokio::test]
async fn call_tool_dispatches_comment_and_transition_flows() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let comment = call_tool(
        &state,
        "task_comment_add",
        Some(
            serde_json::json!({
                "id": task.short_id,
                "body": "Dispatch-level architect note"
            })
            .as_object()
            .expect("task_comment_add args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("architect"),
        None,
    )
    .await
    .expect("task_comment_add dispatch should succeed");

    assert_eq!(
        comment.get("task_id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        comment.get("actor_id").and_then(|v| v.as_str()),
        Some("architect")
    );
    assert_eq!(
        comment.get("actor_role").and_then(|v| v.as_str()),
        Some("architect")
    );
    assert_eq!(
        comment.get("event_type").and_then(|v| v.as_str()),
        Some("comment")
    );
    assert_eq!(
        comment
            .get("payload")
            .and_then(|v| v.get("body"))
            .and_then(|v| v.as_str()),
        Some("Dispatch-level architect note")
    );

    let transitioned = call_tool(
        &state,
        "task_transition",
        Some(
            serde_json::json!({
                "id": task.short_id,
                "action": "start"
            })
            .as_object()
            .expect("task_transition args object")
            .clone(),
        ),
        Path::new(&project_path),
        Some(&task.id),
        Some("lead"),
        None,
    )
    .await
    .expect("task_transition dispatch should succeed");

    assert_eq!(
        transitioned.get("id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert_eq!(
        transitioned.get("short_id").and_then(|v| v.as_str()),
        Some(task.short_id.as_str())
    );
    assert_eq!(
        transitioned.get("status").and_then(|v| v.as_str()),
        Some("in_progress")
    );
    assert_eq!(
        transitioned.get("title").and_then(|v| v.as_str()),
        Some(task.title.as_str())
    );
}

#[tokio::test]
async fn call_tool_dispatches_agent_ops_through_shared_agent_seam() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project).to_string_lossy().into_owned();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let create_response = call_tool(
        &state,
        "agent_create",
        Some(
            serde_json::json!({
                "project": project_path.clone(),
                "name": "Rust specialist",
                "base_role": "worker",
                "description": "Handles Rust-heavy tasks",
                "system_prompt_extensions": "Focus on Rust diagnostics",
                "model_preference": "gpt-5"
            })
            .as_object()
            .expect("agent_create args object")
            .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("architect"),
        None,
    )
    .await
    .expect("agent_create dispatch should succeed");

    assert_eq!(
        create_response
            .get("agent_name")
            .and_then(|value| value.as_str()),
        Some("Rust specialist")
    );
    assert_eq!(
        create_response
            .get("base_role")
            .and_then(|value| value.as_str()),
        Some("worker")
    );
    assert_eq!(
        create_response
            .get("created")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    let created_agent_id = create_response
        .get("agent_id")
        .and_then(|value| value.as_str())
        .expect("agent id in create response")
        .to_string();

    let metrics_response = call_tool(
        &state,
        "agent_metrics",
        Some(
            serde_json::json!({
                "project": project_path.clone(),
                "agent_id": created_agent_id,
                "window_days": 14
            })
            .as_object()
            .expect("agent_metrics args object")
            .clone(),
        ),
        Path::new(&project_path),
        None,
        Some("architect"),
        None,
    )
    .await
    .expect("agent_metrics dispatch should succeed");

    assert_eq!(
        metrics_response
            .get("window_days")
            .and_then(|value| value.as_i64()),
        Some(14)
    );
    let roles = metrics_response
        .get("roles")
        .and_then(|value| value.as_array())
        .expect("roles array in metrics response");
    assert_eq!(roles.len(), 1);
    assert_eq!(
        roles[0].get("agent_name").and_then(|value| value.as_str()),
        Some("Rust specialist")
    );
    assert_eq!(
        roles[0].get("base_role").and_then(|value| value.as_str()),
        Some("worker")
    );
    assert!(roles[0].get("learned_prompt").is_some());
    let extraction_quality = roles[0]
        .get("extraction_quality")
        .and_then(|value| value.as_object())
        .expect("extraction_quality object");
    assert_eq!(
        extraction_quality
            .get("extracted")
            .and_then(|value| value.as_i64()),
        Some(0)
    );
}
