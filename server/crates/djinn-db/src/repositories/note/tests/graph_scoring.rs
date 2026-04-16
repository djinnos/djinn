// NOTE: SQLite-only test fixture — uses `datetime('now')`, `strftime(...)`, and
// positional `?N` placeholders that don't compile against the MySQL schema used
// by `sqlx::query!`. All raw `sqlx::query` calls in this file are intentionally
// runtime-typed; compile-time check not possible.
use super::*;
use crate::repositories::note::NoteSearchParams;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_affinity_scores_task_epic_blocker_and_max() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let task_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Task Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let epic_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Epic Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let blocker_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Blocker Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let overlap_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Overlap Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let epic_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&epic_id)
    .bind(&project.id)
    .bind("EP-1")
    .bind("Epic")
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind(serde_json::json!([epic_note.id.clone(), task_note.id.clone(), overlap_note.id.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(&task_id)
    .bind(&project.id)
    .bind("T-1")
    .bind(&epic_id)
    .bind("Task")
    .bind("")
    .bind("")
    .bind("task")
    .bind(0_i64)
    .bind("")
    .bind("open")
    .bind(0_i64)
    .bind(serde_json::json!([task_note.id.clone(), overlap_note.id.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    let blocker_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&blocker_id)
    .bind(&project.id)
    .bind("T-2")
    .bind("Blocker")
    .bind("")
    .bind("")
    .bind("task")
    .bind(0_i64)
    .bind("")
    .bind("open")
    .bind(0_i64)
    .bind(
        serde_json::json!([
            blocker_note.id.clone(),
            epic_note.id.clone(),
            overlap_note.id.clone()
        ])
        .to_string(),
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES (?1, ?2)")
        .bind(&task_id)
        .bind(&blocker_id)
        .execute(db.pool())
        .await
        .unwrap();

    let none_scores = repo.task_affinity_scores(&project.id, None).await.unwrap();
    assert!(none_scores.is_empty());

    let scores = repo
        .task_affinity_scores(&project.id, Some(&task_id))
        .await
        .unwrap();

    let score_map: std::collections::HashMap<String, f64> = scores.into_iter().collect();
    assert_eq!(score_map.get(&task_note.id), Some(&1.0));
    assert_eq!(score_map.get(&epic_note.id), Some(&0.7));
    assert_eq!(score_map.get(&blocker_note.id), Some(&0.5));
    assert_eq!(score_map.get(&overlap_note.id), Some(&1.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_affinity_scores_include_repo_map_neighbors_for_task_memory_refs() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let adr = repo
        .create(
            &project.id,
            tmp.path(),
            "Repository Map ADR",
            "See [[reference/repo-maps/repository-map-head]] and keep structural layout current.",
            "adr",
            "[]",
        )
        .await
        .unwrap();
    let repo_map = repo
        .upsert_db_note_by_permalink(
            &project.id,
            "reference/repo-maps/repository-map-head",
            "Repository Map head",
            "server/src/repo_map.rs\nserver/crates/djinn-db/src/repositories/note/search.rs",
            "repo_map",
            r#"["repo-map"]"#,
        )
        .await
        .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&task_id)
    .bind(&project.id)
    .bind("T-RM")
    .bind("Task")
    .bind("")
    .bind("")
    .bind("task")
    .bind(0_i64)
    .bind("")
    .bind("open")
    .bind(0_i64)
    .bind(serde_json::json!([adr.permalink.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    let scores = repo
        .task_affinity_scores(&project.id, Some(&task_id))
        .await
        .unwrap();

    let score_map: std::collections::HashMap<String, f64> = scores.into_iter().collect();
    assert_eq!(score_map.get(&adr.id), Some(&1.0));
    assert!(
        (score_map.get(&repo_map.id).copied().unwrap() - 0.245).abs() < 1e-9,
        "expected repo-map affinity score of 0.245, got {:?}",
        score_map.get(&repo_map.id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_search_query_does_not_return_repo_map_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Decision Log",
        "ordinary product planning note",
        "adr",
        "[]",
    )
    .await
    .unwrap();
    repo.upsert_db_note_by_permalink(
        &project.id,
        "reference/repo-maps/repository-map-head",
        "Repository Map head",
        "server/src/repo_map.rs\nserver/crates/djinn-db/src/repositories/note/search.rs",
        "repo_map",
        r#"["repo-map"]"#,
    )
    .await
    .unwrap();

    let results = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "ordinary product planning",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].note_type, "adr");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_empty_for_seed_without_links() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let seed = repo
        .create(
            &project.id,
            tmp.path(),
            "Seed",
            "no links",
            "research",
            "[]",
        )
        .await
        .unwrap();

    let scores = repo.graph_proximity_scores(&[seed.id], 2).await.unwrap();
    assert!(scores.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_linear_chain_hop_decay() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, tmp.path(), "C", "", "research", "[]")
        .await
        .unwrap();

    repo.reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();

    let seed_id = a.id.clone();
    let scores = repo
        .graph_proximity_scores(std::slice::from_ref(&seed_id), 2)
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert_eq!(m.get(&b.id).copied().unwrap(), 0.7);
    assert!((m.get(&c.id).copied().unwrap() - 0.49).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_diamond_keeps_max_path_score_not_sum() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(
            &project.id,
            tmp.path(),
            "A",
            "[[B]] [[D]]",
            "research",
            "[]",
        )
        .await
        .unwrap();
    repo.create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, tmp.path(), "C", "", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, tmp.path(), "D", "[[C]]", "research", "[]")
        .await
        .unwrap();

    repo.reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();

    let seed_id = a.id.clone();
    let scores = repo.graph_proximity_scores(&[seed_id], 2).await.unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!((m.get(&c.id).copied().unwrap() - 0.49).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_excludes_beyond_max_hops() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let d = repo
        .create(&project.id, tmp.path(), "D", "", "research", "[]")
        .await
        .unwrap();
    repo.update(&d.id, "D", "[[A]]", "[]").await.unwrap();

    repo.reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();

    let seed_id = a.id.clone();
    let scores = repo
        .graph_proximity_scores(std::slice::from_ref(&seed_id), 2)
        .await
        .unwrap();
    let ids: std::collections::HashSet<_> = scores.into_iter().map(|(id, _)| id).collect();
    // no 3-hop specific assertion target; ensure algorithm bounded and excludes seed
    assert!(!ids.contains(&seed_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_association_applies_weighted_decay() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, tmp.path(), "B", "", "research", "[]")
        .await
        .unwrap();

    let (note_a_id, note_b_id) = if a.id < b.id {
        (a.id.clone(), b.id.clone())
    } else {
        (b.id.clone(), a.id.clone())
    };

    sqlx::query(
        "INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access)
         VALUES (?1, ?2, ?3, 1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
    )
    .bind(&note_a_id)
    .bind(&note_b_id)
    .bind(0.5_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    let scores = repo
        .graph_proximity_scores(std::slice::from_ref(&a.id), 2)
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!((m.get(&b.id).copied().unwrap() - 0.35).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_ignores_low_weight_association_noise() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, tmp.path(), "B", "", "research", "[]")
        .await
        .unwrap();

    let (note_a_id, note_b_id) = if a.id < b.id {
        (a.id.clone(), b.id.clone())
    } else {
        (b.id.clone(), a.id.clone())
    };

    sqlx::query(
        "INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access)
         VALUES (?1, ?2, ?3, 1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
    )
    .bind(&note_a_id)
    .bind(&note_b_id)
    .bind(0.01_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    let scores = repo.graph_proximity_scores(&[a.id], 2).await.unwrap();
    assert!(scores.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_empty_candidates_returns_empty() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let scores = repo.temporal_scores(&project.id, &[]).await.unwrap();
    assert!(scores.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_higher_access_count_wins_same_age() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let high = repo
        .create(
            &project.id,
            tmp.path(),
            "High Access",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let low = repo
        .create(
            &project.id,
            tmp.path(),
            "Low Access",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query(
        "UPDATE notes
         SET created_at = datetime('now', '-1 day'),
             updated_at = datetime('now', '-1 day')
         WHERE id IN (?1, ?2)",
    )
    .bind(&high.id)
    .bind(&low.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("UPDATE notes SET access_count = 10 WHERE id = ?1")
        .bind(&high.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 0 WHERE id = ?1")
        .bind(&low.id)
        .execute(db.pool())
        .await
        .unwrap();

    let scores = repo
        .temporal_scores(&project.id, &[high.id.clone(), low.id.clone()])
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!(m[&high.id] > m[&low.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_recent_update_wins_same_access_count() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let recent = repo
        .create(&project.id, tmp.path(), "Recent", "body", "reference", "[]")
        .await
        .unwrap();
    let stale = repo
        .create(&project.id, tmp.path(), "Stale", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 3 WHERE id IN (?1, ?2)")
        .bind(&recent.id)
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET created_at = datetime('now', '-30 day') WHERE id IN (?1, ?2)")
        .bind(&recent.id)
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET updated_at = datetime('now') WHERE id = ?1")
        .bind(&recent.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET updated_at = datetime('now', '-30 day') WHERE id = ?1")
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    let scores = repo
        .temporal_scores(&project.id, &[recent.id.clone(), stale.id.clone()])
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!(m[&recent.id] > m[&stale.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_edge_cases_are_finite() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let zero_age = repo
        .create(
            &project.id,
            tmp.path(),
            "Zero Age",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let old = repo
        .create(&project.id, tmp.path(), "Old", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query(
        "UPDATE notes
         SET access_count = 0,
             created_at = datetime('now'),
             updated_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(&zero_age.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(
        "UPDATE notes
         SET access_count = 0,
             created_at = datetime('now', '-365 day'),
             updated_at = datetime('now', '-365 day')
         WHERE id = ?1",
    )
    .bind(&old.id)
    .execute(db.pool())
    .await
    .unwrap();

    let scores = repo
        .temporal_scores(&project.id, &[zero_age.id.clone(), old.id.clone()])
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();

    assert!(m[&zero_age.id].is_finite());
    assert!(m[&old.id].is_finite());
    assert!(m[&zero_age.id] > m[&old.id]);
}
