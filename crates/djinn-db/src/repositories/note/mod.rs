use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sqlx::Sqlite;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    BrokenLink, GraphEdge, GraphNode, GraphResponse, HealthReport, Note, NoteCompact,
    NoteSearchResult, OrphanNote, ReindexSummary, StaleFolder,
};

use crate::database::Database;
use crate::error::{DbError as Error, DbResult as Result};

mod crud;
mod file_helpers;
mod graph;
mod indexing;
mod search;

use file_helpers::{build_catalog, write_note_file};
pub use file_helpers::{file_path_for, folder_for_type, is_singleton, permalink_for, slugify};
use indexing::{index_links_for_note, resolve_links_for_note};

// ── SQL constant ─────────────────────────────────────────────────────────────

const NOTE_SELECT_WHERE_ID: &str = "SELECT id, project_id, permalink, title, file_path,
            note_type, folder, tags, content,
            created_at, updated_at, last_accessed
     FROM notes WHERE id = ?1";

// ── Repository ────────────────────────────────────────────────────────────────

pub struct NoteRepository {
    db: Database,
    events: EventBus,
}

impl NoteRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::{Note, Project};
    use tokio::sync::broadcast;

    use crate::database::Database;

    use super::*;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, path: &Path) -> Project {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)")
            .bind(&id)
            .bind("test-project")
            .bind(path.to_str().unwrap())
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query_as::<_, Project>(
            "SELECT id, name, path, created_at, target_branch, auto_merge, sync_enabled, sync_remote \
             FROM projects WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "My ADR",
                "This is the content.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        assert_eq!(note.title, "My ADR");
        assert_eq!(note.note_type, "adr");
        assert_eq!(note.folder, "decisions");
        assert_eq!(note.permalink, "decisions/my-adr");
        assert!(note.file_path.ends_with("decisions/my-adr.md"));

        // File should exist on disk.
        assert!(Path::new(&note.file_path).exists());

        // Should be retrievable.
        let fetched = repo.get(&note.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My ADR");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn singleton_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "Project Brief",
                "...",
                "brief",
                "[]",
            )
            .await
            .unwrap();

        assert_eq!(note.permalink, "brief");
        assert!(note.file_path.ends_with(".djinn/brief.md"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_by_permalink() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "A Pattern",
                "body",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let found = repo
            .get_by_permalink(&project.id, &note.permalink)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, note.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "Original",
                "old content",
                "research",
                "[]",
            )
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap(); // NoteCreated

        let updated = repo
            .update(&note.id, "Original", "new content", r#"["updated"]"#)
            .await
            .unwrap();
        assert_eq!(updated.content, "new content");
        assert_eq!(updated.tags, r#"["updated"]"#);

        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.entity_type, "note");
        assert_eq!(envelope.action, "updated");
        let n: Note = envelope.parse_payload().unwrap();
        assert_eq!(n.content, "new content");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "To Delete",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        let file_path = note.file_path.clone();

        repo.delete(&note.id).await.unwrap();
        assert!(repo.get(&note.id).await.unwrap().is_none());
        assert!(!Path::new(&file_path).exists());

        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.entity_type, "note");
        assert_eq!(envelope.action, "deleted");
        assert_eq!(envelope.payload["id"].as_str().unwrap(), note.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fts5_search() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(
            &project.id,
            tmp.path(),
            "Rust Database Choice",
            "We chose rusqlite for its simplicity and bundled SQLite.",
            "adr",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "Connection Strategy",
            "Use direct MCP connections for local operation.",
            "adr",
            "[]",
        )
        .await
        .unwrap();

        // Search for "rusqlite" — should hit only the first note.
        let results = repo
            .search(&project.id, "rusqlite", None, None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Database Choice");
        assert!(results[0].snippet.contains("rusqlite"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fts5_search_folder_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(
            &project.id,
            tmp.path(),
            "Design Note",
            "common term",
            "design",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "Research Note",
            "common term",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let results = repo
            .search(&project.id, "common", Some("design"), None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].folder, "design");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(&project.id, tmp.path(), "First ADR", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "Second ADR", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "A Pattern",
            "body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

        let catalog = repo.catalog(&project.id).await.unwrap();
        assert!(catalog.contains("# Knowledge Base"));
        assert!(catalog.contains("## decisions"));
        assert!(catalog.contains("First ADR"));
        assert!(catalog.contains("Second ADR"));
        assert!(catalog.contains("## patterns"));
        assert!(catalog.contains("A Pattern"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_with_folder_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(&project.id, tmp.path(), "ADR One", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "Research One",
            "body",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let decisions = repo.list(&project.id, Some("decisions")).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].title, "ADR One");

        let all = repo.list(&project.id, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(
            &project.id,
            tmp.path(),
            "Event Note",
            "body",
            "design",
            "[]",
        )
        .await
        .unwrap();

        let envelope = rx.recv().await.unwrap();
        assert_eq!(envelope.entity_type, "note");
        assert_eq!(envelope.action, "created");
        let n: Note = envelope.parse_payload().unwrap();
        assert_eq!(n.title, "Event Note");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slugify_roundtrip() {
        assert_eq!(slugify("My ADR Title"), "my-adr-title");
        assert_eq!(slugify("Hello  World"), "hello-world");
        assert_eq!(slugify("--leading dashes--"), "leading-dashes");
        assert_eq!(slugify("rust/database"), "rust-database");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn touch_accessed_does_not_emit_event() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "Touch Me",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap(); // NoteCreated

        repo.touch_accessed(&note.id).await.unwrap();

        // No event should be in the channel.
        assert!(rx.try_recv().is_err());
    }

    // ── Wikilink graph tests ──────────────────────────────────────────────────

    #[test]
    fn extract_wikilinks_basic() {
        let links = indexing::extract_wikilinks("See [[Rust Database Choice]] for details.");
        assert_eq!(links, vec![("Rust Database Choice".to_string(), None)]);
    }

    #[test]
    fn extract_wikilinks_with_display() {
        let links = indexing::extract_wikilinks("See [[Rust DB|the ADR]] for details.");
        assert_eq!(
            links,
            vec![("Rust DB".to_string(), Some("the ADR".to_string()))]
        );
    }

    #[test]
    fn extract_wikilinks_multiple() {
        let links = indexing::extract_wikilinks("[[A]] and [[B|Bee]] and [[C]]");
        assert_eq!(links.len(), 3);
        assert_eq!(links[0], ("A".to_string(), None));
        assert_eq!(links[1], ("B".to_string(), Some("Bee".to_string())));
        assert_eq!(links[2], ("C".to_string(), None));
    }

    #[test]
    fn extract_wikilinks_empty_and_none() {
        let links = indexing::extract_wikilinks("No links here. [[]] empty.");
        assert!(links.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wikilink_resolves_on_create() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        // Create target first.
        let target = repo
            .create(
                &project.id,
                tmp.path(),
                "Connection Strategy",
                "body",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Create source with a wikilink to the target by title.
        repo.create(
            &project.id,
            tmp.path(),
            "Overview",
            "See [[Connection Strategy]] for details.",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let graph = repo.graph(&project.id).await.unwrap();
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].target_id, target.id);
        assert_eq!(graph.edges[0].raw_text, "Connection Strategy");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broken_link_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(
            &project.id,
            tmp.path(),
            "Source Note",
            "Links to [[Missing Note]] which does not exist.",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let broken = repo.broken_links(&project.id, None).await.unwrap();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].raw_text, "Missing Note");
        assert_eq!(broken[0].source_title, "Source Note");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orphan_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        // Two notes: source links to target, isolated is orphaned.
        let target = repo
            .create(&project.id, tmp.path(), "Target", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "Source",
            "See [[Target]].",
            "research",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "Isolated",
            "no links",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

        let orphans = repo.orphans(&project.id, None).await.unwrap();
        // Target has an inbound link; Source and Isolated do not.
        let orphan_titles: Vec<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
        assert!(
            !orphan_titles.contains(&target.title.as_str()),
            "target should not be orphan"
        );
        assert!(
            orphan_titles.contains(&"Source"),
            "Source has no inbound links"
        );
        assert!(
            orphan_titles.contains(&"Isolated"),
            "Isolated has no inbound links"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_previously_broken_links_on_create() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        // Create source first (target doesn't exist yet → broken link).
        repo.create(
            &project.id,
            tmp.path(),
            "Source",
            "See [[Future Note]].",
            "research",
            "[]",
        )
        .await
        .unwrap();
        assert_eq!(repo.broken_links(&project.id, None).await.unwrap().len(), 1);

        // Now create the target → broken link should be resolved.
        repo.create(&project.id, tmp.path(), "Future Note", "body", "adr", "[]")
            .await
            .unwrap();
        assert_eq!(repo.broken_links(&project.id, None).await.unwrap().len(), 0);
        assert_eq!(repo.graph(&project.id).await.unwrap().edges.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_from_disk_detects_created_updated_and_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let decisions_dir = tmp.path().join(".djinn").join("decisions");
        std::fs::create_dir_all(&decisions_dir).unwrap();

        let existing_path = decisions_dir.join("existing.md");
        std::fs::write(
            &existing_path,
            "---\ntitle: Existing\ntype: adr\ntags: []\n---\n\noriginal content",
        )
        .unwrap();

        let first = repo
            .reindex_from_disk(&project.id, tmp.path())
            .await
            .unwrap();
        assert_eq!(first.created, 1);
        assert_eq!(first.updated, 0);
        assert_eq!(first.deleted, 0);

        // Modify existing + add one new file.
        std::fs::write(
            &existing_path,
            "---\ntitle: Existing\ntype: adr\ntags: []\n---\n\nupdated content",
        )
        .unwrap();
        std::fs::write(
            decisions_dir.join("new-note.md"),
            "---\ntitle: New Note\ntype: adr\ntags: []\n---\n\nhello",
        )
        .unwrap();

        let second = repo
            .reindex_from_disk(&project.id, tmp.path())
            .await
            .unwrap();
        assert_eq!(second.created, 1);
        assert_eq!(second.updated, 1);
        assert_eq!(second.deleted, 0);

        // Delete one file from disk.
        std::fs::remove_file(decisions_dir.join("new-note.md")).unwrap();
        let third = repo
            .reindex_from_disk(&project.id, tmp.path())
            .await
            .unwrap();
        assert_eq!(third.deleted, 1);
    }
}
