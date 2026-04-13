use super::*;

fn embedding_with_value(value: f32) -> Vec<f32> {
    vec![value; 768]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_upsert_and_delete_round_trip() {
    crate::database::set_sqlite_vec_disabled_for_tests(false);

    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note(&project.id, "Embedding Seed", "body", "reference", "[]")
        .await
        .unwrap();

    let embedding = embedding_with_value(0.1);
    let record = repo
        .upsert_embedding(UpsertNoteEmbedding {
            note_id: &note.id,
            content_hash: "hash-1",
            model_version: "nomic-embed-text-v1.5",
            embedding: &embedding,
        })
        .await
        .unwrap();

    assert_eq!(record.note_id, note.id);
    assert_eq!(record.content_hash, "hash-1");
    assert_eq!(record.model_version, "nomic-embed-text-v1.5");
    assert_eq!(record.embedding_dim, 768);

    let fetched = repo.get_embedding(&note.id).await.unwrap().unwrap();
    assert_eq!(fetched.note_id, note.id);

    repo.delete_embedding(&note.id).await.unwrap();
    assert!(repo.get_embedding(&note.id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_query_gracefully_returns_empty_when_vec_disabled() {
    crate::database::set_sqlite_vec_disabled_for_tests(true);

    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note(&project.id, "Fallback Embedding", "body", "reference", "[]")
        .await
        .unwrap();

    let embedding = embedding_with_value(0.4);
    let record = repo
        .upsert_embedding(UpsertNoteEmbedding {
            note_id: &note.id,
            content_hash: "hash-2",
            model_version: "nomic-embed-text-v1.5",
            embedding: &embedding,
        })
        .await
        .unwrap();

    let status = repo.db.sqlite_vec_status().await.unwrap();
    let results = repo.query_similar_embeddings(&embedding, 5).await.unwrap();
    if status.available {
        assert_eq!(record.extension_state, "ready");
        assert!(!results.is_empty());
    } else {
        assert_eq!(record.extension_state, "pending");
        assert!(results.is_empty());
    }

    crate::database::set_sqlite_vec_disabled_for_tests(false);
}
