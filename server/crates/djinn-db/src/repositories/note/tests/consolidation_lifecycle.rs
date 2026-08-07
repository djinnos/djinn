//! Integration and unit coverage for proposal `t5rn`.
//!
//! Every assertion here reads a **side effect** — an actual row, an actual
//! status, an actual edge endpoint set, or a statement count taken from
//! `sqlx`'s own query records — never a flag, a label, or the fact that a
//! function was called.

use std::collections::BTreeSet;
use std::collections::HashMap;

use super::*;
use crate::error::DbResult;
use crate::query_observer::{finish_query_capture, start_query_capture};

// ── fixture helpers ──────────────────────────────────────────────────────────

/// Create a durable extracted note through the **real** extraction mutation:
/// the same `mutate_with_revision` command `djinn-slot/src/llm_extraction.rs`
/// issues, with system/`extraction` attribution and trusted session provenance.
async fn extraction_create(
    repo: &NoteRepository,
    project_id: &str,
    session_id: &str,
    title: &str,
    content: &str,
    note_type: &str,
) -> DbResult<djinn_memory::Note> {
    let result = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: Some(uuid::Uuid::now_v7().to_string()),
            event_kind: NoteRevisionEventKind::Created,
            desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                title: title.to_owned(),
                permalink: permalink_for(note_type, title),
                content: content.to_owned(),
                note_type: note_type.to_owned(),
                folder: folder_for_type(note_type).to_owned(),
                status: "active".to_owned(),
                tags: "[]".to_owned(),
                retrieval_anchor: None,
                scope_paths: "[]".to_owned(),
                confidence: 0.5,
            }),
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
            provenance: TrustedNoteRevisionProvenance::new(
                Some(session_id.to_owned()),
                Some("task-fixture".to_owned()),
                None,
            )
            .unwrap(),
            reason: NoteRevisionReason::new("created note from completed session extraction")
                .unwrap(),
        })
        .await?;
    Ok(result.note.expect("create returns a note"))
}

/// Update an existing durable note through the real extraction mutation.
async fn extraction_update(
    repo: &NoteRepository,
    project_id: &str,
    session_id: &str,
    note: &djinn_memory::Note,
    content: &str,
) -> DbResult<NoteRevisionMutationResult> {
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project_id.to_owned(),
        note_id: Some(note.id.clone()),
        event_kind: NoteRevisionEventKind::Updated,
        desired: NoteRevisionDesiredState::Existing {
            content: content.to_owned(),
            confidence: note.confidence,
        },
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
        provenance: TrustedNoteRevisionProvenance::new(
            Some(session_id.to_owned()),
            Some("task-fixture".to_owned()),
            None,
        )
        .unwrap(),
        reason: NoteRevisionReason::new("updated note from completed session extraction").unwrap(),
    })
    .await
}

async fn provenance_pairs(db: &Database) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT note_id, session_id FROM consolidated_note_provenance \
         ORDER BY note_id ASC, session_id ASC",
    )
    .fetch_all(db.pool())
    .await
    .unwrap()
}

async fn note_status(db: &Database, note_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT status FROM notes WHERE id = $1")
        .bind(note_id)
        .fetch_optional(db.pool())
        .await
        .unwrap()
}

async fn revision_count(db: &Database, note_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM note_revision_events WHERE note_id = $1",
    )
    .bind(note_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn supersedes_edge_count(db: &Database) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM note_associations WHERE kind = 'supersedes'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn canonical_note_count(db: &Database) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM note_revision_events \
         WHERE event_kind = 'created' AND actor_kind = 'system' AND subsystem = 'consolidation'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap()
}

fn synthetic_note(index: usize, permalink_prefix: &str) -> ConsolidationNote {
    ConsolidationNote {
        id: format!("00000000-0000-7000-8000-{index:012}"),
        project_id: "project".to_owned(),
        permalink: format!("{permalink_prefix}/{index:04}"),
        title: format!("Note {index}"),
        note_type: "pattern".to_owned(),
        folder: "patterns".to_owned(),
        scope_paths: "[]".to_owned(),
        content: format!("body {index}"),
        abstract_: None,
        overview: None,
        confidence: 0.5,
    }
}

fn score(seed: &ConsolidationNote, candidate: &ConsolidationNote, value: f64) -> DirectedScoreRow {
    DirectedScoreRow {
        seed_id: seed.id.clone(),
        candidate_id: candidate.id.clone(),
        score: value,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1 — extraction seeds session provenance atomically
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extraction_create_commits_note_revision_and_session_provenance_together() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/extract").await;

    // Precondition: the only production writer of `consolidated_note_provenance`
    // used to be canonical consolidation itself, so the runner had no entry.
    assert!(
        consolidation
            .list_sessions_with_provenance()
            .await
            .unwrap()
            .is_empty()
    );

    let note = extraction_create(
        &repo,
        &project.id,
        &session,
        "Retry Storm",
        "Retry storms amplify duplicate recovery work.",
        "pattern",
    )
    .await
    .unwrap();

    assert_eq!(note_status(&db, &note.id).await.as_deref(), Some("active"));
    assert_eq!(revision_count(&db, &note.id).await, 1);
    assert_eq!(
        provenance_pairs(&db).await,
        vec![(note.id.clone(), session.clone())]
    );

    // No canonical writer ran, yet the session is now discoverable.
    assert_eq!(canonical_note_count(&db).await, 0);
    assert_eq!(
        consolidation.list_sessions_with_provenance().await.unwrap(),
        vec![session]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extraction_create_provenance_failure_rolls_back_note_and_revision() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let session = make_session(&db, &project.id, None, "worker/extract").await;

    repo.set_extraction_provenance_failure_for_test(true);
    let error = extraction_create(
        &repo,
        &project.id,
        &session,
        "Retry Storm",
        "Retry storms amplify duplicate recovery work.",
        "pattern",
    )
    .await
    .expect_err("provenance failure must fail the whole mutation");
    assert!(
        error.to_string().contains("session provenance"),
        "unexpected error: {error}"
    );
    repo.set_extraction_provenance_failure_for_test(false);

    // Neither an unpaired note mutation nor an orphan provenance row survives.
    let notes: i64 = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE project_id = $1")
        .bind(&project.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(notes, 0);
    let revisions: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1")
            .bind(&project.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(revisions, 0);
    assert!(provenance_pairs(&db).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extraction_update_provenance_failure_rolls_back_update_and_revision() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let first_session = make_session(&db, &project.id, None, "worker/one").await;
    let second_session = make_session(&db, &project.id, None, "worker/two").await;

    let note = extraction_create(
        &repo,
        &project.id,
        &first_session,
        "Retry Storm",
        "original body about retry storms",
        "pattern",
    )
    .await
    .unwrap();
    let baseline_revisions = revision_count(&db, &note.id).await;

    repo.set_extraction_provenance_failure_for_test(true);
    let error = extraction_update(
        &repo,
        &project.id,
        &second_session,
        &note,
        "rewritten body about retry storms",
    )
    .await
    .expect_err("provenance failure must fail the update");
    assert!(
        error.to_string().contains("session provenance"),
        "unexpected error: {error}"
    );
    repo.set_extraction_provenance_failure_for_test(false);

    let persisted: String = sqlx::query_scalar::<_, String>("SELECT content FROM notes WHERE id = $1")
        .bind(&note.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(persisted, "original body about retry storms");
    assert_eq!(revision_count(&db, &note.id).await, baseline_revisions);
    assert_eq!(
        provenance_pairs(&db).await,
        vec![(note.id.clone(), first_session.clone())],
        "the failed second-session update must not have seeded provenance"
    );

    // The same update succeeds once the injected failure is cleared, adding the
    // second session membership on the `(note_id, session_id)` key.
    extraction_update(
        &repo,
        &project.id,
        &second_session,
        &note,
        "rewritten body about retry storms",
    )
    .await
    .unwrap();
    let mut expected = vec![
        (note.id.clone(), first_session),
        (note.id.clone(), second_session),
    ];
    expected.sort();
    assert_eq!(provenance_pairs(&db).await, expected);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC2 — resumable, trusted-only backfill
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_backfill_seeds_only_trusted_same_project_eligible_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let other_project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/one").await;
    let foreign_session = make_session(&db, &other_project.id, None, "worker/foreign").await;

    // (a) trusted, same project, eligible → seeded.
    let seedable = extraction_create(
        &repo,
        &project.id,
        &session,
        "Seedable Pattern",
        "seedable pattern body",
        "pattern",
    )
    .await
    .unwrap();

    // (b) trusted session that belongs to a *different* project → mismatch.
    let mismatched = extraction_create(
        &repo,
        &project.id,
        &foreign_session,
        "Mismatched Pattern",
        "mismatched pattern body",
        "pattern",
    )
    .await
    .unwrap();

    // (c) no trusted session provenance at all → reported, never guessed.
    let unprovenanced = repo
        .create_db_note(
            &project.id,
            "Unprovenanced Pattern",
            "unprovenanced pattern body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    // (d) a canonical, identified by its immutable consolidation creation
    //     revision → excluded from the destination eligibility key.
    let canonical = repo
        .create_db_note(
            &project.id,
            "Canonical Pattern",
            "canonical pattern body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, \
         content_after, confidence_after, actor_kind, subsystem, session_id, reason) \
         VALUES ($1, $2, $3, 1, 'created', 'canonical pattern body', 0.5, 'system', \
         'consolidation', $4, 'fixture canonical creation')",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&project.id)
    .bind(&canonical.id)
    .bind(&session)
    .execute(db.pool())
    .await
    .unwrap();

    // Simulate the pre-migration corpus: drop every row the live writer seeded
    // so the backfill has to rebuild them from immutable revision provenance.
    sqlx::query("DELETE FROM consolidated_note_provenance")
        .execute(db.pool())
        .await
        .unwrap();

    let report = consolidation
        .run_provenance_backfill("t5rn-fixture", None)
        .await
        .unwrap();

    assert!(report.completed);
    assert_eq!(report.seeded_provenance_row_count, 1);
    assert_eq!(report.skipped_without_provenance, 1);
    assert_eq!(report.skipped_project_mismatch, 1);
    assert_eq!(report.skipped_canonical_attribution, 1);
    assert_eq!(report.scanned_note_count, 4);

    // The rows themselves, not the counters, are the witness.
    assert_eq!(
        provenance_pairs(&db).await,
        vec![(seedable.id.clone(), session.clone())]
    );
    for excluded in [&mismatched.id, &unprovenanced.id, &canonical.id] {
        assert!(
            !provenance_pairs(&db)
                .await
                .iter()
                .any(|(note_id, _)| note_id == excluded),
            "note {excluded} must not be seeded"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_backfill_resumes_from_its_watermark_and_is_idempotent() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/one").await;

    let mut created = Vec::new();
    for index in 0..4 {
        created.push(
            extraction_create(
                &repo,
                &project.id,
                &session,
                &format!("Resumable Pattern {index}"),
                &format!("resumable pattern body {index}"),
                "pattern",
            )
            .await
            .unwrap(),
        );
    }
    created.sort_by(|left, right| left.id.cmp(&right.id));
    sqlx::query("DELETE FROM consolidated_note_provenance")
        .execute(db.pool())
        .await
        .unwrap();

    // Persist a mid-corpus watermark, exactly as an interrupted run would have.
    sqlx::query(
        "INSERT INTO consolidation_provenance_backfill_state (scope_key, last_note_id) \
         VALUES ($1, $2)",
    )
    .bind("t5rn-resume")
    .bind(&created[1].id)
    .execute(db.pool())
    .await
    .unwrap();

    let resumed = consolidation
        .run_provenance_backfill("t5rn-resume", None)
        .await
        .unwrap();
    assert!(resumed.completed);
    // Only the two notes *after* the watermark are rescanned and seeded.
    assert_eq!(resumed.scanned_note_count, 2);
    assert_eq!(resumed.seeded_provenance_row_count, 2);
    let seeded_after_resume = provenance_pairs(&db)
        .await
        .into_iter()
        .map(|(note_id, _)| note_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        seeded_after_resume,
        BTreeSet::from([created[2].id.clone(), created[3].id.clone()]),
        "a resumed run must not rescan completed batches"
    );

    // Re-invoking the completed scope neither rescans nor writes.
    let repeat = consolidation
        .run_provenance_backfill("t5rn-resume", None)
        .await
        .unwrap();
    assert_eq!(repeat.scanned_note_count, resumed.scanned_note_count);
    assert_eq!(
        repeat.seeded_provenance_row_count,
        resumed.seeded_provenance_row_count
    );

    // A full run under a fresh scope key seeds the two remaining notes and
    // re-seeds nothing: the insert is idempotent on `(note_id, session_id)`.
    let full = consolidation
        .run_provenance_backfill("t5rn-full", None)
        .await
        .unwrap();
    assert_eq!(full.scanned_note_count, 4);
    assert_eq!(full.seeded_provenance_row_count, 2);
    assert_eq!(provenance_pairs(&db).await.len(), 4);

    let again = consolidation
        .run_provenance_backfill("t5rn-full-again", None)
        .await
        .unwrap();
    assert_eq!(again.seeded_provenance_row_count, 0);
    assert_eq!(provenance_pairs(&db).await.len(), 4);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3 — bounded complete-link clustering
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn bridge_chain_never_merges_through_a_shared_neighbor() {
    // A—B—C—D—E: consecutive pairs are similar, non-consecutive pairs are not.
    // Single-link would collapse the whole chain into one giant cluster.
    let notes = (0..5)
        .map(|index| synthetic_note(index, "patterns/chain"))
        .collect::<Vec<_>>();
    let mut scores = Vec::new();
    for index in 0..4 {
        scores.push(score(&notes[index], &notes[index + 1], 0.9));
        scores.push(score(&notes[index + 1], &notes[index], 0.9));
    }

    let (clusters, comparisons) =
        build_bounded_clusters(&notes, &scores, CONSOLIDATION_DEFAULT_SCORE_THRESHOLD);

    assert!(
        clusters.is_empty(),
        "complete-link admission must not form a transitive cluster from a bridge chain: {clusters:?}"
    );
    assert!(comparisons > 0, "the fixture must actually exercise admission");
    assert!(comparisons <= CONSOLIDATION_MAX_ADMISSION_COMPARISONS);
}

#[test]
fn dense_component_splits_into_bounded_groups_of_at_most_eight() {
    let notes = (0..CONSOLIDATION_MAX_PARTITION_INPUTS)
        .map(|index| synthetic_note(index, "patterns/dense"))
        .collect::<Vec<_>>();
    // Fully connected in both directions, well above the threshold.
    let mut scores = Vec::with_capacity(notes.len() * (notes.len() - 1));
    for seed in &notes {
        for candidate in &notes {
            if seed.id != candidate.id {
                scores.push(score(seed, candidate, 0.9));
            }
        }
    }

    let (clusters, comparisons) =
        build_bounded_clusters(&notes, &scores, CONSOLIDATION_DEFAULT_SCORE_THRESHOLD);

    assert_eq!(
        clusters.len(),
        CONSOLIDATION_MAX_PARTITION_INPUTS / CONSOLIDATION_MAX_CLUSTER_SOURCES
    );
    for cluster in &clusters {
        assert_eq!(cluster.source_note_ids.len(), CONSOLIDATION_MAX_CLUSTER_SOURCES);
        assert!(cluster.source_note_ids.len() >= CONSOLIDATION_MIN_CLUSTER_SOURCES);
    }
    assert!(
        clusters_are_disjoint(&clusters),
        "a source may appear in at most one cluster per run"
    );
    assert_eq!(
        cluster_source_id_set(&clusters).len(),
        CONSOLIDATION_MAX_PARTITION_INPUTS
    );

    // Each of the 25 groups of 8 evaluates 1+2+…+7 = 28 admission comparisons,
    // and every unordered pair is evaluated at most once.
    assert_eq!(comparisons, 25 * 28);
    assert!(comparisons <= CONSOLIDATION_MAX_ADMISSION_COMPARISONS);
}

#[test]
fn mutually_unrelated_inputs_stay_within_the_unordered_comparison_bound() {
    // The pathological shape: 200 inputs, no pair above threshold, so every
    // seed scans every later candidate. This is the exact C(200, 2) worst case.
    let notes = (0..CONSOLIDATION_MAX_PARTITION_INPUTS)
        .map(|index| synthetic_note(index, "patterns/sparse"))
        .collect::<Vec<_>>();

    let (clusters, comparisons) =
        build_bounded_clusters(&notes, &[], CONSOLIDATION_DEFAULT_SCORE_THRESHOLD);

    assert!(clusters.is_empty());
    assert_eq!(comparisons, CONSOLIDATION_MAX_ADMISSION_COMPARISONS);
}

#[test]
fn a_cluster_stops_growing_at_eight_and_leaves_the_rest_to_later_seeds() {
    let notes = (0..11)
        .map(|index| synthetic_note(index, "patterns/nine"))
        .collect::<Vec<_>>();
    let mut scores = Vec::new();
    for seed in &notes {
        for candidate in &notes {
            if seed.id != candidate.id {
                scores.push(score(seed, candidate, 0.9));
            }
        }
    }

    let (clusters, _) =
        build_bounded_clusters(&notes, &scores, CONSOLIDATION_DEFAULT_SCORE_THRESHOLD);

    assert_eq!(clusters.len(), 2);
    assert_eq!(clusters[0].source_note_ids.len(), 8);
    assert_eq!(clusters[1].source_note_ids.len(), 3);
    assert!(clusters_are_disjoint(&clusters));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_clustering_caps_inputs_and_issues_one_set_based_score_query() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/dense").await;

    // 205 eligible sources: five more than the per-partition cap.
    let total_inputs = CONSOLIDATION_MAX_PARTITION_INPUTS + 5;
    for index in 0..total_inputs {
        let note = repo
            .create_db_note(
                &project.id,
                &format!("Retry Storm Variant {index}"),
                "Retry storms amplify duplicate recovery work during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        consolidation.add_provenance(&note.id, &session).await.unwrap();
    }

    let partition = ConsolidationPartitionKey {
        project_id: project.id.clone(),
        session_id: session.clone(),
        note_type: "pattern".to_owned(),
    };

    // The threshold is deliberately relaxed here so real `ts_rank` scores from
    // this fixture's short bodies clear it; the *bound* under test is the query
    // count and the input cap, not the tuned production threshold.
    start_query_capture();
    let outcome = consolidation
        .bounded_clusters_for_partition(&partition, 1e-6)
        .await
        .unwrap();
    let trace = finish_query_capture();

    assert_eq!(outcome.input_count, CONSOLIDATION_MAX_PARTITION_INPUTS);
    assert_eq!(outcome.overflow_count, 5);
    assert!(
        outcome.score_matrix_rows > 0,
        "the fixture must produce a nonempty score matrix"
    );
    assert!(
        outcome.admission_comparisons <= CONSOLIDATION_MAX_ADMISSION_COMPARISONS,
        "admission comparisons {} exceeded the 200-input bound",
        outcome.admission_comparisons
    );
    for cluster in &outcome.clusters {
        assert!(cluster.source_note_ids.len() >= CONSOLIDATION_MIN_CLUSTER_SOURCES);
        assert!(cluster.source_note_ids.len() <= CONSOLIDATION_MAX_CLUSTER_SOURCES);
    }
    assert!(clusters_are_disjoint(&outcome.clusters));

    // Instrumentation comes from `sqlx`'s own query records, so a per-note or
    // per-pair loop would show up here no matter how the repository is written.
    // A lower bound guards against a silently uninstalled observer.
    assert!(
        trace.round_trips() >= 2,
        "query instrumentation captured nothing; trace was:\n{}",
        trace.rendered()
    );
    assert_eq!(
        trace.matching("scoped c ON c.id <> s.seed_id"),
        1,
        "expected exactly one set-based score-matrix query, got:\n{}",
        trace.rendered()
    );
    assert!(
        trace.round_trips() <= 4,
        "clustering must not fan out per note or per pair; observed {} round trips for {} inputs:\n{}",
        trace.round_trips(),
        outcome.input_count,
        trace.rendered()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4 / AC5 / AC7 — the atomic canonical transaction
// ═══════════════════════════════════════════════════════════════════════════

struct CanonicalFixture {
    session_id: String,
    partition: ConsolidationPartitionKey,
    sources: Vec<djinn_memory::Note>,
}

async fn canonical_fixture(
    repo: &NoteRepository,
    consolidation: &NoteConsolidationRepository,
    project_id: &str,
    session_id: &str,
    count: usize,
) -> CanonicalFixture {
    let mut sources = Vec::new();
    for index in 0..count {
        let note = repo
            .create_db_note(
                project_id,
                &format!("Retry Storm Source {index}"),
                &format!("Retry storm source body {index}."),
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        consolidation
            .add_provenance(&note.id, session_id)
            .await
            .unwrap();
        sources.push(note);
    }
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    CanonicalFixture {
        session_id: session_id.to_owned(),
        partition: ConsolidationPartitionKey {
            project_id: project_id.to_owned(),
            session_id: session_id.to_owned(),
            note_type: "pattern".to_owned(),
        },
        sources,
    }
}

impl CanonicalFixture {
    fn source_ids(&self) -> Vec<String> {
        self.sources.iter().map(|note| note.id.clone()).collect()
    }

    fn request<'a>(&'a self, source_ids: &'a [String]) -> CommitConsolidationCanonical<'a> {
        CommitConsolidationCanonical {
            partition: &self.partition,
            source_note_ids: source_ids,
            title: "Canonical pattern: Retry Storm",
            content: "# Canonical pattern: Retry Storm\n\nConsolidated retry-storm guidance.",
            abstract_: Some("Retry storms amplify duplicate work."),
            overview: Some("Prefer idempotent recovery with backoff."),
            confidence: 0.7,
            scope_paths: "[]",
            reason: NoteRevisionReason::new("consolidation:create canonical cluster note").unwrap(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_transaction_commits_every_effect_in_one_transaction() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/canonical").await;
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();

    let outcome = consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap();
    let ConsolidationCommitOutcome::Committed(committed) = outcome else {
        panic!("expected a committed canonical, got {outcome:?}");
    };

    // 1. an active canonical note exists
    assert_eq!(
        note_status(&db, &committed.canonical_note_id).await.as_deref(),
        Some("active")
    );
    // 2. its immutable creation revision carries consolidation attribution and
    //    the unique attempt identity
    let (subsystem, attempt): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT subsystem, consolidation_attempt_id FROM note_revision_events \
         WHERE id = $1",
    )
    .bind(&committed.creation_revision_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(subsystem.as_deref(), Some("consolidation"));
    assert_eq!(attempt.as_deref(), Some(committed.consolidation_attempt_id.as_str()));
    assert!(
        consolidation
            .is_consolidation_canonical(&committed.canonical_note_id)
            .await
            .unwrap()
    );
    // 3. the reserved display tag
    let tags: String = sqlx::query_scalar::<_, String>("SELECT tags::text FROM notes WHERE id = $1")
        .bind(&committed.canonical_note_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(tags.contains(CONSOLIDATION_CANONICAL_TAG), "tags were {tags}");
    // 4. summary fields
    let (abstract_, overview): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT abstract, overview FROM notes WHERE id = $1")
            .bind(&committed.canonical_note_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(abstract_.as_deref(), Some("Retry storms amplify duplicate work."));
    assert_eq!(
        overview.as_deref(),
        Some("Prefer idempotent recovery with backoff.")
    );
    // 5. canonical provenance carries the selected session
    assert!(
        committed
            .canonical_provenance_session_ids
            .contains(&fixture.session_id)
    );
    // 6. one supersedes edge per source, and every source retired
    assert_eq!(committed.supersedes_source_note_ids, source_ids);
    assert_eq!(supersedes_edge_count(&db).await, source_ids.len() as i64);
    for source_id in &source_ids {
        assert_eq!(
            note_status(&db, source_id).await.as_deref(),
            Some("superseded"),
            "source {source_id} was not retired"
        );
        assert!(revision_count(&db, source_id).await >= 1);
    }
    assert_eq!(
        committed.final_source_statuses,
        source_ids
            .iter()
            .map(|id| (id.clone(), "superseded".to_owned()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_failure_at_each_write_boundary_leaves_no_partial_effect() {
    for boundary in ConsolidationWriteBoundary::ALL {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let consolidation = NoteConsolidationRepository::new(db.clone());
        let session = make_session(&db, &project.id, None, "worker/boundary").await;
        let fixture =
            canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
        let source_ids = fixture.source_ids();
        let notes_before: i64 =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE project_id = $1")
                .bind(&project.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let provenance_before = provenance_pairs(&db).await;

        consolidation.set_canonical_write_failure_for_test(Some(boundary));
        let error = consolidation
            .commit_consolidation_canonical(fixture.request(&source_ids))
            .await
            .expect_err("injected boundary failure must abort the transaction");
        assert!(
            error.to_string().contains("forced consolidation write failure"),
            "unexpected error at {boundary:?}: {error}"
        );
        consolidation.set_canonical_write_failure_for_test(None);

        // No canonical, display tag, provenance row, edge, or partial source
        // transition survives.
        let notes_after: i64 =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE project_id = $1")
                .bind(&project.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(notes_after, notes_before, "boundary {boundary:?} leaked a note");
        assert_eq!(canonical_note_count(&db).await, 0, "boundary {boundary:?}");
        assert_eq!(supersedes_edge_count(&db).await, 0, "boundary {boundary:?}");
        assert_eq!(
            provenance_pairs(&db).await,
            provenance_before,
            "boundary {boundary:?} leaked provenance"
        );
        for source_id in &source_ids {
            assert_eq!(
                note_status(&db, source_id).await.as_deref(),
                Some("active"),
                "boundary {boundary:?} left source {source_id} partially transitioned"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_canonical_attempts_commit_at_most_one() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/race").await;
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();

    let left_db = db.clone();
    let right_db = db.clone();
    let left_partition = fixture.partition.clone();
    let right_partition = fixture.partition.clone();
    let left_sources = source_ids.clone();
    let right_sources = source_ids.clone();

    let left = tokio::spawn(async move {
        let repo = NoteConsolidationRepository::new(left_db);
        repo.commit_consolidation_canonical(CommitConsolidationCanonical {
            partition: &left_partition,
            source_note_ids: &left_sources,
            title: "Canonical pattern: Retry Storm",
            content: "# Canonical pattern: Retry Storm\n\nConsolidated retry-storm guidance.",
            abstract_: None,
            overview: None,
            confidence: 0.7,
            scope_paths: "[]",
            reason: NoteRevisionReason::new("consolidation:create canonical cluster note").unwrap(),
        })
        .await
    });
    let right = tokio::spawn(async move {
        let repo = NoteConsolidationRepository::new(right_db);
        repo.commit_consolidation_canonical(CommitConsolidationCanonical {
            partition: &right_partition,
            source_note_ids: &right_sources,
            title: "Canonical pattern: Retry Storm",
            content: "# Canonical pattern: Retry Storm\n\nConsolidated retry-storm guidance.",
            abstract_: None,
            overview: None,
            confidence: 0.7,
            scope_paths: "[]",
            reason: NoteRevisionReason::new("consolidation:create canonical cluster note").unwrap(),
        })
        .await
    });

    let outcomes = [left.await.unwrap(), right.await.unwrap()];
    let committed = outcomes
        .iter()
        .filter(|outcome| {
            matches!(outcome, Ok(ConsolidationCommitOutcome::Committed(_)))
        })
        .count();
    assert_eq!(
        committed, 1,
        "sorted source locks must let exactly one competitor commit: {outcomes:?}"
    );

    // Whatever the loser reported, the database holds exactly one canonical and
    // one supersedes edge per source.
    assert_eq!(canonical_note_count(&db).await, 1);
    assert_eq!(supersedes_edge_count(&db).await, source_ids.len() as i64);
    for source_id in &source_ids {
        assert_eq!(
            note_status(&db, source_id).await.as_deref(),
            Some("superseded")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_attempt_identity_is_the_only_retry_witness() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/retry").await;
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();

    let ConsolidationCommitOutcome::Committed(committed) = consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap()
    else {
        panic!("first attempt must commit");
    };

    // An identical retry locates the exact attempt-ID canonical and matches its
    // partition, digest, and full supersedes endpoint set.
    let retried = consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap();
    let ConsolidationCommitOutcome::AlreadyCommitted(witness) = retried else {
        panic!("identical retry must resolve to the committed attempt, got {retried:?}");
    };
    assert_eq!(witness.canonical_note_id, committed.canonical_note_id);
    assert_eq!(
        witness.consolidation_attempt_id,
        committed.consolidation_attempt_id
    );
    assert_eq!(witness.supersedes_source_note_ids, source_ids);
    assert_eq!(canonical_note_count(&db).await, 1);
}

/// The single most important negative test: source status must never become a
/// false success witness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_independent_supersession_reports_conflict_and_claims_nothing() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/ambiguous").await;
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();

    // Injected pre-commit failure: the attempt's outcome is unknown to the
    // client and nothing was written.
    consolidation.set_canonical_write_failure_for_test(Some(
        ConsolidationWriteBoundary::SupersedesEdges,
    ));
    consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .expect_err("injected failure aborts the attempt");
    consolidation.set_canonical_write_failure_for_test(None);

    // A *different* actor independently retires every requested source. This is
    // exactly the state a naive implementation would read as "already done".
    for source_id in &source_ids {
        sqlx::query("UPDATE notes SET status = 'superseded' WHERE id = $1")
            .bind(source_id)
            .execute(db.pool())
            .await
            .unwrap();
    }
    assert_eq!(canonical_note_count(&db).await, 0);

    let retried = consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap();
    let ConsolidationCommitOutcome::Conflict(conflict) = retried else {
        panic!("all-superseded with no attempt-ID canonical must be a conflict, got {retried:?}");
    };
    assert_eq!(conflict.reason, ConsolidationConflictReason::SourceNotEligible);
    assert!(
        conflict
            .observed_source_statuses
            .iter()
            .all(|(_, status)| status == "superseded"),
        "the conflict should still report what it observed"
    );

    // Nothing was created; no attempt claims completion.
    assert_eq!(canonical_note_count(&db).await, 0);
    assert_eq!(supersedes_edge_count(&db).await, 0);
    let digest = crate::note_hash::note_content_hash(
        "# Canonical pattern: Retry Storm\n\nConsolidated retry-storm guidance.",
    );
    let attempt = consolidation_attempt_id(&fixture.partition, &source_ids, &digest);
    assert!(
        consolidation
            .find_consolidation_attempt(&attempt)
            .await
            .unwrap()
            .is_none(),
        "no canonical carries this attempt identity, so the retry must not claim success"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_source_state_creates_nothing_and_reports_conflict() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/mixed").await;
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();

    sqlx::query("UPDATE notes SET status = 'superseded' WHERE id = $1")
        .bind(&source_ids[1])
        .execute(db.pool())
        .await
        .unwrap();

    let outcome = consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap();
    let ConsolidationCommitOutcome::Conflict(conflict) = outcome else {
        panic!("a mixed source set must be a conflict, got {outcome:?}");
    };
    assert_eq!(conflict.reason, ConsolidationConflictReason::SourceNotEligible);
    assert_eq!(canonical_note_count(&db).await, 0);
    assert_eq!(supersedes_edge_count(&db).await, 0);
    assert_eq!(
        note_status(&db, &source_ids[0]).await.as_deref(),
        Some("active"),
        "the still-active sources must be untouched"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC7 — immutable creation attribution outlives the mutable display tag
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_tag_update_cannot_restore_canonical_eligibility() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/two-sweep").await;
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();

    let ConsolidationCommitOutcome::Committed(committed) = consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap()
    else {
        panic!("first sweep must commit");
    };

    // The canonical carries the selected session's provenance, so a second
    // sweep for the same partition genuinely sees it as a candidate row.
    assert!(
        committed
            .canonical_provenance_session_ids
            .contains(&fixture.session_id)
    );

    // Add enough additional similar active notes for a second cluster.
    for index in 0..3 {
        let note = repo
            .create_db_note(
                &project.id,
                &format!("Second Sweep Source {index}"),
                &format!("Second sweep source body {index}."),
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        consolidation.add_provenance(&note.id, &session).await.unwrap();
    }

    // A *generic* tag/content update strips the reserved display tag.
    repo.update(
        &committed.canonical_note_id,
        "Canonical pattern: Retry Storm",
        "# Canonical pattern: Retry Storm\n\nEdited by an ordinary tag/content update.",
        r#"["hand-edited"]"#,
    )
    .await
    .unwrap();
    let tags: String = sqlx::query_scalar::<_, String>("SELECT tags::text FROM notes WHERE id = $1")
        .bind(&committed.canonical_note_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(
        !tags.contains(CONSOLIDATION_CANONICAL_TAG),
        "the fixture must actually remove the display tag; tags were {tags}"
    );
    assert_eq!(
        note_status(&db, &committed.canonical_note_id).await.as_deref(),
        Some("active"),
        "the canonical is still an active note, so only attribution can exclude it"
    );

    // The second candidate query still omits it.
    let selection = consolidation
        .select_eligible_partition_sources(&fixture.partition)
        .await
        .unwrap();
    assert!(
        !selection
            .notes
            .iter()
            .any(|note| note.id == committed.canonical_note_id),
        "immutable consolidation creation attribution must keep excluding the canonical"
    );
    assert_eq!(
        selection.notes.len(),
        3,
        "only the three new active sources remain eligible"
    );

    // Direct submission of the canonical id fails revalidation without side
    // effects.
    let mut direct = selection
        .notes
        .iter()
        .take(2)
        .map(|note| note.id.clone())
        .collect::<Vec<_>>();
    direct.push(committed.canonical_note_id.clone());
    direct.sort();
    let outcome = consolidation
        .commit_consolidation_canonical(CommitConsolidationCanonical {
            partition: &fixture.partition,
            source_note_ids: &direct,
            title: "Canonical pattern: Second Sweep",
            content: "# Canonical pattern: Second Sweep\n\nShould never be written.",
            abstract_: None,
            overview: None,
            confidence: 0.7,
            scope_paths: "[]",
            reason: NoteRevisionReason::new("consolidation:second sweep").unwrap(),
        })
        .await
        .unwrap();
    let ConsolidationCommitOutcome::Conflict(conflict) = outcome else {
        panic!("submitting a canonical as a source must be rejected, got {outcome:?}");
    };
    assert_eq!(conflict.reason, ConsolidationConflictReason::SourceNotEligible);
    assert_eq!(
        canonical_note_count(&db).await,
        1,
        "the rejected second sweep must not create a canonical"
    );
    for note_id in direct.iter().filter(|id| **id != committed.canonical_note_id) {
        assert_eq!(
            note_status(&db, note_id).await.as_deref(),
            Some("active"),
            "no source may be retired by a rejected attempt"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AC11 — passive partition pressure
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_pressure_is_passive_and_reports_zero_slot_pressure() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation = NoteConsolidationRepository::new(db.clone());
    let session = make_session(&db, &project.id, None, "worker/pressure").await;

    for index in 0..4 {
        repo.create_db_note(
            &project.id,
            &format!("Pressure Pattern {index}"),
            "pressure pattern body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    }
    repo.create_db_note(&project.id, "Pressure Case", "pressure case body", "case", "[]")
        .await
        .unwrap();

    // A canonical is excluded from `eligible_notes` by the same immutable
    // attribution predicate the candidate query uses.
    let fixture =
        canonical_fixture(&repo, &consolidation, &project.id, &session, 3).await;
    let source_ids = fixture.source_ids();
    consolidation
        .commit_consolidation_canonical(fixture.request(&source_ids))
        .await
        .unwrap();

    let notes_before: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE project_id = $1")
            .bind(&project.id)
            .fetch_one(db.pool())
            .await
            .unwrap();

    let slots = HashMap::from([("pattern".to_owned(), 2i64), ("case".to_owned(), 0i64)]);
    let metrics = consolidation.partition_pressure_metrics(&slots).await.unwrap();

    let pattern = metrics
        .iter()
        .find(|metric| metric.project_id == project.id && metric.note_type == "pattern")
        .expect("pattern pressure reported");
    // Four standalone patterns remain active and non-canonical; the three
    // consolidated sources are superseded and the canonical is attributed.
    assert_eq!(pattern.eligible_notes, 4);
    assert_eq!(pattern.injectable_slots, 2);
    assert_eq!(pattern.oversubscription_ratio, Some(2.0));
    assert!(!pattern.unbounded_pressure);

    let case = metrics
        .iter()
        .find(|metric| metric.project_id == project.id && metric.note_type == "case")
        .expect("case pressure reported");
    assert_eq!(case.eligible_notes, 1);
    assert_eq!(case.injectable_slots, 0);
    assert_eq!(case.oversubscription_ratio, None);
    assert!(case.unbounded_pressure);

    // Passive: the snapshot mutated nothing.
    let notes_after: i64 =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE project_id = $1")
            .bind(&project.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(notes_after, notes_before);
}

// ═══════════════════════════════════════════════════════════════════════════
// Partition validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn partition_key_rejects_blank_wildcard_and_ineligible_keys() {
    let valid = ConsolidationPartitionKey {
        project_id: "project".to_owned(),
        session_id: "session".to_owned(),
        note_type: "pattern".to_owned(),
    };
    assert!(valid.validate().is_ok());

    for (project_id, session_id, note_type) in [
        ("", "session", "pattern"),
        ("project", "", "pattern"),
        ("project", "session", ""),
        ("project", "session", "design"),
        ("project", "session", "adr"),
        ("*", "session", "pattern"),
        ("project", "a,b", "pattern"),
        ("project", "session", "case,pattern"),
        (" project", "session", "pattern"),
    ] {
        let key = ConsolidationPartitionKey {
            project_id: project_id.to_owned(),
            session_id: session_id.to_owned(),
            note_type: note_type.to_owned(),
        };
        assert!(
            key.validate().is_err(),
            "expected rejection for {project_id:?}/{session_id:?}/{note_type:?}"
        );
    }
}
