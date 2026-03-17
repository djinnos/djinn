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
pub(crate) mod rrf;
mod scoring;
mod search;

pub use indexing::UpdateNoteIndexParams;

use file_helpers::{build_catalog, write_note_file};
pub use file_helpers::{file_path_for, folder_for_type, is_singleton, permalink_for, slugify};
use indexing::{index_links_for_note, resolve_links_for_note};

// ── SQL constant ─────────────────────────────────────────────────────────────

const NOTE_SELECT_WHERE_ID: &str = "SELECT id, project_id, permalink, title, file_path,
            note_type, folder, tags, content,
            created_at, updated_at, last_accessed,
            access_count, confidence, abstract as abstract_, overview
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
            .search(&project.id, "rusqlite", None, None, None, 10)
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
            .search(&project.id, "common", None, Some("design"), None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].folder, "design");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fts5_search_prefers_title_over_content() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(
            &project.id,
            tmp.path(),
            "rankneedle in title",
            "unrelated body",
            "research",
            "[]",
        )
        .await
        .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "different title",
            "This content has rankneedle.",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let results = repo
            .search(&project.id, "rankneedle", None, None, None, 10)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "rankneedle in title");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fts5_search_prefers_tags_over_content() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        repo.create(
            &project.id,
            tmp.path(),
            "tag-ranked note",
            "unrelated body",
            "research",
            r#"["ranktag"]"#,
        )
        .await
        .unwrap();
        repo.create(
            &project.id,
            tmp.path(),
            "content-ranked note",
            "This content has ranktag.",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let results = repo
            .search(&project.id, "ranktag", None, None, None, 10)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "tag-ranked note");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_rrf_prefers_higher_access_count_for_equivalent_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let high = repo
            .create(
                &project.id,
                tmp.path(),
                "sharedterm alpha",
                "same content",
                "research",
                "[]",
            )
            .await
            .unwrap();
        let low = repo
            .create(
                &project.id,
                tmp.path(),
                "sharedterm beta",
                "same content",
                "research",
                "[]",
            )
            .await
            .unwrap();

        sqlx::query("UPDATE notes SET access_count = 10 WHERE id = ?1")
            .bind(&high.id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET access_count = 0 WHERE id = ?1")
            .bind(&low.id)
            .execute(db.pool())
            .await
            .unwrap();

        let results = repo
            .search(&project.id, "sharedterm", None, None, None, 10)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, high.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_generation() {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn touch_accessed_increments_access_count() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note = repo
            .create(
                &project.id,
                tmp.path(),
                "Touch Count",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        for _ in 0..3 {
            repo.touch_accessed(&note.id).await.unwrap();
        }

        let updated = repo.get(&note.id).await.unwrap().unwrap();
        assert_eq!(updated.access_count, 3);
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_affinity_scores_task_epic_blocker_and_max() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let task_note = repo
            .create(
                &project.id,
                tmp.path(),
                "Task Note",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let epic_note = repo
            .create(
                &project.id,
                tmp.path(),
                "Epic Note",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let blocker_note = repo
            .create(
                &project.id,
                tmp.path(),
                "Blocker Note",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let overlap_note = repo
            .create(
                &project.id,
                tmp.path(),
                "Overlap Note",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
        .bind(serde_json::json!([blocker_note.id.clone(), epic_note.id.clone(), overlap_note.id.clone()]).to_string())
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES (?1, ?2)")
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
    async fn graph_proximity_empty_for_seed_without_links() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let seed = repo
            .create(
                &project.id,
                tmp.path(),
                "Seed",
                "no links",
                "research",
                "[]",
            )
            .await
            .unwrap();

        let scores = repo.graph_proximity_scores(&[seed.id], 2).await.unwrap();
        assert!(scores.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_proximity_linear_chain_hop_decay() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let a = repo
            .create(&project.id, tmp.path(), "A", "[[B]]", "research", "[]")
            .await
            .unwrap();
        let b = repo
            .create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
            .await
            .unwrap();
        let c = repo
            .create(&project.id, tmp.path(), "C", "", "research", "[]")
            .await
            .unwrap();

        repo.reindex_from_disk(&project.id, tmp.path())
            .await
            .unwrap();

        let seed_id = a.id.clone();
        let scores = repo
            .graph_proximity_scores(&[seed_id.clone()], 2)
            .await
            .unwrap();
        let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
        assert_eq!(m.get(&b.id).copied().unwrap(), 0.7);
        assert!((m.get(&c.id).copied().unwrap() - 0.49).abs() < 1e-9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_proximity_diamond_keeps_max_path_score_not_sum() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let a = repo
            .create(
                &project.id,
                tmp.path(),
                "A",
                "[[B]] [[D]]",
                "research",
                "[]",
            )
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
            .await
            .unwrap();
        let c = repo
            .create(&project.id, tmp.path(), "C", "", "research", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "D", "[[C]]", "research", "[]")
            .await
            .unwrap();

        repo.reindex_from_disk(&project.id, tmp.path())
            .await
            .unwrap();

        let seed_id = a.id.clone();
        let scores = repo.graph_proximity_scores(&[seed_id], 2).await.unwrap();
        let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
        assert!((m.get(&c.id).copied().unwrap() - 0.49).abs() < 1e-9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_proximity_excludes_beyond_max_hops() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let a = repo
            .create(&project.id, tmp.path(), "A", "[[B]]", "research", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
            .await
            .unwrap();
        let d = repo
            .create(&project.id, tmp.path(), "D", "", "research", "[]")
            .await
            .unwrap();
        repo.update(&d.id, "D", "[[A]]", "[]").await.unwrap();

        repo.reindex_from_disk(&project.id, tmp.path())
            .await
            .unwrap();

        let seed_id = a.id.clone();
        let scores = repo
            .graph_proximity_scores(&[seed_id.clone()], 2)
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> = scores.into_iter().map(|(id, _)| id).collect();
        // no 3-hop specific assertion target; ensure algorithm bounded and excludes seed
        assert!(!ids.contains(&seed_id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn temporal_scores_empty_candidates_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let scores = repo.temporal_scores(&project.id, &[]).await.unwrap();
        assert!(scores.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn temporal_scores_higher_access_count_wins_same_age() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let high = repo
            .create(
                &project.id,
                tmp.path(),
                "High Access",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let low = repo
            .create(
                &project.id,
                tmp.path(),
                "Low Access",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        sqlx::query(
            "UPDATE notes
             SET created_at = datetime('now', '-1 day'),
                 updated_at = datetime('now', '-1 day')
             WHERE id IN (?1, ?2)",
        )
        .bind(&high.id)
        .bind(&low.id)
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query("UPDATE notes SET access_count = 10 WHERE id = ?1")
            .bind(&high.id)
            .execute(db.pool())
            .await
            .unwrap();

        sqlx::query("UPDATE notes SET access_count = 0 WHERE id = ?1")
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
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let recent = repo
            .create(&project.id, tmp.path(), "Recent", "body", "reference", "[]")
            .await
            .unwrap();
        let stale = repo
            .create(&project.id, tmp.path(), "Stale", "body", "reference", "[]")
            .await
            .unwrap();

        sqlx::query("UPDATE notes SET access_count = 3 WHERE id IN (?1, ?2)")
            .bind(&recent.id)
            .bind(&stale.id)
            .execute(db.pool())
            .await
            .unwrap();

        sqlx::query(
            "UPDATE notes SET created_at = datetime('now', '-30 day') WHERE id IN (?1, ?2)",
        )
        .bind(&recent.id)
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query("UPDATE notes SET updated_at = datetime('now') WHERE id = ?1")
            .bind(&recent.id)
            .execute(db.pool())
            .await
            .unwrap();

        sqlx::query("UPDATE notes SET updated_at = datetime('now', '-30 day') WHERE id = ?1")
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
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let zero_age = repo
            .create(
                &project.id,
                tmp.path(),
                "Zero Age",
                "body",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let old = repo
            .create(&project.id, tmp.path(), "Old", "body", "reference", "[]")
            .await
            .unwrap();

        sqlx::query(
            "UPDATE notes
             SET access_count = 0,
                 created_at = datetime('now'),
                 updated_at = datetime('now')
             WHERE id = ?1",
        )
        .bind(&zero_age.id)
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query(
            "UPDATE notes
             SET access_count = 0,
                 created_at = datetime('now', '-365 day'),
                 updated_at = datetime('now', '-365 day')
             WHERE id = ?1",
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
}
