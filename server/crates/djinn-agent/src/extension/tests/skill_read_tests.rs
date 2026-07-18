use super::*;
use std::fs;

fn write_checked_skills_manifest(project_root: &std::path::Path) {
    let manifest_path = project_root.join(crate::skills_manifest::DEFAULT_MANIFEST_PATH);
    fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    let manifest = crate::skills_manifest::generate_manifest(project_root, None).unwrap();
    fs::write(
        manifest_path,
        crate::skills_manifest::to_pretty_json(&manifest).unwrap(),
    )
    .unwrap();
}

/// `skill_read` is wired into the native tool dispatch like any other tool:
/// it resolves the named skill from the session worktree and returns the full
/// body, and errors cleanly for an unknown name.
#[tokio::test]
async fn skill_read_returns_content_for_known_skill_and_errors_for_unknown() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();

    // Lay down a worktree with a `.djinn/skills/<name>.md` flat skill file.
    let tmp = crate::test_helpers::test_tempdir("djinn-skill-read-");
    let skills_dir = tmp.path().join(".djinn").join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    fs::write(
        skills_dir.join("rust-safety.md"),
        "---\nname: rust-safety\ndescription: Safe Rust guidelines\n---\n\nAvoid unsafe blocks.\n",
    )
    .unwrap();

    // Known skill → full content returned.
    let ok = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "rust-safety" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        None,
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("skill_read should succeed for a known skill");

    assert_eq!(ok.get("name").and_then(|v| v.as_str()), Some("rust-safety"));
    assert_eq!(
        ok.get("description").and_then(|v| v.as_str()),
        Some("Safe Rust guidelines")
    );
    assert_eq!(
        ok.get("content").and_then(|v| v.as_str()),
        Some("Avoid unsafe blocks.")
    );
    assert_eq!(ok.get("required").and_then(|v| v.as_bool()), Some(false));

    // Unknown skill → clean error (not a panic, not an Ok payload).
    let err = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "does-not-exist" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        None,
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("skill_read should error for an unknown skill");
    assert!(
        err.contains("unknown skill"),
        "error should name the missing skill, got: {err}"
    );

    // Missing `name` arg → clean error.
    let missing = call_tool(
        &state,
        &services,
        "skill_read",
        Some(serde_json::Map::new()),
        tmp.path(),
        None,
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("skill_read should error when `name` is absent");
    assert!(missing.contains("name"), "got: {missing}");
}

#[tokio::test]
async fn skill_read_rejects_tampered_skill_when_manifest_exists() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();

    let tmp = crate::test_helpers::test_tempdir("djinn-skill-read-manifest-");
    let skills_dir = tmp.path().join(".djinn").join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    fs::write(
        skills_dir.join("rust-safety.md"),
        "---\nname: rust-safety\ndescription: Safe Rust guidelines\n---\n\nOriginal body.\n",
    )
    .unwrap();
    write_checked_skills_manifest(tmp.path());

    fs::write(
        skills_dir.join("rust-safety.md"),
        "---\nname: rust-safety\ndescription: Safe Rust guidelines\n---\n\nTampered body.\n",
    )
    .unwrap();

    let err = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "rust-safety" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        None,
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("skill_read must reject a stale/tampered manifested body");

    assert!(
        err.contains("skill_read refused to serve `rust-safety`"),
        "got: {err}"
    );
    assert!(
        err.contains("skills manifest verification failed"),
        "got: {err}"
    );
    assert!(err.contains("content_hash"), "got: {err}");
}

#[tokio::test]
async fn skill_read_serves_directory_skill_references_and_rejects_reference_tamper() {
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    let services = crate::test_helpers::test_services();

    let tmp = crate::test_helpers::test_tempdir("djinn-skill-read-ref-manifest-");
    let skill_dir = tmp.path().join(".djinn").join("skills").join("ref-skill");
    let references_dir = skill_dir.join("references");
    fs::create_dir_all(&references_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: ref-skill\ndescription: Directory skill with references\n---\n\nPrimary body.\n",
    )
    .unwrap();
    fs::write(references_dir.join("guide.md"), "Reference guide body.\n").unwrap();
    write_checked_skills_manifest(tmp.path());

    let ok = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "ref-skill" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        None,
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect("skill_read should serve a verified directory skill");

    let content = ok.get("content").and_then(|v| v.as_str()).unwrap();
    assert!(content.contains("Primary body."));
    assert!(content.contains("## References"));
    assert!(content.contains("### guide.md"));
    assert!(content.contains("Reference guide body."));

    fs::write(
        references_dir.join("guide.md"),
        "Tampered reference body.\n",
    )
    .unwrap();
    let err = call_tool(
        &state,
        &services,
        "skill_read",
        Some(
            serde_json::json!({ "name": "ref-skill" })
                .as_object()
                .unwrap()
                .clone(),
        ),
        tmp.path(),
        None,
        Some("worker"),
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .expect_err("skill_read must reject a stale/tampered reference file");

    assert!(
        err.contains("skill_read refused to serve `ref-skill`"),
        "got: {err}"
    );
    assert!(
        err.contains("skills manifest verification failed"),
        "got: {err}"
    );
    assert!(
        err.contains("content_hash") || err.contains("source file"),
        "got: {err}"
    );
}

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
    let creator = crate::test_helpers::create_test_creator(&db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            djinn_db::EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
    let creator = crate::test_helpers::create_test_creator(&db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            djinn_db::EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
    let creator = crate::test_helpers::create_test_creator(&db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            djinn_db::EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
    let creator = crate::test_helpers::create_test_creator(&db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            djinn_db::EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
    let creator = crate::test_helpers::create_test_creator(&db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            djinn_db::EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
