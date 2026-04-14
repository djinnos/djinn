//! Integration-style hardening coverage for the transport-neutral memory filesystem core.
//!
//! These tests intentionally exercise the repository-backed filesystem end-to-end:
//! list/read/create/update/rename/delete all flow through [`MemoryFilesystemCore`] while
//! assertions validate repository-visible side effects such as access tracking, frontmatter
//! normalization, and wikilink/index maintenance.
//!
//! Wave-1 gaps remain intentionally out of scope here:
//! - debounced write batching and commit coalescing belong to the mount transport/runtime layer
//! - branch-aware mount switching belongs to later ADR-057 waves once session-scoped mounts land
//! - transport adapter behavior (FUSE/NFS inode semantics, kernel caching, mount lifecycle) is
//!   covered separately from this repository-backed core seam
//!
//! Keeping those gaps documented here makes it explicit that these tests are validating semantic
//! parity for the filesystem core rather than transport-specific behavior.

use std::path::Path;

use djinn_db::{Database, NoteRepository};

use super::{MemoryEntryKind, MemoryFilesystemCore, MemoryFsError};
use crate::test_helpers::{create_test_db, create_test_project_with_dir, test_events};

async fn make_core() -> (MemoryFilesystemCore, Database, String, tempfile::TempDir) {
    let db = create_test_db();
    let (project, project_dir) = create_test_project_with_dir(&db).await;
    let repo = NoteRepository::new(db.clone(), test_events());
    (MemoryFilesystemCore::new(repo), db, project.id, project_dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_backed_read_and_list_flow_tracks_access_and_rendering() {
    let (core, db, project_id, project_dir) = make_core().await;
    let repo = NoteRepository::new(db.clone(), test_events());

    let singleton = repo
        .create(
            &project_id,
            project_dir.path(),
            "Project Brief",
            "Brief body",
            "brief",
            "[\"summary\"]",
        )
        .await
        .unwrap();
    let nested = repo
        .create(
            &project_id,
            project_dir.path(),
            "Routing Guide",
            "Read [[Project Brief]].",
            "design_spec",
            "[\"routing\"]",
        )
        .await
        .unwrap();

    let root = core.list_dir(&project_id, "/").await.unwrap();
    assert!(root.iter().any(|entry| entry.name == "brief.md"));
    assert!(root.iter().any(|entry| entry.name == "design"));

    let design = core.list_dir(&project_id, "design").await.unwrap();
    assert!(design.iter().any(|entry| entry.name == "specs"));

    let nested_dir = core.list_dir(&project_id, "design/specs").await.unwrap();
    let nested_entry = nested_dir
        .iter()
        .find(|entry| entry.name == "routing-guide.md")
        .unwrap();
    assert_eq!(nested_entry.metadata.kind, MemoryEntryKind::File);
    assert_eq!(nested_entry.metadata.path, "design/specs/routing-guide.md");

    let before_singleton = repo
        .get_by_permalink(&project_id, &singleton.permalink)
        .await
        .unwrap()
        .unwrap();
    let before_nested = repo
        .get_by_permalink(&project_id, &nested.permalink)
        .await
        .unwrap()
        .unwrap();

    let brief = core.read_file(&project_id, "brief.md").await.unwrap();
    assert_eq!(brief.metadata.kind, MemoryEntryKind::File);
    assert!(
        brief
            .content
            .starts_with("---\ntitle: Project Brief\ntype: brief")
    );
    assert!(brief.content.contains("tags: [\"summary\"]"));
    assert!(brief.content.ends_with("Brief body"));

    let routed = core
        .read_file(&project_id, "/design/specs/routing-guide.md")
        .await
        .unwrap();
    assert!(routed.content.contains("Read [[Project Brief]]."));
    assert_eq!(routed.metadata.size, routed.content.len() as u64);

    let after_singleton = repo
        .get_by_permalink(&project_id, &singleton.permalink)
        .await
        .unwrap()
        .unwrap();
    let after_nested = repo
        .get_by_permalink(&project_id, &nested.permalink)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after_singleton.access_count,
        before_singleton.access_count + 1
    );
    assert!(after_singleton.last_accessed >= before_singleton.last_accessed);
    assert_eq!(after_nested.access_count, before_nested.access_count + 1);
    assert!(after_nested.last_accessed >= before_nested.last_accessed);

    let stat = core
        .stat(&project_id, "design/specs/routing-guide.md")
        .await
        .unwrap();
    assert_eq!(stat.kind, MemoryEntryKind::File);
    assert_eq!(stat.size, routed.content.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_backed_mutation_flow_preserves_frontmatter_and_index_side_effects() {
    let (core, db, project_id, project_dir) = make_core().await;
    let repo = NoteRepository::new(db.clone(), test_events());

    let target = repo
        .create(
            &project_id,
            project_dir.path(),
            "Target Note",
            "Target body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let created = core
        .write_file(
            &project_id,
            project_dir.path(),
            "patterns/source-note.md",
            "---\ntitle: Source Note\ntype: pattern\ntags: [\"fs\",\"core\"]\n---\n\nLinks to [[Target Note]].",
        )
        .await
        .unwrap();
    assert_eq!(created.permalink, "patterns/source-note");
    assert!(created.content.contains("tags: [\"fs\",\"core\"]"));

    let created_note = repo
        .get_by_permalink(&project_id, "patterns/source-note")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(created_note.title, "Source Note");
    assert_eq!(created_note.note_type, "pattern");
    assert_eq!(created_note.tags, "[\"fs\",\"core\"]");
    assert_eq!(repo.broken_links(&project_id, None).await.unwrap().len(), 0);
    let graph = repo.graph(&project_id).await.unwrap();
    assert_eq!(graph.edges.len(), 1);
    assert_eq!(graph.edges[0].source_id, created_note.id);
    assert_eq!(graph.edges[0].target_id, target.id);

    let updated = core
        .write_file(
            &project_id,
            project_dir.path(),
            "patterns/source-note.md",
            "---\ntitle: Source Note\ntype: pattern\ntags: [\"fs\",\"updated\"]\n---\n\nNow links to [[Missing Note]].",
        )
        .await
        .unwrap();
    assert!(updated.content.contains("tags: [\"fs\",\"updated\"]"));

    let updated_note = repo
        .get_by_permalink(&project_id, "patterns/source-note")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated_note.tags, "[\"fs\",\"updated\"]");
    assert_eq!(updated_note.content, "Now links to [[Missing Note]].");
    assert!(repo.graph(&project_id).await.unwrap().edges.is_empty());
    let broken = repo.broken_links(&project_id, None).await.unwrap();
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].source_id, updated_note.id);
    assert_eq!(broken[0].raw_text, "Missing Note");

    let renamed = core
        .rename_file(
            &project_id,
            project_dir.path(),
            "patterns/source-note.md",
            "research/renamed-note.md",
        )
        .await
        .unwrap();
    assert_eq!(renamed.logical_path, "research/renamed-note.md");
    assert_eq!(renamed.permalink, "research/renamed-note");
    assert!(matches!(
        core.read_file(&project_id, "patterns/source-note.md").await,
        Err(MemoryFsError::NotFound { .. })
    ));

    let renamed_note = repo
        .get_by_permalink(&project_id, "research/renamed-note")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renamed_note.title, "Renamed Note");
    assert_eq!(renamed_note.note_type, "research");
    assert_eq!(renamed_note.folder, "research");
    assert_eq!(renamed_note.tags, "[\"fs\",\"updated\"]");

    let renamed_file = core
        .read_file(&project_id, "research/renamed-note.md")
        .await
        .unwrap();
    assert!(
        renamed_file
            .content
            .starts_with("---\ntitle: Renamed Note\ntype: research")
    );
    assert!(
        renamed_file
            .content
            .contains("Now links to [[Missing Note]].")
    );

    core.delete_file(&project_id, "research/renamed-note.md")
        .await
        .unwrap();
    assert!(
        repo.get_by_permalink(&project_id, "research/renamed-note")
            .await
            .unwrap()
            .is_none()
    );
    assert!(repo.graph(&project_id).await.unwrap().edges.is_empty());
    assert!(
        repo.broken_links(&project_id, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        core.stat(&project_id, "research/renamed-note.md").await,
        Err(MemoryFsError::NotFound { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_backed_frontmatter_can_move_notes_across_folders_on_write() {
    let (core, db, project_id, project_dir) = make_core().await;
    let repo = NoteRepository::new(db.clone(), test_events());

    core.write_file(
        &project_id,
        project_dir.path(),
        "patterns/frontmatter-move.md",
        "---\ntitle: Frontmatter Move\ntype: pattern\ntags: [\"before\"]\n---\n\nOriginal body",
    )
    .await
    .unwrap();

    let rewritten = core
        .write_file(
            &project_id,
            project_dir.path(),
            "patterns/frontmatter-move.md",
            "---\ntitle: Architecture Decision\ntype: adr\ntags: [\"after\"]\n---\n\nUpdated body",
        )
        .await
        .unwrap();

    assert_eq!(rewritten.permalink, "decisions/architecture-decision");
    assert_eq!(
        rewritten.metadata.path,
        "decisions/architecture-decision.md"
    );
    assert!(matches!(
        core.read_file(&project_id, "patterns/frontmatter-move.md")
            .await,
        Err(MemoryFsError::NotFound { .. })
    ));

    let moved = repo
        .get_by_permalink(&project_id, "decisions/architecture-decision")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.title, "Architecture Decision");
    assert_eq!(moved.note_type, "adr");
    assert_eq!(moved.folder, Path::new("decisions").to_string_lossy());
    assert_eq!(moved.tags, "[\"after\"]");
    assert_eq!(moved.content, "Updated body");
}
