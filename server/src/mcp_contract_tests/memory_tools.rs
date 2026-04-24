//! Remaining memory-tool contract tests (worktree-specific).
//!
//! All non-worktree memory tests migrated to
//! `djinn-control-plane/tests/memory_tools.rs`. Worktree tests need the HTTP
//! harness because they rely on the `x-djinn-worktree-root` request header,
//! which flows through `DjinnMcpServer::dispatch_tool_with_worktree` — a
//! surface the control-plane `call_tool(name, args)` entrypoint does not yet
//! expose.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::events::EventBus;
use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_project_with_dir,
    initialize_mcp_session_with_headers, mcp_call_tool, mcp_call_tool_with_headers,
    workspace_tempdir,
};
use djinn_db::NoteRepository;

/// Removes a directory tree on drop. Tests that write into the
/// synthesized `project_dir(owner, repo)` location (outside the
/// `TempDir` tree returned by the test harness) use this so the files
/// don't accumulate under `~/.djinn/projects`.
struct DirCleanup(PathBuf);

impl Drop for DirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn mcp_proposal_pipeline_regression_recovers_worktree_draft_survives_sync_and_lists() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let canonical_path =
        djinn_core::paths::project_dir(&proj.github_owner, &proj.github_repo);
    std::fs::create_dir_all(&canonical_path).expect("create canonical project dir");
    let _canonical_guard = DirCleanup(canonical_path.clone());
    let project_slug = proj.slug();
    let worktree = canonical_path.join(".djinn/worktrees/proposal-pipeline-regression");
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
            "project": project_slug,
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
            "project": project_slug,
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

    let worktree_repo = NoteRepository::new(db.clone(), EventBus::noop())
        .with_worktree_root(Some(worktree.clone()));
    let synced = worktree_repo
        .sync_worktree_notes_to_canonical(&proj.id, &canonical_path, &worktree)
        .await
        .expect("sync worktree notes to canonical memory");
    assert_eq!(synced, 1);

    let canonical_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let canonical = canonical_repo
        .get_by_permalink(&proj.id, "decisions/proposed/pipeline-regression-draft")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical.note_type, "proposed_adr");
    assert!(Path::new(&canonical.file_path).exists());

    let proposals = mcp_call_tool(
        &app,
        &session_id,
        "propose_adr_list",
        json!({"project": project_slug}),
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
async fn mcp_memory_write_edit_delete_use_worktree_root_header_for_file_ops() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let canonical_path =
        djinn_core::paths::project_dir(&proj.github_owner, &proj.github_repo);
    let _canonical_guard = DirCleanup(canonical_path.clone());
    let project_slug = proj.slug();
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
        json!({"project": project_slug, "title": "Worktree Note", "content": "alpha", "type": "reference"}),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&proj.id, created["permalink"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();

    let canonical_note_path = Path::new(&note.file_path).to_path_buf();
    let worktree_path = worktree.path().join(".djinn/reference/worktree-note.md");
    assert_eq!(
        canonical_note_path,
        canonical_path.join(".djinn/reference/worktree-note.md")
    );
    assert!(
        worktree_path.exists(),
        "note file should be created in worktree .djinn"
    );
    assert!(
        !canonical_note_path.exists(),
        "canonical .djinn path should remain untouched during worktree session writes"
    );

    let edited = mcp_call_tool_with_headers(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project_slug, "identifier": created["permalink"], "operation": "append", "content": "beta"}),
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
        json!({"project": project_slug, "identifier": created["permalink"]}),
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
    let canonical_path =
        djinn_core::paths::project_dir(&proj.github_owner, &proj.github_repo);
    let _canonical_guard = DirCleanup(canonical_path.clone());
    let project_slug = proj.slug();
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
        json!({"project": project_slug, "title": "Project Brief", "content": "alpha", "type": "brief"}),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;
    assert_eq!(created["permalink"], "brief");

    let edited = mcp_call_tool_with_headers(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project_slug, "identifier": "brief", "operation": "append", "content": "beta"}),
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;
    assert!(edited["content"].as_str().unwrap().contains("beta"));

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&proj.id, "brief")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(note.permalink, "brief");
    assert_eq!(note.note_type, "brief");
    let canonical_note_path = Path::new(&note.file_path).to_path_buf();
    let worktree_path = worktree.path().join(".djinn/brief.md");
    assert_eq!(canonical_note_path, canonical_path.join(".djinn/brief.md"));
    assert!(
        canonical_note_path.exists(),
        "singleton canonical file should exist"
    );
    assert!(
        worktree_path.exists(),
        "singleton worktree mirror should exist"
    );

    let canonical_contents =
        std::fs::read_to_string(&canonical_note_path).expect("read canonical brief");
    let worktree_contents =
        std::fs::read_to_string(&worktree_path).expect("read worktree brief");
    assert!(canonical_contents.contains("alpha"));
    assert!(canonical_contents.contains("beta"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(&proj.id, "reference/project-brief")
            .await
            .unwrap()
            .is_none(),
        "singleton write should not retarget to reference note"
    );
    assert!(
        !canonical_path
            .join(".djinn/reference/project-brief.md")
            .exists(),
        "singleton write should not create duplicate typed note"
    );
}

#[tokio::test]
async fn mcp_current_requirement_edits_use_canonical_project_root_and_mirror_worktree() {
    let db = create_test_db();
    let (proj, _dir) = create_test_project_with_dir(&db).await;
    let canonical_path =
        djinn_core::paths::project_dir(&proj.github_owner, &proj.github_repo);
    std::fs::create_dir_all(&canonical_path).expect("create canonical project dir");
    let _canonical_guard = DirCleanup(canonical_path.clone());
    let project_slug = proj.slug();
    let worktree = workspace_tempdir("mcp-current-requirement-worktree-");
    std::fs::create_dir_all(worktree.path().join(".git")).expect("create synthetic .git dir");
    let app = create_test_app_with_db(db.clone());
    let worktree_header = worktree.path().to_string_lossy().to_string();
    let session_id = initialize_mcp_session_with_headers(
        &app,
        &[("x-djinn-worktree-root", &worktree_header)],
    )
    .await;

    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    note_repo
        .create(
            &proj.id,
            &canonical_path,
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
            "project": project_slug,
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
        .get_by_permalink(&proj.id, "requirements/v1-requirements")
        .await
        .unwrap()
        .unwrap();

    let canonical_note_path = Path::new(&note.file_path).to_path_buf();
    let worktree_path = worktree.path().join(".djinn/requirements/v1-requirements.md");
    assert_eq!(
        canonical_note_path,
        canonical_path.join(".djinn/requirements/v1-requirements.md")
    );
    assert!(canonical_note_path.exists(), "current-note canonical file should exist");
    assert!(worktree_path.exists(), "current-note worktree mirror should exist");

    let canonical_contents =
        std::fs::read_to_string(&canonical_note_path).expect("read canonical requirements");
    let worktree_contents =
        std::fs::read_to_string(&worktree_path).expect("read worktree requirements");
    assert!(canonical_contents.contains("[[reference/cognitive-memory-scope]]"));
    assert_eq!(canonical_contents, worktree_contents);

    assert!(
        note_repo
            .get_by_permalink(&proj.id, "reference/v1-requirements")
            .await
            .unwrap()
            .is_none(),
        "current-note edit should not retarget to reference note"
    );
    assert!(
        !canonical_path
            .join(".djinn/reference/v1-requirements.md")
            .exists(),
        "current-note edit should not create duplicate typed note"
    );
}
