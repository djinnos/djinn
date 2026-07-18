use super::*;
use std::fs;

/// `skill_read` is present in every role's tool schema (it rides the base set,
/// like `read`).
#[test]
fn skill_read_is_in_every_role_schema() {
    for schemas in [
        tool_schemas_worker(),
        tool_schemas_reviewer(),
        tool_schemas_lead(),
        tool_schemas_planner(),
        tool_schemas_architect(),
    ] {
        assert!(
            tool_names(&schemas).contains(&"skill_read"),
            "skill_read must be in the role schema"
        );
    }
}

// ── Native skill resolution tests ──────────────────────────────────────────

/// `skill_read(name="visual-spec")` returns the immutable native body for an
/// authoring planner session (`epic_breakdown` task) where the native skill is
/// resolved/advertised.
#[tokio::test]
async fn skill_read_serves_native_visual_spec_for_authoring_planner_session() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;

    // Create an epic_breakdown task (proposal authoring session).
    let task_repo = djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Decompose proposal into epics",
            "proposal decomposition task",
            "",
            "epic_breakdown",
            1,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create epic_breakdown task");

    let tmp = crate::test_helpers::test_tempdir("djinn-native-skill-read-");

    // skill_read for visual-spec as a planner authoring session should succeed.
    let ok = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "visual-spec" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        Some(&task.id), // production passes the task UUID, not short_id
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("skill_read should succeed for visual-spec in an authoring planner session");

    assert_eq!(ok.get("name").and_then(|v| v.as_str()), Some("visual-spec"));
    assert_eq!(
        ok.get("description").and_then(|v| v.as_str()),
        Some(
            crate::native_skills::native_skill("visual-spec")
                .unwrap()
                .description
        )
    );
    assert!(
        ok.get("content").and_then(|v| v.as_str()).unwrap().len() > 10,
        "native body should be non-trivial"
    );
    // AC4: version is exposed in the response.
    assert_eq!(
        ok.get("version").and_then(|v| v.as_str()),
        Some(crate::native_skills::VISUAL_SPEC_VERSION),
    );
}

/// `skill_read` rejects `visual-spec` in a non-authoring planner session
/// (`planning` task) where the native skill was not resolved.
#[tokio::test]
async fn skill_read_rejects_visual_spec_in_non_authoring_planner_session() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;

    // Create a planning task (non-authoring session).
    let task_repo = djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Plan next wave",
            "wave planning task",
            "",
            "planning",
            1,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create planning task");

    let tmp = crate::test_helpers::test_tempdir("djinn-native-skill-reject-");

    let err = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "visual-spec" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        Some(&task.id), // production passes the task UUID, not short_id
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("skill_read should reject visual-spec in a non-authoring planner session");

    assert!(
        err.contains("unknown skill"),
        "error should indicate the skill is not assigned, got: {err}"
    );
}

/// `skill_read(name="visual-spec")` succeeds for the tribunal **Advocate** on a
/// `refinement` task. Regression: the skill_read gate hardcoded planner-only and
/// rejected the advocate ("not an assigned skill") even though session
/// construction assigns the skill — so the advocate could never author rich MDX.
#[tokio::test]
async fn skill_read_serves_native_visual_spec_for_advocate_refinement_session() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;

    let task_repo = djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Refinement advocate — revise proposal spec",
            "advocate refinement task",
            "",
            "refinement",
            1,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create refinement task");

    let tmp = crate::test_helpers::test_tempdir("djinn-native-skill-advocate-");

    let ok = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "visual-spec" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        Some(&task.id), // production passes the task UUID, not short_id
        Some("advocate"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("skill_read should succeed for visual-spec in an advocate refinement session");

    assert_eq!(ok.get("name").and_then(|v| v.as_str()), Some("visual-spec"));
    assert!(
        ok.get("content").and_then(|v| v.as_str()).unwrap().len() > 10,
        "native body should be non-trivial"
    );
}

/// `skill_read` rejects `visual-spec` for a non-planner role even when the
/// task is `epic_breakdown`, because native skills are role-gated.
#[tokio::test]
async fn skill_read_rejects_visual_spec_for_non_planner_role() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;

    let task_repo = djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Some worker task",
            "worker task description",
            "",
            "epic_breakdown",
            1,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create task");

    let tmp = crate::test_helpers::test_tempdir("djinn-native-skill-worker-");

    let err = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "visual-spec" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        Some(&task.id), // production passes the task UUID, not short_id
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("skill_read should reject visual-spec for a worker role");

    assert!(
        err.contains("unknown skill"),
        "error should indicate the skill is not assigned, got: {err}"
    );
}

/// Native body is served from the registry — placing a `visual-spec.md` file
/// in the worktree does NOT change the body returned by `skill_read`.
#[tokio::test]
async fn skill_read_native_body_not_from_worktree() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let services = crate::test_helpers::test_services();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;

    let task_repo = djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Decompose proposal",
            "proposal decomposition",
            "",
            "epic_breakdown",
            1,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create epic_breakdown task");

    // Place a fake visual-spec.md in the worktree to try to shadow the native.
    let tmp = crate::test_helpers::test_tempdir("djinn-native-not-worktree-");
    let skills_dir = tmp.path().join(".djinn").join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    fs::write(
        skills_dir.join("visual-spec.md"),
        "---\nname: visual-spec\ndescription: Fake\n---\n\nTampered body.\n",
    )
    .unwrap();

    let ok = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "visual-spec" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        Some(&task.id), // production passes the task UUID, not short_id
        Some("planner"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("skill_read should succeed for visual-spec in an authoring session");

    let content = ok.get("content").and_then(|v| v.as_str()).unwrap();
    assert!(
        !content.contains("Tampered body"),
        "native body must come from the registry, not the worktree"
    );
    assert!(
        content.contains("backtick") || content.contains("angle") || content.contains("MDX"),
        "native body should contain known visual-spec content markers"
    );
}
