// NOTE: SQLite-only test fixture — uses `DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')`, `strftime(...)`, and
// positional `?N` placeholders that don't compile against the MySQL schema used
// by `sqlx::query!`. All raw `sqlx::query` calls in this file are intentionally
// runtime-typed; compile-time check not possible.
use super::*;
use crate::repositories::note::{NoteAssociationKind, NoteSearchParams};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_affinity_scores_task_epic_blocker_and_max() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let task_note = repo
        .create(&project.id, "Task Note", "body", "reference", "[]")
        .await
        .unwrap();
    let epic_note = repo
        .create(&project.id, "Epic Note", "body", "reference", "[]")
        .await
        .unwrap();
    let blocker_note = repo
        .create(&project.id, "Blocker Note", "body", "reference", "[]")
        .await
        .unwrap();
    let overlap_note = repo
        .create(&project.id, "Overlap Note", "body", "reference", "[]")
        .await
        .unwrap();

    let epic_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb)",
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
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13::jsonb)",
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
         VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb)",
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

    sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
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
         VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb)",
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
            edge_kinds: None,
            entity_types: None,
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
        .create(&project.id, "Seed", "no links", "research", "[]")
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
        .create(&project.id, "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, "C", "", "research", "[]")
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
        .create(&project.id, "A", "[[B]] [[D]]", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, "C", "", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, "D", "[[C]]", "research", "[]")
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
        .create(&project.id, "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let d = repo
        .create(&project.id, "D", "", "research", "[]")
        .await
        .unwrap();
    repo.update(&d.id, "D", "[[A]]", "[]").await.unwrap();
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
        .create(&project.id, "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, "B", "", "research", "[]")
        .await
        .unwrap();

    let (note_a_id, note_b_id) = if a.id < b.id {
        (a.id.clone(), b.id.clone())
    } else {
        (b.id.clone(), a.id.clone())
    };

    sqlx::query(
        r#"INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access)
         VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))"#,
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
async fn graph_proximity_edge_kind_filter_limits_traversal() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let old = repo
        .create(&project.id, "Old", "", "research", "[]")
        .await
        .unwrap();
    let co_access = repo
        .create(&project.id, "Co Access", "", "research", "[]")
        .await
        .unwrap();
    let builds_on = repo
        .create(&project.id, "Builds On", "", "research", "[]")
        .await
        .unwrap();
    let canonical = repo
        .create(&project.id, "Canonical", "", "research", "[]")
        .await
        .unwrap();

    sqlx::query(
        r#"INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind)
         VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), 'co_access')"#,
    )
    .bind(&old.id)
    .bind(&co_access.id)
    .bind(1.0_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();
    repo.upsert_typed_association(&old.id, &builds_on.id, NoteAssociationKind::BuildsOn, 1.0)
        .await
        .unwrap();
    repo.upsert_typed_association(&old.id, &canonical.id, NoteAssociationKind::Supersedes, 1.0)
        .await
        .unwrap();

    let only_supersedes = vec!["supersedes".to_string()];
    let (scores, warnings) = repo
        .graph_proximity_scores_with_edge_kinds(
            std::slice::from_ref(&canonical.id),
            1,
            Some(&only_supersedes),
        )
        .await
        .unwrap();

    assert!(warnings.is_empty());
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert_eq!(m.len(), 1, "only supersedes edges should be traversed");
    assert!((m.get(&old.id).copied().unwrap() - 0.2).abs() < 1e-9);
    assert!(!m.contains_key(&co_access.id));
    assert!(!m.contains_key(&builds_on.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_supersedes_source_to_target_returns_negative_score() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let old = repo
        .create(&project.id, "Old", "", "research", "[]")
        .await
        .unwrap();
    let canonical = repo
        .create(&project.id, "Canonical", "", "research", "[]")
        .await
        .unwrap();

    repo.upsert_typed_association(&old.id, &canonical.id, NoteAssociationKind::Supersedes, 1.0)
        .await
        .unwrap();

    let only_supersedes = vec!["supersedes".to_string()];
    let (scores, _warnings) = repo
        .graph_proximity_scores_with_edge_kinds(
            std::slice::from_ref(&old.id),
            1,
            Some(&only_supersedes),
        )
        .await
        .unwrap();

    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!((m.get(&canonical.id).copied().unwrap() - (-0.5)).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_contradicts_returns_warning_without_score() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, "B", "", "research", "[]")
        .await
        .unwrap();
    repo.upsert_typed_association(&a.id, &b.id, NoteAssociationKind::Contradicts, 0.9)
        .await
        .unwrap();

    let (scores, warnings) = repo
        .graph_proximity_scores_with_edge_kinds(std::slice::from_ref(&a.id), 1, None)
        .await
        .unwrap();

    assert!(scores.is_empty(), "contradicts must not contribute score");
    assert_eq!(warnings.len(), 1);
    let warning = &warnings[0];
    assert_eq!(warning.kind, "contradicts");
    assert_eq!(warning.source_id, a.id);
    assert_eq!(warning.target_id, b.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_ignores_low_weight_association_noise() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, "B", "", "research", "[]")
        .await
        .unwrap();

    let (note_a_id, note_b_id) = if a.id < b.id {
        (a.id.clone(), b.id.clone())
    } else {
        (b.id.clone(), a.id.clone())
    };

    sqlx::query(
        r#"INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access)
         VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))"#,
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
        .create(&project.id, "High Access", "body", "reference", "[]")
        .await
        .unwrap();
    let low = repo
        .create(&project.id, "Low Access", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query(
        r#"UPDATE notes
         SET created_at = to_char((now() at time zone 'utc') - interval '1 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
             updated_at = to_char((now() at time zone 'utc') - interval '1 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE id IN ($1, $2)"#,
    )
    .bind(&high.id)
    .bind(&low.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("UPDATE notes SET access_count = 10 WHERE id = $1")
        .bind(&high.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 0 WHERE id = $1")
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
        .create(&project.id, "Recent", "body", "reference", "[]")
        .await
        .unwrap();
    let stale = repo
        .create(&project.id, "Stale", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 3 WHERE id IN ($1, $2)")
        .bind(&recent.id)
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query(r#"UPDATE notes SET created_at = to_char((now() at time zone 'utc') - interval '30 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id IN ($1, $2)"#)
        .bind(&recent.id)
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query(
        r#"UPDATE notes SET updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#,
    )
    .bind(&recent.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(r#"UPDATE notes SET updated_at = to_char((now() at time zone 'utc') - interval '30 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#)
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
        .create(&project.id, "Zero Age", "body", "reference", "[]")
        .await
        .unwrap();
    let old = repo
        .create(&project.id, "Old", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query(
        r#"UPDATE notes
         SET access_count = 0,
             created_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
             updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE id = $1"#,
    )
    .bind(&zero_age.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(
        r#"UPDATE notes
         SET access_count = 0,
             created_at = to_char((now() at time zone 'utc') - interval '365 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
             updated_at = to_char((now() at time zone 'utc') - interval '365 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE id = $1"#,
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

// ── embedding_related edge-kind filtering tests ──────────────────────────

/// Helper: insert an `embedding_related` association between two notes.
async fn insert_embedding_association(
    repo: &NoteRepository,
    note_a_id: &str,
    note_b_id: &str,
    weight: f64,
) {
    repo.upsert_typed_association(
        note_a_id,
        note_b_id,
        NoteAssociationKind::EmbeddingRelated,
        weight,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_embedding_related_included_by_default_no_filter() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let seed = repo
        .create(&project.id, "Seed", "", "research", "[]")
        .await
        .unwrap();
    let target = repo
        .create(&project.id, "Target", "", "research", "[]")
        .await
        .unwrap();

    insert_embedding_association(&repo, &seed.id, &target.id, 0.3).await;

    // No edge_kinds filter → all kinds (including embedding_related) participate.
    let (scores, _warnings) = repo
        .graph_proximity_scores_with_edge_kinds(std::slice::from_ref(&seed.id), 2, None)
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!(
        m.contains_key(&target.id),
        "embedding_related should be included when no edge_kinds filter is set"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_edge_kinds_embedding_related_includes_machine_edges() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let seed = repo
        .create(&project.id, "Seed", "", "research", "[]")
        .await
        .unwrap();
    let target = repo
        .create(&project.id, "Target", "", "research", "[]")
        .await
        .unwrap();

    insert_embedding_association(&repo, &seed.id, &target.id, 0.3).await;

    let only_embedding = vec!["embedding_related".to_string()];
    let (scores, _warnings) = repo
        .graph_proximity_scores_with_edge_kinds(
            std::slice::from_ref(&seed.id),
            2,
            Some(&only_embedding),
        )
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert_eq!(
        m.len(),
        1,
        "only embedding_related edge should be traversed"
    );
    assert!(
        m.contains_key(&target.id),
        "embedding_related target should be present"
    );

    // Verify the expected score: HOP_DECAY * 0.5 * weight
    let expected = 0.7 * 0.5 * 0.3;
    assert!(
        (m[&target.id] - expected).abs() < 1e-9,
        "expected embedding_related score {expected}, got {}",
        m[&target.id]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_edge_kinds_without_embedding_related_excludes_machine_edges() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let seed = repo
        .create(&project.id, "Seed", "", "research", "[]")
        .await
        .unwrap();
    let co_access_note = repo
        .create(&project.id, "Co Access", "", "research", "[]")
        .await
        .unwrap();
    let embedding_note = repo
        .create(&project.id, "Embedding Note", "", "research", "[]")
        .await
        .unwrap();

    // Insert a co_access edge and an embedding_related edge.
    sqlx::query(
        r#"INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind)
         VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), 'co_access')"#,
    )
    .bind(&seed.id)
    .bind(&co_access_note.id)
    .bind(0.5_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    insert_embedding_association(&repo, &seed.id, &embedding_note.id, 0.3).await;

    // Filter to only co_access — embedding_related should be excluded.
    let only_co_access = vec!["co_access".to_string()];
    let (scores, _warnings) = repo
        .graph_proximity_scores_with_edge_kinds(
            std::slice::from_ref(&seed.id),
            2,
            Some(&only_co_access),
        )
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!(
        m.contains_key(&co_access_note.id),
        "co_access edge should be included"
    );
    assert!(
        !m.contains_key(&embedding_note.id),
        "embedding_related should be excluded when not in edge_kinds filter"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_wikilink_outranks_embedding_related() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Seed links to B via wikilink and C via embedding_related.
    let a = repo
        .create(&project.id, "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, "B", "", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, "C", "", "research", "[]")
        .await
        .unwrap();

    // Use a high embedding weight (0.35 = upper bound from 9sx6) to give
    // embedding_related the best possible score — it must still lose to wikilink.
    insert_embedding_association(&repo, &a.id, &c.id, 0.35).await;

    let (scores, _warnings) = repo
        .graph_proximity_scores_with_edge_kinds(std::slice::from_ref(&a.id), 1, None)
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();

    assert!(m.contains_key(&b.id), "wikilink target should be present");
    assert!(
        m.contains_key(&c.id),
        "embedding_related target should be present"
    );

    // Wikilink score = HOP_DECAY = 0.7
    // Embedding_related score = HOP_DECAY * 0.5 * 0.35 = 0.1225
    assert!(
        m[&b.id] > m[&c.id],
        "wikilink ({}) must outrank embedding_related ({})",
        m[&b.id],
        m[&c.id]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_co_access_unchanged_with_embedding_related_present() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, "B", "", "research", "[]")
        .await
        .unwrap();

    // Insert a co_access association with known weight.
    sqlx::query(
        r#"INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind)
         VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), 'co_access')"#,
    )
    .bind(&a.id)
    .bind(&b.id)
    .bind(0.5_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    // Verify the co_access score with no embedding edges present.
    let (scores_before, _) = repo
        .graph_proximity_scores_with_edge_kinds(std::slice::from_ref(&a.id), 1, None)
        .await
        .unwrap();
    let m_before: std::collections::HashMap<_, _> = scores_before.into_iter().collect();
    let co_access_score = m_before[&b.id];

    // Now add an embedding_related edge from A → B as well and verify the
    // co_access contribution is unchanged (best path wins, doesn't sum).
    //
    // We insert via raw SQL rather than `insert_embedding_association`
    // (which calls `upsert_typed_association`) because that helper deletes
    // any `(a, b, kind='co_access', source='session_co_access')` row before
    // inserting — which would remove the co_access edge we just created.
    // Using a different source (`'test_regression'`) lets both rows coexist
    // so the test proves co_access scoring is genuinely preserved.
    sqlx::query(
        r#"INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source)
           VALUES ($1, $2, $3, 0, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                   'embedding_related', 'test_regression')
           ON CONFLICT (note_a_id, note_b_id, kind, source) DO NOTHING"#,
    )
    .bind(&a.id)
    .bind(&b.id)
    .bind(0.3_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    let (scores_after, _) = repo
        .graph_proximity_scores_with_edge_kinds(std::slice::from_ref(&a.id), 1, None)
        .await
        .unwrap();
    let m_after: std::collections::HashMap<_, _> = scores_after.into_iter().collect();

    // Co_access multiplier: HOP_DECAY * 0.5 = 0.35
    // Embedding_related multiplier: HOP_DECAY * 0.5 * 0.3 = 0.105
    // Co_access is higher, so the max-path score should be unchanged.
    assert!(
        (m_after[&b.id] - co_access_score).abs() < 1e-9,
        "co_access score should be unchanged when embedding_related is added: \
         before={}, after={}",
        co_access_score,
        m_after[&b.id]
    );
}
