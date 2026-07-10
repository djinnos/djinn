use super::*;
use crate::database::Database;
use djinn_core::events::EventBus;
use std::collections::HashSet;

async fn make_repo_and_project() -> (NoteRepository, tempfile::TempDir, String) {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let owner = "test";
    let repo_slug = format!("scope-overlap-{id}");
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
    )
    .bind(&id)
    .bind("test")
    .bind(owner)
    .bind(repo_slug)
    .execute(db.pool())
    .await
    .unwrap();
    (NoteRepository::new(db, EventBus::noop()), tmp, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_scoped_by_path_overlap_matches_parent_and_child_scopes_only() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;

    let parent = repo
        .create_with_scope(
            &project_id,
            "Parent Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    let child = repo
        .create_with_scope(
            &project_id,
            "Child Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src/server/state"]"#,
        )
        .await
        .unwrap();
    let unrelated = repo
        .create_with_scope(
            &project_id,
            "Unrelated Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["desktop/src"]"#,
        )
        .await
        .unwrap();
    let global = repo
        .create(&project_id, "Global Note", "content", "pattern", "[]")
        .await
        .unwrap();

    let matches = repo
        .query_scoped_by_path_overlap(
            &project_id,
            &["server/src/server/state/mod.rs".to_string()],
            20,
        )
        .await
        .unwrap();

    let ids: HashSet<String> = matches.into_iter().map(|note| note.id).collect();
    assert!(ids.contains(&parent.id));
    assert!(ids.contains(&child.id));
    assert!(!ids.contains(&unrelated.id));
    assert!(!ids.contains(&global.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_scoped_by_path_overlap_is_noop_for_empty_changed_paths() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    repo.create_with_scope(
        &project_id,
        "Scoped Note",
        "content",
        "pattern",
        None,
        "[]",
        r#"["server/src"]"#,
    )
    .await
    .unwrap();

    let matches = repo
        .query_scoped_by_path_overlap(&project_id, &[], 20)
        .await
        .unwrap();
    assert!(matches.is_empty());
}

async fn set_scope_trace_signals(
    repo: &NoteRepository,
    note_id: &str,
    confidence: f64,
    updated_at: &str,
) {
    sqlx::query("UPDATE notes SET confidence = $1, updated_at = $2 WHERE id = $3")
        .bind(confidence)
        .bind(updated_at)
        .bind(note_id)
        .execute(repo.db.pool())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_by_scope_overlap_trace_candidates_keeps_unfiltered_ordered_candidates() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    let other_project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
    )
    .bind(&other_project_id)
    .bind("other")
    .bind("test")
    .bind(format!("scope-overlap-other-{other_project_id}"))
    .execute(repo.db.pool())
    .await
    .unwrap();

    let task_paths = vec!["server/src/server/state/mod.rs".to_string()];
    let cases = [
        (
            "High Parent",
            0.95,
            "2026-01-07T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
        (
            "Recent Tie",
            0.90,
            "2026-01-08T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Older Tie",
            0.90,
            "2026-01-06T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Below Production Limit",
            0.80,
            "2026-01-05T00:00:00.000Z",
            "[]",
        ),
        (
            "Below Threshold",
            0.40,
            "2026-01-04T00:00:00.000Z",
            r#"["server/src/server/state/mod.rs"]"#,
        ),
        (
            "Capped Out",
            0.30,
            "2026-01-03T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
    ];

    let mut notes = Vec::new();
    for (title, confidence, updated_at, scope_paths) in cases {
        let note = repo
            .create_with_scope(
                &project_id,
                title,
                "content",
                "pattern",
                None,
                "[]",
                scope_paths,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &note.id, confidence, updated_at).await;
        notes.push(note);
    }

    let unrelated = repo
        .create_with_scope(
            &project_id,
            "Unrelated Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["desktop/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &unrelated.id, 0.99, "2026-01-09T00:00:00.000Z").await;

    let archived = repo
        .create_with_scope(
            &project_id,
            "Archived Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &archived.id, 0.98, "2026-01-09T00:00:00.000Z").await;
    sqlx::query("UPDATE notes SET status = 'archived' WHERE id = $1")
        .bind(&archived.id)
        .execute(repo.db.pool())
        .await
        .unwrap();

    let wrong_type = repo
        .create_with_scope(
            &project_id,
            "Wrong Type",
            "content",
            "adr",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &wrong_type.id, 0.97, "2026-01-09T00:00:00.000Z").await;

    let other_project = repo
        .create_with_scope(
            &other_project_id,
            "Other Project",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &other_project.id, 0.96, "2026-01-09T00:00:00.000Z").await;

    let production = repo
        .query_by_scope_overlap(&project_id, &task_paths, &["pattern"], 0.5, 3)
        .await
        .unwrap();
    let production_titles: Vec<_> = production.iter().map(|note| note.title.as_str()).collect();
    assert_eq!(
        production_titles,
        vec!["High Parent", "Recent Tie", "Older Tie"]
    );

    let candidates = repo
        .query_by_scope_overlap_trace_candidates(&project_id, &task_paths, &["pattern"], 5)
        .await
        .unwrap();
    let candidate_titles: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.title.as_str())
        .collect();
    assert_eq!(
        candidate_titles,
        vec![
            "High Parent",
            "Recent Tie",
            "Older Tie",
            "Below Production Limit",
            "Below Threshold",
        ]
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.rank)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(candidates[4].confidence, 0.40);
    assert_eq!(candidates[3].scope_paths, "[]");
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.note_type == "pattern")
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != unrelated.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != archived.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != wrong_type.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != other_project.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != notes[5].id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_by_scope_overlap_trace_candidates_empty_task_paths_matches_global_only() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    let global = repo
        .create_with_scope(
            &project_id,
            "Global Trace Candidate",
            "content",
            "pattern",
            None,
            "[]",
            "[]",
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &global.id, 0.20, "2026-01-01T00:00:00.000Z").await;
    let scoped = repo
        .create_with_scope(
            &project_id,
            "Scoped Trace Candidate",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &scoped.id, 0.99, "2026-01-02T00:00:00.000Z").await;

    let candidates = repo
        .query_by_scope_overlap_trace_candidates(&project_id, &[], &["pattern"], 10)
        .await
        .unwrap();

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, global.id);
    assert_eq!(candidates[0].rank, 1);
}
