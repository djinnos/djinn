use std::sync::Arc;

use super::*;
use crate::EmbeddedNote;

struct StubEmbeddingProvider {
    value: f32,
    model_version: &'static str,
    fail: bool,
}

#[async_trait::async_trait]
impl NoteEmbeddingProvider for StubEmbeddingProvider {
    async fn embed_note(&self, _text: &str) -> std::result::Result<EmbeddedNote, String> {
        if self.fail {
            Err("model unavailable".to_string())
        } else {
            Ok(EmbeddedNote {
                values: embedding_with_value(self.value),
                model_version: self.model_version.to_string(),
            })
        }
    }

    fn model_version(&self) -> String {
        self.model_version.to_string()
    }
}

fn embedding_with_value(value: f32) -> Vec<f32> {
    vec![value; 768]
}

#[test]
fn worktree_root_infers_task_embedding_branch() {
    let worktree = std::path::Path::new("/tmp/.djinn/worktrees/exen");
    assert_eq!(
        infer_embedding_branch_from_worktree(worktree).as_deref(),
        Some("task/exen")
    );
    assert_eq!(
        infer_embedding_branch_from_worktree(std::path::Path::new("/tmp/.djinn/worktrees/_index")),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_upsert_and_delete_round_trip() {
    let _guard = super::sqlite_vec_test_lock().lock().await;
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
            branch: "main",
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
    let _guard = super::sqlite_vec_test_lock().lock().await;
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
            branch: "main",
        })
        .await
        .unwrap();

    let status = repo.db.sqlite_vec_status().await.unwrap();
    let results = repo
        .query_similar_embeddings(&embedding, EmbeddingQueryContext::default(), 5)
        .await
        .unwrap();
    if status.available {
        assert_eq!(record.extension_state, "ready");
        assert!(!results.is_empty());
    } else {
        assert_eq!(record.extension_state, "pending");
        assert!(results.is_empty());
    }

    crate::database::set_sqlite_vec_disabled_for_tests(false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_lifecycle_tracks_create_update_delete_with_provider() {
    let _guard = super::sqlite_vec_test_lock().lock().await;
    crate::database::set_sqlite_vec_disabled_for_tests(false);

    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx)).with_embedding_provider(Some(Arc::new(
        StubEmbeddingProvider {
            value: 0.2,
            model_version: "model-v1",
            fail: false,
        },
    )));

    let note = repo
        .create(
            &project.id,
            "Lifecycle Note",
            "original body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    // Embeddings are now scheduled on a background tokio task; poll until
    // the create-time embedding lands.
    let expected_hash = crate::note_hash::note_content_hash(
        "title: Lifecycle Note\ntype: reference\ntags: []\n\noriginal body",
    );
    let created = poll_embedding_with_hash(&repo, &note.id, &expected_hash).await;
    assert_eq!(created.model_version, "model-v1");

    let updated = repo
        .update(&note.id, "Lifecycle Note", "updated body", "[]")
        .await
        .unwrap();
    let updated_expected_hash = crate::note_hash::note_content_hash(
        "title: Lifecycle Note\ntype: reference\ntags: []\n\nupdated body",
    );
    let updated_embedding =
        poll_embedding_with_hash(&repo, &updated.id, &updated_expected_hash).await;
    assert_eq!(updated_embedding.model_version, "model-v1");
    assert_ne!(created.content_hash, updated_embedding.content_hash);

    repo.delete(&updated.id).await.unwrap();
    assert!(repo.get_embedding(&updated.id).await.unwrap().is_none());
}

async fn poll_embedding_with_hash(
    repo: &NoteRepository,
    note_id: &str,
    expected_hash: &str,
) -> NoteEmbeddingRecord {
    for _ in 0..200 {
        if let Some(record) = repo.get_embedding(note_id).await.unwrap()
            && record.content_hash == expected_hash
        {
            return record;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("embedding for {note_id} never reached expected hash {expected_hash}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_embedding_promotion_and_discard_update_metadata() {
    let _guard = super::sqlite_vec_test_lock().lock().await;
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note(&project.id, "Task Branch Note", "body", "reference", "[]")
        .await
        .unwrap();

    repo.upsert_embedding(UpsertNoteEmbedding {
        note_id: &note.id,
        content_hash: "task-hash",
        model_version: "model-v1",
        embedding: &embedding_with_value(0.7),
        branch: "task/exen",
    })
    .await
    .unwrap();

    assert_eq!(
        repo.embedding_branch_for_note(&note.id)
            .await
            .unwrap()
            .as_deref(),
        Some("task/exen")
    );
    assert_eq!(
        repo.promote_branch_embeddings("task/exen", "main")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        repo.embedding_branch_for_note(&note.id)
            .await
            .unwrap()
            .as_deref(),
        Some("main")
    );

    repo.upsert_embedding(UpsertNoteEmbedding {
        note_id: &note.id,
        content_hash: "task-hash-2",
        model_version: "model-v1",
        embedding: &embedding_with_value(0.8),
        branch: "task/exen",
    })
    .await
    .unwrap();
    assert_eq!(
        repo.delete_embeddings_for_branch("task/exen")
            .await
            .unwrap(),
        1
    );
    assert!(repo.get_embedding(&note.id).await.unwrap().is_none());
}

// `reindex_repairs_missing_and_stale_embeddings_with_provider` was deleted
// alongside the reindex pipeline. Embedding refresh is now an out-of-band
// concern; if/when a maintenance tool resurfaces, add a focused test for
// it then.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_provider_failure_degrades_without_persisting_embeddings() {
    let _guard = super::sqlite_vec_test_lock().lock().await;
    crate::database::set_sqlite_vec_disabled_for_tests(false);

    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx)).with_embedding_provider(Some(Arc::new(
        StubEmbeddingProvider {
            value: 0.0,
            model_version: "broken-model",
            fail: true,
        },
    )));

    let note = repo
        .create_db_note(
            &project.id,
            "Fallback Only Note",
            "lexical-only body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    assert!(repo.get_embedding(&note.id).await.unwrap().is_none());

    let updated = repo
        .update(
            &note.id,
            "Fallback Only Note",
            "lexical-only body updated",
            "[]",
        )
        .await
        .unwrap();
    assert!(repo.get_embedding(&updated.id).await.unwrap().is_none());
}
