//! Signal-participation regressions for `search_knowledge_injection_candidates`
//! (proposal `5205`).
//!
//! The proposal requires that lexical, embedding, temporal, graph,
//! task-affinity, and validated-scope retrieval **each** request and retain at
//! most 50 eligible notes before fusion. Graph proximity and task affinity are
//! the two signals that can introduce candidates the text query never found, so
//! they are the two that a naive eligibility filter silently reduces to
//! re-orderers. These tests assert the ordering consequence — that such a note
//! is actually present in the fused list — rather than that a signal was
//! plumbed through.

use super::*;
use crate::repositories::note::KnowledgeInjectionSearchParams;

/// The note types knowledge injection retrieves.
const INJECTED_TYPES: &[&str] = &["pattern", "pitfall", "case"];

/// A note reachable only through the task's `memory_refs` — no lexical overlap
/// with the query and no scope paths at all — must be able to **enter** fusion.
///
/// Before the fix, eligibility was computed over the union of lexical, semantic,
/// and scope candidates only, and every signal was then filtered against it. A
/// task-affinity note outside that union was therefore dropped before fusion,
/// so the task-affinity signal could only reorder notes the text query had
/// already found. This test fails under that behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_affinity_note_enters_fusion_without_lexical_or_scope_match() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    // Found by the lexical query.
    let lexical_note = repo
        .create(
            &project.id,
            "Zephyrium Retrieval",
            "zephyrium retrieval pipeline body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    // Shares no term with the query and carries no scope path. The only route
    // into the candidate set is the task-affinity signal.
    let affinity_note = repo
        .create(
            &project.id,
            "Quokka Ledger",
            "quokka ledger reconciliation body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    let epic_id = make_epic(&db, &project.id).await;
    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .create_fixture_with_ac(
            &epic_id,
            "Zephyrium retrieval work",
            "zephyrium retrieval pipeline",
            "",
            "task",
            1,
            "worker",
            None,
            None,
        )
        .await
        .unwrap();

    sqlx::query("UPDATE tasks SET memory_refs = $1::jsonb WHERE id = $2")
        .bind(serde_json::json!([affinity_note.id.clone()]).to_string())
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let result = repo
        .search_knowledge_injection_candidates(KnowledgeInjectionSearchParams {
            project_id: &project.id,
            query: "zephyrium retrieval pipeline",
            task_id: Some(&task.id),
            note_types: INJECTED_TYPES,
            task_paths: &[],
            top_k: 10,
            semantic_scores: None,
        })
        .await
        .unwrap();

    let ids: Vec<&str> = result
        .candidates
        .iter()
        .map(|candidate| candidate.note.id.as_str())
        .collect();

    assert!(
        ids.contains(&lexical_note.id.as_str()),
        "the lexically matching note must be retrieved; got {ids:?}"
    );
    assert!(
        ids.contains(&affinity_note.id.as_str()),
        "a task-affinity-only note must be able to ENTER fusion, not merely \
         reorder what lexical/semantic/scope already found; got {ids:?}"
    );

    let affinity = result
        .candidates
        .iter()
        .find(|candidate| candidate.note.id == affinity_note.id)
        .expect("affinity note present");
    assert!(
        affinity.signal_ranks.task_affinity.is_some(),
        "the affinity note must carry a task-affinity rank"
    );
    assert!(
        affinity.signal_ranks.lexical.is_none(),
        "the affinity note must NOT have been found lexically — otherwise this \
         test would pass even with the signal neutered"
    );
    assert!(
        affinity.signal_ranks.scope.is_none(),
        "the affinity note has no scope paths, so it cannot be in the scope signal"
    );
}

/// A note whose stored `scope_paths` entry is non-canonical (`./…`) must still
/// match an equivalent task path.
///
/// Stored scope paths are written by many producers over time and are not
/// guaranteed canonical. Before the fix, component splitting was applied to the
/// raw string, so `./src/app` produced `[".", "src", "app"]` and never compared
/// equal to `src/app`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_canonical_stored_scope_path_still_matches_the_task_path() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let canonical = repo
        .create_with_scope(
            &project.id,
            "Canonical Scope",
            "body",
            "pattern",
            None,
            "[]",
            r#"["src/app"]"#,
        )
        .await
        .unwrap();
    let non_canonical = repo
        .create_with_scope(
            &project.id,
            "Non Canonical Scope",
            "body",
            "pattern",
            None,
            "[]",
            r#"["./src/app"]"#,
        )
        .await
        .unwrap();

    // `src\app` — a Windows-separator form. The JSON text is `["src\\app"]`.
    let backslashed = repo
        .create_with_scope(
            &project.id,
            "Backslash Scope",
            "body",
            "pattern",
            None,
            "[]",
            r#"["src\\app"]"#,
        )
        .await
        .unwrap();

    // A non-canonical *task* path must work too — normalization is applied to
    // both sides before the SQL prefilter runs.
    let task_paths = vec!["./src/app/".to_string()];
    let ranked = repo
        .ranked_scope_signal(&project.id, &task_paths, INJECTED_TYPES, 50)
        .await
        .unwrap();

    let ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
    assert!(
        ids.contains(&canonical.id.as_str()),
        "canonical scope path must match; got {ids:?}"
    );
    assert!(
        ids.contains(&non_canonical.id.as_str()),
        "a non-canonical stored scope path must be normalized before comparison; got {ids:?}"
    );
    assert!(
        ids.contains(&backslashed.id.as_str()),
        "a backslash-separated stored scope path must be normalized too; got {ids:?}"
    );
    // All three are exact matches after normalization, so all score 1.0.
    assert_eq!(ranked.len(), 3);
    for (_, score) in &ranked {
        assert_eq!(*score, 1.0);
    }
}

/// The ranked-scope signal orders exact matches above ancestors and excludes
/// notes that are only *string*-prefix related.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ranked_scope_signal_orders_by_distance_and_excludes_false_prefixes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let exact = repo
        .create_with_scope(
            &project.id,
            "Exact",
            "body",
            "pattern",
            None,
            "[]",
            r#"["src/app/handler.rs"]"#,
        )
        .await
        .unwrap();
    let ancestor = repo
        .create_with_scope(
            &project.id,
            "Ancestor",
            "body",
            "pattern",
            None,
            "[]",
            r#"["src/app"]"#,
        )
        .await
        .unwrap();
    let coarse = repo
        .create_with_scope(
            &project.id,
            "Coarse",
            "body",
            "pattern",
            None,
            "[]",
            r#"["src"]"#,
        )
        .await
        .unwrap();
    // `src/ap` is a raw string prefix of `src/app` but not a component ancestor.
    let false_prefix = repo
        .create_with_scope(
            &project.id,
            "False Prefix",
            "body",
            "pattern",
            None,
            "[]",
            r#"["src/ap"]"#,
        )
        .await
        .unwrap();
    // A global note is never a member of this signal.
    let global = repo
        .create(&project.id, "Global", "body", "pattern", "[]")
        .await
        .unwrap();

    let task_paths = vec!["src/app/handler.rs".to_string()];
    let ranked = repo
        .ranked_scope_signal(&project.id, &task_paths, INJECTED_TYPES, 50)
        .await
        .unwrap();

    let ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        vec![exact.id.as_str(), ancestor.id.as_str(), coarse.id.as_str()],
        "exact then nearest then coarse; false prefixes and globals excluded"
    );
    assert_eq!(ranked[0].1, 1.0);
    assert_eq!(ranked[1].1, 0.5);
    assert_eq!(ranked[2].1, 1.0 / 3.0);
    assert!(!ids.contains(&false_prefix.id.as_str()));
    assert!(!ids.contains(&global.id.as_str()));
}
