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
    let result = call_write(&state, &args, worktree.path(), None, None, None).await;
    assert!(result.is_err());
    let err = result.err().unwrap_or_default();
    assert!(err.contains("outside worktree"));
    assert!(!outside.path().join("pwned.txt").exists());
}

/// `call_write` should enrich its response with a `related_files` list
/// when the coupling index has co-edit data for the touched path and
/// the dispatcher hands us a `project_id`. Verifies the write-nudge
/// wire-up end-to-end: threshold (`co_edits >= 2`), exclusion filter,
/// and top-5 cap.
#[tokio::test]
async fn call_write_emits_related_files_when_coupling_data_exists() {
    use djinn_db::{CommitFileChange, CommitFileChangeRepository};

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-coupling-");

    // Seed commit_file_changes so that `src/a.rs` co-edits twice with
    // `src/b.rs` (→ related, co_edits=2), once with `src/c.rs` (→
    // below threshold, dropped), and `dist/out.js` lives under an
    // exclusion glob that we configure on the project.
    let repo = CommitFileChangeRepository::new(db.clone());
    let pid = project.id.as_str();
    let row = |sha: &str, path: &str, ts: &str| CommitFileChange {
        project_id: pid.to_owned(),
        commit_sha: sha.into(),
        file_path: path.into(),
        change_kind: "M".into(),
        committed_at: ts.into(),
        author_email: "t@t".into(),
        insertions: 1,
        deletions: 0,
        old_path: None,
    };
    let rows = vec![
        row("c1", "src/a.rs", "2026-04-01T00:00:00Z"),
        row("c1", "src/b.rs", "2026-04-01T00:00:00Z"),
        row("c2", "src/a.rs", "2026-04-02T00:00:00Z"),
        row("c2", "src/b.rs", "2026-04-02T00:00:00Z"),
        row("c3", "src/a.rs", "2026-04-03T00:00:00Z"),
        row("c3", "src/c.rs", "2026-04-03T00:00:00Z"),
        row("c4", "src/a.rs", "2026-04-04T00:00:00Z"),
        row("c4", "dist/out.js", "2026-04-04T00:00:00Z"),
        row("c5", "src/a.rs", "2026-04-05T00:00:00Z"),
        row("c5", "dist/out.js", "2026-04-05T00:00:00Z"),
    ];
    repo.upsert_batch(&rows).await.expect("seed coupling");
    // Coupling reads from `coupling_pair_events` (built at ingest by
    // the warmer); this test seeds raw rows so we drive the same
    // backfill path the warmer triggers on first run after the
    // pair-events migration landed.
    repo.rebuild_pair_events_for_project(pid)
        .await
        .expect("rebuild pair events");

    // Set an exclusion glob that hides `dist/**` — the reused
    // `GraphExclusions` matcher should drop `dist/out.js` from the
    // `related_files` output even though it has co_edits=2.
    let project_repo = djinn_db::ProjectRepository::new(db.clone(), EventBus::noop());
    project_repo
        .update_config_field(&project.id, "graph_excluded_paths", "[\"dist/**\"]")
        .await
        .expect("set exclusions");

    // Pre-create `src/` so the `canonicalize(parent)` path check in
    // ensure_path_within_worktree can resolve the directory.
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    let args = Some(
        serde_json::json!({
            "path": "src/a.rs",
            "content": "// hello\n",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");

    let related = response
        .get("related_files")
        .and_then(|v| v.as_array())
        .expect("related_files present");
    // Expect exactly one entry: src/b.rs (co_edits=2). src/c.rs is
    // below threshold; dist/out.js is excluded.
    assert_eq!(related.len(), 1, "expected 1 entry, got {related:?}");
    let first = &related[0];
    assert_eq!(first.get("path").and_then(|v| v.as_str()), Some("src/b.rs"));
    assert_eq!(first.get("co_edits").and_then(|v| v.as_i64()), Some(2));
}

/// `call_write` should NOT emit `related_files` when there's no
/// coupling data for the project — the field is omitted rather than
/// serialized as `null` or `[]` to keep the JSON shape stable for
/// day-one projects.
#[tokio::test]
async fn call_write_omits_related_files_when_coupling_empty() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-coupling-empty-");

    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    let args = Some(
        serde_json::json!({
            "path": "src/lonely.rs",
            "content": "// new file\n",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let response = call_write(
        &state,
        &args,
        worktree.path(),
        Some(project.id.as_str()),
        None,
        None,
    )
    .await
    .expect("write");
    assert!(
        response.get("related_files").is_none(),
        "expected no related_files field, got {response:?}"
    );
}

#[tokio::test]
async fn call_tool_dispatches_task_create_with_public_response_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let response = call_tool(
        &state,
        &crate::test_helpers::test_services(),
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
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let response = call_tool(
        &state,
        &crate::test_helpers::test_services(),
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
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
    state.task_ops_project_path_override = Some(project_path.clone().into());

    let comment = call_tool(
        &state,
        &crate::test_helpers::test_services(),
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
        &crate::test_helpers::test_services(),
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
    let project_path = crate::extension::tests::project_fs_path(&project)
        .to_string_lossy()
        .into_owned();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    let create_response = call_tool(
        &state,
        &crate::test_helpers::test_services(),
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
        &crate::test_helpers::test_services(),
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
    assert!(
        roles[0].get("success_rate").is_some(),
        "metrics entry should have success_rate"
    );
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

/// (G2) A windowed read (`offset` + `limit`) must return exactly the right
/// window AND must stop streaming before reaching the rest of the file. We
/// prove the early-stop by planting a NUL byte well past the window: a
/// whole-file read would reject the file as binary, but the windowed read
/// stops before ever touching the NUL, so it succeeds.
#[tokio::test]
async fn read_window_does_not_scan_whole_file() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-window-");
    let file = worktree.path().join("big.txt");

    // 50 clean text lines, then a line containing a NUL byte far past the
    // window we will request.
    let mut contents = String::new();
    for i in 0..50 {
        contents.push_str(&format!("line {i}\n"));
    }
    let mut bytes = contents.into_bytes();
    bytes.extend_from_slice(b"poisoned\0line\n");
    tokio::fs::write(&file, &bytes).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "big.txt", "offset": 2, "limit": 3 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let result = call_read(&state, &args, worktree.path())
        .await
        .expect("windowed read should succeed without scanning the NUL");

    let content = result
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content");
    assert!(content.contains("line 2"), "got: {content}");
    assert!(content.contains("line 3"), "got: {content}");
    assert!(content.contains("line 4"), "got: {content}");
    assert!(
        !content.contains("line 5"),
        "window leaked extra line: {content}"
    );
    assert!(
        !content.contains("line 1\n"),
        "window leaked earlier line: {content}"
    );
    assert_eq!(result.get("offset").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(result.get("limit").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(result.get("has_more").and_then(|v| v.as_bool()), Some(true));
}

/// (G2) A file exceeding the byte budget must return a truncation signal in
/// the content rather than reading the whole thing into memory.
#[tokio::test]
async fn read_truncates_file_over_byte_budget() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-budget-");
    let file = worktree.path().join("huge.txt");

    // Long lines (~6 KiB each) so that the requested window of 2000 lines
    // crosses the 8 MiB budget before the line count stops the scan: the
    // budget — not the window — must be what halts the read and emits the
    // truncation signal.
    let line = "x".repeat(6 * 1024 - 1); // line + '\n' = 6 KiB/line
    let mut contents = String::with_capacity(9 * 1024 * 1024 + 1024);
    let target = 9 * 1024 * 1024;
    while contents.len() < target {
        contents.push_str(&line);
        contents.push('\n');
    }
    tokio::fs::write(&file, contents.as_bytes())
        .await
        .expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "huge.txt", "offset": 0, "limit": 2000 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let result = call_read(&state, &args, worktree.path())
        .await
        .expect("over-budget read should succeed with a truncation signal");

    let content = result
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content");
    assert!(
        content.contains("file too large") && content.contains("truncated"),
        "expected truncation signal, got tail: {}",
        &content[content.len().saturating_sub(200)..]
    );
    assert_eq!(result.get("has_more").and_then(|v| v.as_bool()), Some(true));
}

/// (G2) A file containing NUL bytes within the scanned window must be
/// detected as binary and rejected (not dumped as bytes).
#[tokio::test]
async fn read_detects_binary_file() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-binary-");
    let file = worktree.path().join("blob.bin");
    tokio::fs::write(&file, b"\x00\x01\x02\x03binary\xff\x00data")
        .await
        .expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "blob.bin" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let err = call_read(&state, &args, worktree.path())
        .await
        .expect_err("binary file must be rejected");
    assert!(err.contains("binary file"), "got: {err}");
}

// ─── ReadCoverage: accurate coverage metadata from call_read ──────────────

/// A small file read from offset 0 with no budget truncation must record
/// full-file coverage.
#[tokio::test]
async fn read_full_file_records_full_coverage() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-cov-full-");
    let file = worktree.path().join("small.txt");
    // 5 lines, well under the default limit of 2000.
    tokio::fs::write(&file, "line 0\nline 1\nline 2\nline 3\nline 4\n")
        .await
        .expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "small.txt", "offset": 0, "limit": 2000 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let result = call_read(&state, &args, worktree.path())
        .await
        .expect("full-file read should succeed");

    assert_eq!(
        result.get("has_more").and_then(|v| v.as_bool()),
        Some(false),
        "full-file read must have has_more=false"
    );

    // Inspect the recorded coverage.
    let worktree_key = worktree.path().display().to_string();
    let path = worktree.path().join("small.txt");
    let rec = state
        .file_time
        .latest_record(&worktree_key, &path)
        .await
        .expect("read record should exist");
    assert!(
        rec.is_full(),
        "full-file read should record ReadCoverage::Full, got {:?}",
        rec.coverage
    );
    assert!(
        !rec.truncated,
        "small-file read must not be marked truncated"
    );
}

/// A read with offset > 0 and limit < remaining lines must record partial
/// (Range) coverage with accurate byte boundaries.
#[tokio::test]
async fn read_offset_limit_records_range_coverage() {
    use crate::file_time::ReadCoverage;

    let worktree = crate::test_helpers::test_tempdir("djinn-ext-cov-range-");
    let file = worktree.path().join("lines.txt");

    // Build a file with known line lengths. Each line is "NNN\n" = 4 bytes.
    let mut contents = String::new();
    for i in 0..50 {
        contents.push_str(&format!("{:03}\n", i));
    }
    tokio::fs::write(&file, &contents).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    // Read lines [10, 15) — offset=10, limit=5.
    let args = Some(
        serde_json::json!({ "file_path": "lines.txt", "offset": 10, "limit": 5 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let result = call_read(&state, &args, worktree.path())
        .await
        .expect("windowed read should succeed");

    assert_eq!(
        result.get("has_more").and_then(|v| v.as_bool()),
        Some(true),
        "windowed read with remaining content must have has_more=true"
    );

    let worktree_key = worktree.path().display().to_string();
    let path = worktree.path().join("lines.txt");
    let rec = state
        .file_time
        .latest_record(&worktree_key, &path)
        .await
        .expect("read record should exist");

    match rec.coverage {
        ReadCoverage::Range { start, end } => {
            // Each line is 4 bytes ("NNN\n"). Lines 0–9 precede our window,
            // so the window starts at byte 40.
            assert_eq!(start, 40, "range should start at line 10's byte offset");
            // Window is lines 10–14 inclusive. Line 14 ends at byte 60
            // (15 lines × 4 bytes). The exclusive end is the byte offset
            // after line 14, which is 15 * 4 = 60.
            assert_eq!(
                end,
                Some(60),
                "range should end at byte offset after line 14"
            );
        }
        other => panic!("expected Range coverage, got {other:?}"),
    }
    assert!(
        !rec.truncated,
        "offset/limit read within budget must not be truncated"
    );
}

/// A large file exceeding the byte budget must record truncated=true and
/// must NOT record full-file coverage.
#[tokio::test]
async fn read_budget_truncated_records_truncated_coverage() {
    use crate::file_time::ReadCoverage;

    let worktree = crate::test_helpers::test_tempdir("djinn-ext-cov-trunc-");
    let file = worktree.path().join("huge.txt");

    // Long lines (~6 KiB each) so the default limit=2000 window crosses the
    // 8 MiB byte budget.
    let line = "x".repeat(6 * 1024 - 1); // line + '\n' = 6 KiB/line
    let mut contents = String::with_capacity(9 * 1024 * 1024 + 1024);
    let target = 9 * 1024 * 1024;
    while contents.len() < target {
        contents.push_str(&line);
        contents.push('\n');
    }
    tokio::fs::write(&file, contents.as_bytes())
        .await
        .expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "huge.txt", "offset": 0, "limit": 2000 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let result = call_read(&state, &args, worktree.path())
        .await
        .expect("over-budget read should succeed with truncation signal");

    let content = result
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content");
    assert!(
        content.contains("file too large"),
        "expected truncation signal"
    );

    let worktree_key = worktree.path().display().to_string();
    let path = worktree.path().join("huge.txt");
    let rec = state
        .file_time
        .latest_record(&worktree_key, &path)
        .await
        .expect("read record should exist");

    assert!(
        rec.truncated,
        "budget-truncated read must have truncated=true"
    );
    assert!(
        !rec.is_full(),
        "budget-truncated read must not be recorded as full-file coverage"
    );
    // The coverage must be a Range (not Full).
    assert!(
        matches!(rec.coverage, ReadCoverage::Range { .. }),
        "budget-truncated read should record Range coverage, got {:?}",
        rec.coverage
    );
    // The range start should be 0 since offset=0.
    if let ReadCoverage::Range { start, end } = rec.coverage {
        assert_eq!(start, 0, "range should start at byte 0 for offset=0 read");
        assert!(
            end.is_some(),
            "range should have a concrete end for a truncated read"
        );
    }
}
