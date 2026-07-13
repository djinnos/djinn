use super::*;
use crate::repositories::note::NoteSearchParams;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_stats_records_all_note_stages() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        "Lexical Match Note",
        "This note contains the distinctive query token.",
        "reference",
        "[]",
    )
    .await
    .unwrap();

    let timed = repo
        .search_with_stats(NoteSearchParams {
            project_id: &project.id,
            query: "distinctive",
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

    assert!(!timed.rows.is_empty(), "expected at least one note result");
    assert_eq!(timed.summary.result_count, timed.rows.len());
    assert_eq!(
        timed.summary.candidate_count,
        timed.rows.iter().filter(|r| r.entity == "note").count()
    );

    let lexical = timed.lexical_duration.expect("lexical stage ran");
    assert!(
        !lexical.is_zero(),
        "lexical duration must reflect actual work"
    );

    let temporal = timed.temporal_duration.expect("temporal stage ran");
    assert!(
        !temporal.is_zero(),
        "temporal duration must reflect actual work"
    );

    let graph = timed.graph_duration.expect("graph stage ran");
    assert!(!graph.is_zero(), "graph duration must reflect actual work");

    let rrf = timed.rrf_fuse_duration.expect("rrf fuse stage ran");
    assert!(!rrf.is_zero(), "rrf fuse duration must reflect actual work");

    assert!(
        timed.semantic_duration.is_none(),
        "semantic stage is skipped when no semantic candidates are supplied"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_stats_skips_stages_when_no_candidates() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        "Unrelated Note",
        "Content that does not match the query at all.",
        "reference",
        "[]",
    )
    .await
    .unwrap();

    let timed = repo
        .search_with_stats(NoteSearchParams {
            project_id: &project.id,
            query: "xyznonexistent",
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

    assert!(
        timed.rows.is_empty(),
        "expected no results for a miss query"
    );
    assert_eq!(timed.summary.result_count, 0);
    assert_eq!(timed.summary.candidate_count, 0);

    let lexical = timed
        .lexical_duration
        .expect("lexical query still executed");
    assert!(
        !lexical.is_zero(),
        "lexical duration must reflect actual query work"
    );

    assert!(
        timed.semantic_duration.is_none(),
        "semantic stage is skipped when no candidates are supplied"
    );
    assert!(
        timed.temporal_duration.is_none(),
        "temporal stage is skipped when there are no candidates"
    );
    assert!(
        timed.graph_duration.is_none(),
        "graph stage is skipped when there are no candidates"
    );
    assert!(
        timed.rrf_fuse_duration.is_none(),
        "rrf fuse stage is skipped when there are no candidates"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_stats_records_semantic_stage_when_provided() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            "Semantic Candidate Note",
            "Content for semantic scoring.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let semantic_scores = vec![(note.id.clone(), 0.75)];
    let timed = repo
        .search_with_stats(NoteSearchParams {
            project_id: &project.id,
            query: "xyznonexistent",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: Some(semantic_scores),
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .unwrap();

    assert!(
        !timed.rows.is_empty(),
        "semantic candidate should drive a result"
    );

    let semantic = timed.semantic_duration.expect("semantic stage ran");
    assert!(
        !semantic.is_zero(),
        "semantic duration must reflect actual merge work"
    );

    let temporal = timed.temporal_duration.expect("temporal stage ran");
    assert!(
        !temporal.is_zero(),
        "temporal duration must reflect actual work"
    );

    let graph = timed.graph_duration.expect("graph stage ran");
    assert!(!graph.is_zero(), "graph duration must reflect actual work");

    let rrf = timed.rrf_fuse_duration.expect("rrf fuse stage ran");
    assert!(!rrf.is_zero(), "rrf fuse duration must reflect actual work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_stats_preserves_compatibility_rows() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        "Compatibility Note",
        "A note that should appear in both search APIs.",
        "reference",
        "[]",
    )
    .await
    .unwrap();

    let params = NoteSearchParams {
        project_id: &project.id,
        query: "compatibility",
        task_id: None,
        folder: None,
        note_type: None,
        limit: 10,
        semantic_scores: None,
        edge_kinds: None,
        entity_types: None,
    };

    let rows = repo.search(params.clone()).await.unwrap();
    let timed = repo.search_with_stats(params).await.unwrap();

    assert_eq!(rows.len(), timed.rows.len());
    for (left, right) in rows.iter().zip(timed.rows.iter()) {
        assert_eq!(left.entity, right.entity);
        assert_eq!(left.id, right.id);
        assert_eq!(left.title, right.title);
        assert_eq!(left.permalink, right.permalink);
        assert_eq!(left.snippet, right.snippet);
        assert!((left.score - right.score).abs() < f64::EPSILON);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_stats_preserves_ranking() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let high = repo
        .create(
            &project.id,
            "ranktoken alpha",
            "same body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let low = repo
        .create(
            &project.id,
            "ranktoken beta",
            "same body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query!("UPDATE notes SET access_count = 10 WHERE id = $1", high.id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query!("UPDATE notes SET access_count = 0 WHERE id = $1", low.id)
        .execute(db.pool())
        .await
        .unwrap();

    let timed = repo
        .search_with_stats(NoteSearchParams {
            project_id: &project.id,
            query: "ranktoken",
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

    assert_eq!(timed.rows.len(), 2);
    assert_eq!(timed.rows[0].id, high.id);
    assert_eq!(timed.rows[1].id, low.id);
}
