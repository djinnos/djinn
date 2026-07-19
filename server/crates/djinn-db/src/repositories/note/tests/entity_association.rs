//! Tests for the heterogeneous typed entity-association substrate (qb9o
//! Wave 3). Verifies proposal↔note, note↔proposal, and proposal↔proposal
//! edges round-trip and dedupe idempotently through the new
//! `memory_entity_associations` table.
//!
//! The existing `note_associations` (F5 substrate, undirected canonical-
//! pair) substrate stays intact — every assertion in here explicitly
//! checks that no proposal body ends up in `notes`.

use djinn_core::models::Project;
use tokio::sync::broadcast;

use super::*;
use crate::repositories::note::{
    MemoryEntityKind, MemoryEntityRef, MemoryEntityType, NoteRepository,
};
use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};
use crate::repositories::test_support::{event_bus_for, make_project};

async fn make_note(repo: &NoteRepository, project: &Project, title: &str) -> String {
    repo.create(project.id.as_str(), title, "content", "reference", "[]")
        .await
        .unwrap()
        .id
}

async fn make_proposal(proposal_repo: &ProposalRepository, title: &str) -> String {
    proposal_repo
        .create(ProposalCreateInput {
            title,
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .unwrap()
        .id
}

// ── Helper: confirm no proposal body has been written into `notes` ──────────
//
// Acceptance criterion: "no proposal body is inserted into `notes`". A
// rogue codepath that mirrored proposals into `notes` would manifest as a
// non-zero count of rows in `notes` whose `permalink` or `title` matches
// one of the proposals we just created. We pin that count to zero after
// every upsert / list call below.
async fn assert_no_proposal_body_in_notes(db: &crate::database::Database, proposals: &[String]) {
    // No row in `notes` should ever carry one of the proposal ids as its
    // `id` (the proposal rows live in the `proposals` table, not `notes`).
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE id = ANY($1::text[])")
        .bind(proposals)
        .fetch_one(db.pool())
        .await
        .expect("notes-proposal-id overlap probe");
    assert_eq!(
        count, 0,
        "proposal body must not be inserted into notes (saw {count} overlap rows)"
    );
}

// ── Round-trip: proposal → note derived_from edge ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_proposal_to_note_derived_from_round_trips() {
    // A proposal that was derived from a note records a
    // `proposal → note, kind=derived_from` edge. The substrate must
    // persist it, expose it via the incident list on either endpoint,
    // and preserve direction (NOT collapse to `note ↔ note`).
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let note_id = make_note(&note_repo, &project, "Source note").await;
    let proposal_id = make_proposal(&proposal_repo, "Derived proposal").await;

    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal_id),
            MemoryEntityRef::note(&note_id),
            MemoryEntityKind::DerivedFrom,
            0.85,
        )
        .await
        .unwrap();

    // Read back from the proposal side.
    let from_proposal = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&proposal_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_proposal.len(), 1);
    let edge = &from_proposal[0];
    assert_eq!(edge.source.entity_type, MemoryEntityType::Proposal);
    assert_eq!(edge.source.id, proposal_id);
    assert_eq!(edge.target.entity_type, MemoryEntityType::Note);
    assert_eq!(edge.target.id, note_id);
    assert_eq!(edge.kind, MemoryEntityKind::DerivedFrom);
    assert!((edge.weight - 0.85).abs() < 1e-12);

    // Read back from the note side — same row, same direction.
    let from_note = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::note(&note_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_note.len(), 1);
    assert_eq!(from_note[0], *edge);

    // The proposal body must NOT have leaked into `notes`.
    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&proposal_id)).await;
}

// ── Round-trip: note → proposal typed traversal data ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_note_to_proposal_typed_edge_round_trips() {
    // The reverse direction — a note that `builds_on` a proposal — must
    // persist with source=note, target=proposal. Distinct from the
    // derived_from row above; both can coexist on the same pair.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let note_id = make_note(&note_repo, &project, "Follow-up note").await;
    let proposal_id = make_proposal(&proposal_repo, "Spec proposal").await;

    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::note(&note_id),
            MemoryEntityRef::proposal(&proposal_id),
            MemoryEntityKind::BuildsOn,
            0.7,
        )
        .await
        .unwrap();

    let from_note = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::note(&note_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_note.len(), 1);
    assert_eq!(from_note[0].source.entity_type, MemoryEntityType::Note);
    assert_eq!(from_note[0].source.id, note_id);
    assert_eq!(from_note[0].target.entity_type, MemoryEntityType::Proposal);
    assert_eq!(from_note[0].target.id, proposal_id);
    assert_eq!(from_note[0].kind, MemoryEntityKind::BuildsOn);
    assert!((from_note[0].weight - 0.7).abs() < 1e-12);

    let from_proposal = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&proposal_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_proposal.len(), 1);
    assert_eq!(from_proposal[0], from_note[0]);

    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&proposal_id)).await;
}

// ── Round-trip: proposal → proposal typed edge ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_proposal_to_proposal_typed_edge_round_trips() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let _project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let p_a = make_proposal(&proposal_repo, "Proposal A").await;
    let p_b = make_proposal(&proposal_repo, "Proposal B").await;

    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&p_a),
            MemoryEntityRef::proposal(&p_b),
            MemoryEntityKind::Supersedes,
            0.95,
        )
        .await
        .unwrap();

    let from_a = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&p_a), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_a[0].source.id, p_a);
    assert_eq!(from_a[0].target.id, p_b);
    assert_eq!(from_a[0].kind, MemoryEntityKind::Supersedes);

    let from_b = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&p_b), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_b.len(), 1);
    assert_eq!(from_b[0], from_a[0]);

    assert_no_proposal_body_in_notes(&db, &[p_a.clone(), p_b.clone()]).await;
}

// ── Idempotent max-weight merge on duplicate upserts ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_upserts_do_not_create_duplicate_rows() {
    // Same `(source, target, kind)` triple written multiple times must
    // collapse to ONE row with `weight = max(observed)`. A weaker later
    // observation must NOT overwrite a stronger earlier one — same
    // contract as `upsert_typed_association` on `note_associations`.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let note_id = make_note(&note_repo, &project, "Source note").await;
    let proposal_id = make_proposal(&proposal_repo, "Derived proposal").await;
    let src = MemoryEntityRef::proposal(&proposal_id);
    let tgt = MemoryEntityRef::note(&note_id);

    // First write — strong.
    note_repo
        .upsert_typed_entity_association(
            src.clone(),
            tgt.clone(),
            MemoryEntityKind::DerivedFrom,
            0.9,
        )
        .await
        .unwrap();
    // Second write — weaker; MAX must win.
    note_repo
        .upsert_typed_entity_association(
            src.clone(),
            tgt.clone(),
            MemoryEntityKind::DerivedFrom,
            0.4,
        )
        .await
        .unwrap();
    // Third — back up.
    note_repo
        .upsert_typed_entity_association(
            src.clone(),
            tgt.clone(),
            MemoryEntityKind::DerivedFrom,
            0.95,
        )
        .await
        .unwrap();

    // Exactly one row at the canonical slot — no duplicates.
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entity_associations
         WHERE source_entity_type = $1 AND source_id = $2
           AND target_entity_type = $3 AND target_id = $4
           AND kind = $5",
    )
    .bind(MemoryEntityType::Proposal.as_str())
    .bind(&proposal_id)
    .bind(MemoryEntityType::Note.as_str())
    .bind(&note_id)
    .bind(MemoryEntityKind::DerivedFrom.as_str())
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row_count, 1, "duplicate typed edge was inserted");

    // Weight preserved as max(0.9, 0.4, 0.95) = 0.95.
    let edges = note_repo
        .list_typed_entity_associations_for(src, 0.0, 100)
        .await
        .unwrap();
    assert_eq!(edges.len(), 1);
    assert!(
        (edges[0].weight - 0.95).abs() < 1e-12,
        "got {}",
        edges[0].weight
    );

    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&proposal_id)).await;
}

// ── Same pair can carry multiple typed kinds as distinct rows ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_pair_can_carry_multiple_distinct_typed_kinds() {
    // `kind` is in the primary key, so a `(source, target)` pair can
    // legitimately carry `builds_on` and `contradicts` (or any other
    // combination) simultaneously. Both edges must be persisted as
    // distinct rows and surface separately in the incident list.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let note_id = make_note(&note_repo, &project, "Source note").await;
    let proposal_id = make_proposal(&proposal_repo, "Tension proposal").await;
    let src = MemoryEntityRef::proposal(&proposal_id);
    let tgt = MemoryEntityRef::note(&note_id);

    note_repo
        .upsert_typed_entity_association(src.clone(), tgt.clone(), MemoryEntityKind::BuildsOn, 0.6)
        .await
        .unwrap();
    note_repo
        .upsert_typed_entity_association(
            src.clone(),
            tgt.clone(),
            MemoryEntityKind::Contradicts,
            0.4,
        )
        .await
        .unwrap();

    let edges = note_repo
        .list_typed_entity_associations_for(src, 0.0, 100)
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        2,
        "expected 2 distinct typed kinds, got {edges:?}"
    );

    let kinds: std::collections::HashSet<_> = edges.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&MemoryEntityKind::BuildsOn));
    assert!(kinds.contains(&MemoryEntityKind::Contradicts));

    // Reverse direction incident list on the note side returns the same
    // two edges.
    let from_note = note_repo
        .list_typed_entity_associations_for(tgt, 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_note.len(), 2);
    let from_note_kinds: std::collections::HashSet<_> = from_note.iter().map(|e| e.kind).collect();
    assert_eq!(from_note_kinds, kinds);

    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&proposal_id)).await;
}

// ── Directional — reverse is a distinct row ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reverse_direction_is_a_distinct_row() {
    // The substrate is NOT canonicalized to (min, max). `proposal → note`
    // and `note → proposal` (with the same kind) are TWO rows — a
    // proposal might `builds_on` a note, and the note might `exemplifies`
    // the proposal — semantically different relationships that must both
    // persist.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let note_id = make_note(&note_repo, &project, "Source note").await;
    let proposal_id = make_proposal(&proposal_repo, "Source proposal").await;

    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&proposal_id),
            MemoryEntityRef::note(&note_id),
            MemoryEntityKind::BuildsOn,
            0.5,
        )
        .await
        .unwrap();
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::note(&note_id),
            MemoryEntityRef::proposal(&proposal_id),
            MemoryEntityKind::Exemplifies,
            0.6,
        )
        .await
        .unwrap();

    let from_note = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::note(&note_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_note.len(), 2);
    // Each edge carries a distinct direction.
    let mut sources = from_note
        .iter()
        .map(|e| (e.source.entity_type, e.source.id.clone()))
        .collect::<Vec<_>>();
    sources.sort();
    assert_eq!(
        sources,
        vec![
            (MemoryEntityType::Note, note_id.clone()),
            (MemoryEntityType::Proposal, proposal_id.clone()),
        ]
    );

    // Total row count across both directions: exactly 2.
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entity_associations
         WHERE (source_entity_type = 'note'   AND source_id = $1)
            OR (target_entity_type = 'note'   AND target_id = $1)
            OR (source_entity_type = 'proposal' AND source_id = $2)
            OR (target_entity_type = 'proposal' AND target_id = $2)",
    )
    .bind(&note_id)
    .bind(&proposal_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row_count, 2);

    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&proposal_id)).await;
}

// ── CHECK constraint rejects illegal entity_type / kind / self-edges ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_constraints_reject_illegal_values() {
    // Raw INSERTs that bypass the helper must hit the CHECK constraints
    // added in migration 74. This guards against silent regressions if
    // a future migration accidentally drops one of the constraints.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let note_id = make_note(&note_repo, &project, "Constraint probe").await;

    // Illegal entity_type → chk_mea_source_entity_type fires.
    let bad_entity = sqlx::query(
        "INSERT INTO memory_entity_associations
             (source_entity_type, source_id, target_entity_type, target_id, kind, weight)
         VALUES ('epic', $1, 'note', $2, 'builds_on', 0.5)",
    )
    .bind(&note_id)
    .bind(&note_id)
    .execute(db.pool())
    .await;
    assert!(
        bad_entity.is_err(),
        "illegal source_entity_type 'epic' must violate CHECK, got {bad_entity:?}"
    );

    // Illegal kind → chk_mea_kind fires.
    let bad_kind = sqlx::query(
        "INSERT INTO memory_entity_associations
             (source_entity_type, source_id, target_entity_type, target_id, kind, weight)
         VALUES ('note', $1, 'note', $2, 'co_access', 0.5)",
    )
    .bind(&note_id)
    .bind(&note_id)
    .execute(db.pool())
    .await;
    assert!(
        bad_kind.is_err(),
        "illegal kind 'co_access' must violate CHECK, got {bad_kind:?}"
    );

    // Self-edge → chk_mea_not_self_edge fires.
    let self_edge = sqlx::query(
        "INSERT INTO memory_entity_associations
             (source_entity_type, source_id, target_entity_type, target_id, kind, weight)
         VALUES ('note', $1, 'note', $1, 'builds_on', 0.5)",
    )
    .bind(&note_id)
    .execute(db.pool())
    .await;
    assert!(
        self_edge.is_err(),
        "self-edge must violate CHECK, got {self_edge:?}"
    );

    // Helper-side self-edge guard surfaces a clean error too.
    let helper_self_edge = note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::note(&note_id),
            MemoryEntityRef::note(&note_id),
            MemoryEntityKind::BuildsOn,
            0.5,
        )
        .await;
    assert!(
        matches!(helper_self_edge, Err(crate::Error::InvalidData(_))),
        "helper must reject self-edges with InvalidData, got {helper_self_edge:?}"
    );
}

// ── note_associations substrate remains intact ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn note_associations_substrate_is_unaffected() {
    // Backward-compat check: writing typed edges through the new substrate
    // must not touch `note_associations` and must not affect the
    // note↔note helpers (upsert_association, upsert_typed_association,
    // get_association_kind).
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let n1 = make_note(&note_repo, &project, "Legacy note A").await;
    let n2 = make_note(&note_repo, &project, "Legacy note B").await;
    let p1 = make_proposal(&proposal_repo, "Sidecar proposal").await;

    // Seed the F5 substrate with a `derived_from` typed edge on note↔note.
    note_repo
        .upsert_typed_association(&n1, &n2, MemoryEntityKind::DerivedFrom.into(), 0.7)
        .await
        .unwrap();
    // Add a co_access Hebbian edge on a different note↔note pair.
    let n3 = make_note(&note_repo, &project, "Legacy note C").await;
    let n4 = make_note(&note_repo, &project, "Legacy note D").await;
    note_repo.upsert_association(&n3, &n4, 1).await.unwrap();

    // Now write a proposal↔note typed edge through the NEW substrate.
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&p1),
            MemoryEntityRef::note(&n1),
            MemoryEntityKind::BuildsOn,
            0.6,
        )
        .await
        .unwrap();

    // The F5 note↔note row is untouched: kind, weight all match the
    // original upsert_typed_association write.
    let (weight, kind) = note_repo
        .get_association_kind(&n1, &n2)
        .await
        .unwrap()
        .expect("note↔note derived_from edge present");
    assert_eq!(kind, "derived_from");
    assert!((weight - 0.7).abs() < 1e-12);

    // The co_access row is untouched.
    let (_w, k) = note_repo
        .get_association_kind(&n3, &n4)
        .await
        .unwrap()
        .expect("co_access edge present");
    assert_eq!(k, "co_access");

    // `note_associations` has exactly the two rows we seeded — no
    // accidental writes through the new substrate.
    let note_assoc_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_associations")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        note_assoc_count, 2,
        "new substrate must not write into note_associations"
    );

    // `memory_entity_associations` has exactly the one row we wrote.
    let mea_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entity_associations")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(mea_count, 1);

    // Sanity: the proposal body never landed in `notes`.
    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&p1)).await;
}

// ── Every kind in the widened F5 set is accepted ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_widened_kind_round_trips_on_proposal_to_note() {
    // Walk every value of `MemoryEntityKind` and verify each one
    // round-trips through upsert + list with the expected string form.
    // Mirrors the existing `upsert_typed_association_writes_each_widened_kind`
    // test on `note_associations` but exercises the heterogeneous substrate.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let cases = [
        (MemoryEntityKind::DerivedFrom, "derived_from", 0.6_f64),
        (MemoryEntityKind::BuildsOn, "builds_on", 0.8),
        (MemoryEntityKind::Contradicts, "contradicts", 0.9),
        (MemoryEntityKind::Supersedes, "supersedes", 0.95),
        (MemoryEntityKind::Exemplifies, "exemplifies", 0.7),
    ];

    let mut proposal_ids = Vec::new();
    for (kind, expected_str, weight) in cases.iter() {
        let note_id = make_note(&note_repo, &project, &format!("Note {kind:?}")).await;
        let proposal_id = make_proposal(&proposal_repo, &format!("Proposal {kind:?}")).await;
        proposal_ids.push(proposal_id.clone());

        note_repo
            .upsert_typed_entity_association(
                MemoryEntityRef::proposal(&proposal_id),
                MemoryEntityRef::note(&note_id),
                *kind,
                *weight,
            )
            .await
            .unwrap();

        let edges = note_repo
            .list_typed_entity_associations_for(MemoryEntityRef::proposal(&proposal_id), 0.0, 100)
            .await
            .unwrap();
        assert_eq!(edges.len(), 1, "{kind:?} should round-trip");
        assert_eq!(edges[0].kind, *kind, "{kind:?} kind mismatch");
        assert!(
            (edges[0].weight - *weight).abs() < 1e-12,
            "{kind:?} weight mismatch: got {}",
            edges[0].weight
        );
        // String form must match the F5 substrate's enumerated value.
        assert_eq!(kind.as_str(), *expected_str);
    }

    assert_no_proposal_body_in_notes(&db, &proposal_ids).await;
}

// ── min_weight / limit filters on the incident list ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_filters_by_min_weight_and_limit() {
    // Verify the helper's `min_weight` and `limit` knobs behave the same
    // way they do on `list_associations_for_note`. Used by graph and
    // retrieval callers to top-K typed-edge fan-out.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let pivot = make_proposal(&proposal_repo, "Pivot").await;

    // Three proposal → note edges with weights 0.3, 0.5, 0.8.
    let n_low = make_note(&note_repo, &project, "Low").await;
    let n_mid = make_note(&note_repo, &project, "Mid").await;
    let n_high = make_note(&note_repo, &project, "High").await;
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&pivot),
            MemoryEntityRef::note(&n_low),
            MemoryEntityKind::BuildsOn,
            0.3,
        )
        .await
        .unwrap();
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&pivot),
            MemoryEntityRef::note(&n_mid),
            MemoryEntityKind::BuildsOn,
            0.5,
        )
        .await
        .unwrap();
    note_repo
        .upsert_typed_entity_association(
            MemoryEntityRef::proposal(&pivot),
            MemoryEntityRef::note(&n_high),
            MemoryEntityKind::BuildsOn,
            0.8,
        )
        .await
        .unwrap();

    // min_weight = 0.4 → 2 edges.
    let above_04 = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&pivot), 0.4, 0)
        .await
        .unwrap();
    assert_eq!(above_04.len(), 2);
    // Strongest first.
    assert!(above_04[0].weight >= above_04[1].weight);
    assert_eq!(above_04[0].target.id, n_high);

    // limit = 1 → top edge.
    let top1 = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&pivot), 0.0, 1)
        .await
        .unwrap();
    assert_eq!(top1.len(), 1);
    assert!((top1[0].weight - 0.8).abs() < 1e-12);

    // min_weight=0.95 → empty.
    let empty = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&pivot), 0.95, 0)
        .await
        .unwrap();
    assert!(empty.is_empty());

    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&pivot)).await;
}

// ── Proposal derived_from edges: proposal→epic→task→notes fixture ────────────
//
// Exercises the repository-backed path that session extraction uses when a
// task belonging to an epic linked to a proposal writes or reads notes.
// Mirrors the flow: proposal_graduate → link_epic → task under epic →
// session extraction records `derived_from` edges from proposal to notes.

use crate::repositories::epic::{EpicCreateInput, EpicRepository};
use crate::{EffectiveCreatorProvenance, TaskRepository, UserRepository};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_derived_from_edges_via_epic_task_notes_fixture() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let epic_repo = EpicRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task_repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // 1. Create a proposal (simulates an approved proposal being graduated).
    let proposal_id = make_proposal(&proposal_repo, "Test proposal").await;

    // 2. Create an epic in the project.
    let epic = epic_repo
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "Build feature X",
                description: "desc",
                emoji: "🚀",
                color: "#ff0000",
                owner: "test-user",
                memory_refs: None,
                status: None,
                auto_breakdown: Some(false),
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .unwrap();

    // 3. Link the proposal to the epic (proposal_graduate path).
    proposal_repo
        .link_epic(&proposal_id, &epic.id, &project.id)
        .await
        .unwrap();

    // 4. Create a task under the epic with an insertion-time fixture creator.
    let fixture_identity = uuid::Uuid::now_v7();
    let github_id = (fixture_identity.as_u128() % 9_000_000_000_000_000_000) as i64;
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("note-association-fixture-{fixture_identity}"),
            Some("Note association fixture"),
            None,
        )
        .await
        .unwrap();
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "Implement feature",
            "Do the work",
            "Design doc",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(task.epic_id.as_deref(), Some(epic.id.as_str()));

    // 5. Create notes that the session read and wrote.
    let read_note_id = make_note(&note_repo, &project, "ADR read during planning").await;
    let written_note_id = make_note(&note_repo, &project, "Case note written by task").await;

    // 6. Simulate session extraction: record derived_from edges from the
    //    proposal to both the read and written notes.
    //
    //    This mirrors what `emit_proposal_derived_from_edges` does:
    //    resolve the proposal via proposal_for_epic, then for each note
    //    call upsert_typed_entity_association.
    let resolved_proposal = proposal_repo
        .proposal_for_epic(&epic.id)
        .await
        .unwrap()
        .expect("proposal_for_epic should return the linked proposal");
    assert_eq!(resolved_proposal.id, proposal_id);

    let proposal_ref = MemoryEntityRef::proposal(&proposal_id);

    // Record edge: proposal → read note (derived_from)
    note_repo
        .upsert_typed_entity_association(
            proposal_ref.clone(),
            MemoryEntityRef::note(&read_note_id),
            MemoryEntityKind::DerivedFrom,
            0.8,
        )
        .await
        .unwrap();

    // Record edge: proposal → written note (derived_from)
    note_repo
        .upsert_typed_entity_association(
            proposal_ref.clone(),
            MemoryEntityRef::note(&written_note_id),
            MemoryEntityKind::DerivedFrom,
            0.8,
        )
        .await
        .unwrap();

    // 7. Assert: the proposal has two derived_from edges.
    let proposal_edges = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::proposal(&proposal_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(
        proposal_edges.len(),
        2,
        "proposal should have two derived_from edges (one per note)"
    );
    for edge in &proposal_edges {
        assert_eq!(edge.source.entity_type, MemoryEntityType::Proposal);
        assert_eq!(edge.source.id, proposal_id);
        assert_eq!(edge.target.entity_type, MemoryEntityType::Note);
        assert_eq!(edge.kind, MemoryEntityKind::DerivedFrom);
        assert!((edge.weight - 0.8).abs() < 1e-12);
    }
    let target_ids: Vec<&str> = proposal_edges
        .iter()
        .map(|e| e.target.id.as_str())
        .collect();
    assert!(
        target_ids.contains(&read_note_id.as_str()),
        "read note should be among the proposal's derived_from targets"
    );
    assert!(
        target_ids.contains(&written_note_id.as_str()),
        "written note should be among the proposal's derived_from targets"
    );

    // 8. Assert: each note also sees the edge from the other direction.
    let from_read_note = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::note(&read_note_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_read_note.len(), 1);
    assert_eq!(from_read_note[0].source.id, proposal_id);
    assert_eq!(from_read_note[0].kind, MemoryEntityKind::DerivedFrom);

    let from_written_note = note_repo
        .list_typed_entity_associations_for(MemoryEntityRef::note(&written_note_id), 0.0, 100)
        .await
        .unwrap();
    assert_eq!(from_written_note.len(), 1);
    assert_eq!(from_written_note[0].source.id, proposal_id);
    assert_eq!(from_written_note[0].kind, MemoryEntityKind::DerivedFrom);

    // 9. Confirm no proposal body leaked into `notes`.
    assert_no_proposal_body_in_notes(&db, std::slice::from_ref(&proposal_id)).await;

    // 10. Confirm existing task/epic memory_refs autolink behavior is
    //     unchanged: the note_associations (F5) substrate still works
    //     independently for co-access.
    note_repo
        .upsert_association(&read_note_id, &written_note_id, 1)
        .await
        .unwrap();
    let co_access = note_repo
        .get_associations_for_note(&read_note_id)
        .await
        .unwrap();
    assert!(
        !co_access.is_empty(),
        "co-access association should still work alongside entity edges"
    );
}

// ── Helper: convert MemoryEntityKind → NoteAssociationKind for legacy path ─
//
// The legacy `note_associations` substrate exposes
// `NoteAssociationKind::DerivedFrom` etc. via the
// `upsert_typed_association` test helper. We provide a tiny
// `From<MemoryEntityKind> for NoteAssociationKind` conversion so the
// backward-compat test above can pass a heterogeneous-substrate kind
// into the note↔note typed helper without re-listing the same enum
// twice.
impl From<MemoryEntityKind> for crate::repositories::note::NoteAssociationKind {
    fn from(k: MemoryEntityKind) -> Self {
        use crate::repositories::note::NoteAssociationKind as N;
        match k {
            MemoryEntityKind::DerivedFrom => N::DerivedFrom,
            MemoryEntityKind::BuildsOn => N::BuildsOn,
            MemoryEntityKind::Contradicts => N::Contradicts,
            MemoryEntityKind::Supersedes => N::Supersedes,
            MemoryEntityKind::Exemplifies => N::Exemplifies,
        }
    }
}
