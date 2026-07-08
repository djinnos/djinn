//! Tests for the split authored-orphan / graph-isolation health metrics.
//!
//! Validates that `HealthReport` correctly distinguishes:
//! - **authored orphans** (no inbound wikilinks or `authored` associations),
//! - **graph-isolated notes** (no retrieval-effective edges at all), and
//! - **machine-connected orphans** (authored orphans rescued by non-authored
//!   retrieval edges such as `embedding_related` or `co_access`).

use tokio::sync::broadcast;

use crate::database::Database;
use crate::repositories::note::embedding_associations::EMBEDDING_ASSOCIATION_THRESHOLD;
use crate::repositories::note::{
    NoteAssociationKind, NoteAssociationProvenanceUpsert, NoteAssociationSource, NoteRepository,
};
use crate::repositories::test_support::{event_bus_for, make_project};

/// Three notes: Hub is linked-to by Source via wikilink; Orphan has no
/// inbound edges; Isolated has no edges at all (not even outbound).
///
/// Source has an outbound wikilink (authored edge) but no inbound → still an
/// authored orphan.  Because outbound wikilinks are authored edges (not
/// machine-minted), Source is NOT counted as a machine-connected orphan.
///
/// Expected health after setup:
/// - authored_orphan_count = 2 (Source + Isolated — neither has inbound wikilinks)
/// - isolated_count = 1 (Isolated only)
/// - machine_connected_orphan_count = 0 (no non-authored retrieval edges exist)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authored_orphan_vs_isolation_basic() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Hub: target of a wikilink.
    let _hub = repo
        .create(&project.id, "Hub", "body", "adr", "[]")
        .await
        .unwrap();

    // Source: links to Hub via wikilink → has outbound wikilink, no inbound.
    repo.create(
        &project.id,
        "Source",
        "See [[Hub]] for details.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    // Orphan: has no inbound wikilink, no inbound authored association, but
    // has an outbound wikilink (to Hub) → NOT isolated, but IS authored orphan.
    // (Source already covers the "outbound wikilink but no inbound" case.)

    // Isolated: no edges at all.
    repo.create(&project.id, "Isolated", "lonely note", "pattern", "[]")
        .await
        .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    // authored_orphan_count: Source (no inbound wikilink) + Isolated (no edges).
    assert_eq!(
        health.authored_orphan_count, 2,
        "Source and Isolated are authored orphans"
    );
    // Backward-compat alias must match.
    assert_eq!(health.orphan_note_count, health.authored_orphan_count);

    // isolated_count: only Isolated has zero retrieval-effective edges.
    assert_eq!(
        health.isolated_count, 1,
        "only Isolated has no edges at all"
    );

    // machine_connected_orphan: authored orphans connected by *non-authored*
    // retrieval edges (co_access, threshold-qualified embedding_related).
    // Source's outbound wikilink is an authored edge — it does NOT qualify.
    assert_eq!(
        health.machine_connected_orphan_count, 0,
        "Source's outbound wikilink is an authored edge, not machine-minted"
    );

    // isolated_pct: 1 isolated out of 3 non-singleton notes.
    let expected_pct = 1.0 / 3.0 * 100.0;
    assert!(
        (health.isolated_pct - expected_pct).abs() < 1e-9,
        "isolated_pct should be ~33.33, got {}",
        health.isolated_pct
    );

    // orphans() detail should return the same 2 authored orphans.
    let orphans = repo.orphans(&project.id, None).await.unwrap();
    assert_eq!(orphans.len(), 2, "orphans() returns Source and Isolated");
    let titles: Vec<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
    assert!(titles.contains(&"Source"));
    assert!(titles.contains(&"Isolated"));
}

/// An authored association (`kind = 'authored'`) should rescue a note from
/// being an authored orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authored_association_rescues_from_orphan() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let anchor = repo
        .create(&project.id, "Anchor", "body", "adr", "[]")
        .await
        .unwrap();
    let rescued = repo
        .create(&project.id, "Rescued", "no wikilinks", "pattern", "[]")
        .await
        .unwrap();
    let _lonely = repo
        .create(&project.id, "Lonely", "no edges", "research", "[]")
        .await
        .unwrap();

    // Give Rescued an inbound authored association from Anchor.
    repo.upsert_typed_association(&anchor.id, &rescued.id, NoteAssociationKind::Authored, 0.9)
        .await
        .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    // Only Lonely is an authored orphan (no inbound wikilink or authored assoc).
    assert_eq!(health.authored_orphan_count, 1);
    assert_eq!(health.orphan_note_count, 1);

    // Lonely is also isolated (no edges at all).
    assert_eq!(health.isolated_count, 1);
    assert_eq!(health.machine_connected_orphan_count, 0);

    // orphans() should not include Rescued.
    let orphans = repo.orphans(&project.id, None).await.unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].title, "Lonely");
}

/// A machine-minted `embedding_related` edge should NOT hide authored-orphan
/// debt but SHOULD reduce graph isolation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_related_creates_machine_connected_orphan() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let hub = repo
        .create(&project.id, "Hub", "body", "adr", "[]")
        .await
        .unwrap();
    let machine_rescued = repo
        .create(
            &project.id,
            "MachineRescued",
            "no wikilinks",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let _isolated = repo
        .create(&project.id, "Isolated", "no edges at all", "research", "[]")
        .await
        .unwrap();

    // Seed an embedding_related edge between Hub and MachineRescued.
    // Confidence above threshold → retrieval-effective.
    let confidence = EMBEDDING_ASSOCIATION_THRESHOLD + 0.05;
    repo.upsert_provenance_association(
        &hub.id,
        &machine_rescued.id,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.20,
            confidence: Some(confidence),
            algorithm_version: Some("test-v1".to_owned()),
            embedding_model: Some("test-model".to_owned()),
            embedding_dim: Some(384),
        },
    )
    .await
    .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    // Hub, MachineRescued, and Isolated all have no inbound wikilink or
    // authored association → all three are authored orphans (machine edges
    // don't hide debt).
    assert_eq!(
        health.authored_orphan_count, 3,
        "Hub, MachineRescued, and Isolated are authored orphans"
    );
    assert_eq!(health.orphan_note_count, health.authored_orphan_count);

    // Only Isolated is graph-isolated; Hub and MachineRescued are connected
    // by the embedding_related edge.
    assert_eq!(health.isolated_count, 1, "only Isolated is graph-isolated");

    // Hub and MachineRescued are both machine-connected orphans (the
    // embedding_related edge connects them).
    assert_eq!(
        health.machine_connected_orphan_count, 2,
        "Hub and MachineRescued are rescued from isolation by embedding_related edge"
    );

    // orphans() should include all three (embedding_related does not hide
    // authored-link debt).
    let orphans = repo.orphans(&project.id, None).await.unwrap();
    assert_eq!(orphans.len(), 3);
    let titles: Vec<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
    assert!(titles.contains(&"Hub"));
    assert!(titles.contains(&"MachineRescued"));
    assert!(titles.contains(&"Isolated"));
}

/// A `co_access` edge should NOT hide authored-orphan debt but SHOULD
/// reduce graph isolation (same semantics as embedding_related).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_access_creates_machine_connected_orphan() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let hub = repo
        .create(&project.id, "Hub", "body", "adr", "[]")
        .await
        .unwrap();
    let co_rescued = repo
        .create(&project.id, "CoRescued", "no wikilinks", "pattern", "[]")
        .await
        .unwrap();
    let _isolated = repo
        .create(&project.id, "Isolated", "no edges at all", "research", "[]")
        .await
        .unwrap();

    // Seed a co_access edge between Hub and CoRescued.
    repo.upsert_association(&hub.id, &co_rescued.id, 1)
        .await
        .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    // Hub, CoRescued, and Isolated all have no inbound wikilink or authored
    // association → all three are authored orphans.
    assert_eq!(health.authored_orphan_count, 3);
    assert_eq!(health.orphan_note_count, health.authored_orphan_count);

    // Only Isolated is graph-isolated; Hub and CoRescued are connected by
    // the co_access edge.
    assert_eq!(health.isolated_count, 1, "only Isolated is graph-isolated");
    assert_eq!(health.machine_connected_orphan_count, 2);

    let orphans = repo.orphans(&project.id, None).await.unwrap();
    assert_eq!(orphans.len(), 3);
}

/// An `embedding_related` edge with confidence *below* threshold should NOT
/// count as a retrieval-effective edge — the note remains isolated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_threshold_embedding_does_not_reduce_isolation() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let hub = repo
        .create(&project.id, "Hub", "body", "adr", "[]")
        .await
        .unwrap();
    let weak_note = repo
        .create(&project.id, "WeakNote", "no wikilinks", "pattern", "[]")
        .await
        .unwrap();

    // Seed an embedding_related edge with confidence BELOW threshold.
    let confidence = EMBEDDING_ASSOCIATION_THRESHOLD - 0.10;
    repo.upsert_provenance_association(
        &hub.id,
        &weak_note.id,
        &NoteAssociationProvenanceUpsert {
            kind: NoteAssociationKind::EmbeddingRelated,
            source: NoteAssociationSource::EmbeddingSimilarity,
            weight: 0.05,
            confidence: Some(confidence),
            algorithm_version: Some("test-v1".to_owned()),
            embedding_model: Some("test-model".to_owned()),
            embedding_dim: Some(384),
        },
    )
    .await
    .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    // Both Hub and WeakNote are authored orphans (neither has an inbound
    // wikilink or authored association).
    assert_eq!(health.authored_orphan_count, 2);

    // Both are ALSO graph-isolated because the below-threshold
    // embedding_related edge is not retrieval-effective.
    assert_eq!(
        health.isolated_count, 2,
        "below-threshold embedding edge does not reduce isolation"
    );
    assert_eq!(
        health.machine_connected_orphan_count, 0,
        "no machine-connected orphans"
    );
}

/// Archived and deprecated non-singleton notes must be excluded from
/// authored-orphan, isolation, and non-singleton counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_and_deprecated_notes_excluded_from_isolation_metrics() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Active note with no edges → authored orphan + isolated.
    repo.create(
        &project.id,
        "ActiveIsolated",
        "no edges at all",
        "pattern",
        "[]",
    )
    .await
    .unwrap();

    // Archived note with no edges → must NOT inflate counts.
    repo.create_with_status(
        &project.id,
        "ArchivedOrphan",
        "archived and alone",
        "pattern",
        Some("archived"),
        "[]",
    )
    .await
    .unwrap();

    // Deprecated note with no edges → must NOT inflate counts.
    repo.create_with_status(
        &project.id,
        "DeprecatedOrphan",
        "deprecated and alone",
        "research",
        Some("deprecated"),
        "[]",
    )
    .await
    .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    // Only ActiveIsolated is counted (active, non-singleton, no edges).
    assert_eq!(
        health.authored_orphan_count, 1,
        "archived/deprecated notes excluded from authored_orphan_count"
    );
    assert_eq!(health.orphan_note_count, health.authored_orphan_count);

    assert_eq!(
        health.isolated_count, 1,
        "archived/deprecated notes excluded from isolated_count"
    );
    assert_eq!(
        health.machine_connected_orphan_count, 0,
        "no machine-connected orphans"
    );

    // non-singleton denominator also excludes archived/deprecated.
    // 1 active non-singleton note → isolated_pct = 100%.
    assert!(
        (health.isolated_pct - 100.0).abs() < 1e-9,
        "isolated_pct should be 100.0 (1/1 active non-singleton), got {}",
        health.isolated_pct
    );

    // orphans() should also exclude archived/deprecated.
    let orphans = repo.orphans(&project.id, None).await.unwrap();
    assert_eq!(
        orphans.len(),
        1,
        "orphans() excludes archived/deprecated notes"
    );
    assert_eq!(orphans[0].title, "ActiveIsolated");
}
