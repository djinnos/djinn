use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::db::connection::{Database, OptionalExt};
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::note::{Note, NoteSearchResult};

pub struct NoteRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl NoteRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
    }

    /// Create a new note. Writes the markdown file then inserts the index row.
    ///
    /// `project_path` is the root directory of the project (the `.djinn/`
    /// subdirectory is created automatically). `tags` must be a JSON array
    /// string, e.g. `'["rust","db"]'`.
    ///
    /// For singleton types (`brief`, `roadmap`) the note is inserted with
    /// a fixed permalink and file path. If the singleton already exists in the
    /// DB the caller must use `update` instead (the DB UNIQUE constraint will
    /// surface the conflict as an error).
    pub async fn create(
        &self,
        project_id: &str,
        project_path: &Path,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
    ) -> Result<Note> {
        let id = uuid::Uuid::now_v7().to_string();
        let permalink = permalink_for(note_type, title);
        let file_path = file_path_for(project_path, note_type, title);
        let file_path_str = file_path.to_string_lossy().to_string();

        let project_id = project_id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let note_type = note_type.to_owned();
        let folder = folder_for_type(&note_type).to_owned();
        let tags = tags.to_owned();

        // Write file to disk before inserting into DB. Directory creation is
        // attempted here; a failure returns an error before touching the DB.
        write_note_file(&file_path, &title, &note_type, &tags, &content)?;

        let note = self
            .db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO notes
                        (id, project_id, permalink, title, file_path,
                         note_type, folder, tags, content)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        &id, &project_id, &permalink, &title, &file_path_str,
                        &note_type, &folder, &tags, &content
                    ],
                )?;
                Ok(conn.query_row(NOTE_SELECT_WHERE_ID, [&id], row_to_note)?)
            })
            .await
            .map_err(|e| {
                // Best-effort cleanup: remove file if DB insert failed.
                let _ = std::fs::remove_file(&file_path);
                e
            })?;

        let _ = self.events.send(DjinnEvent::NoteCreated(note.clone()));
        Ok(note)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Note>> {
        let id = id.to_owned();
        self.db
            .call(move |conn| {
                Ok(conn.query_row(NOTE_SELECT_WHERE_ID, [&id], row_to_note).optional()?)
            })
            .await
    }

    pub async fn get_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> Result<Option<Note>> {
        let project_id = project_id.to_owned();
        let permalink = permalink.to_owned();
        self.db
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT id, project_id, permalink, title, file_path,
                            note_type, folder, tags, content,
                            created_at, updated_at, last_accessed
                     FROM notes WHERE project_id = ?1 AND permalink = ?2",
                    [&project_id, &permalink],
                    row_to_note,
                )
                .optional()?)
            })
            .await
    }

    /// List notes for a project, optionally filtered by folder.
    pub async fn list(
        &self,
        project_id: &str,
        folder: Option<&str>,
    ) -> Result<Vec<Note>> {
        let project_id = project_id.to_owned();
        let folder = folder.map(ToOwned::to_owned);
        self.db
            .call(move |conn| {
                let notes = if let Some(ref f) = folder {
                    let mut stmt = conn.prepare(
                        "SELECT id, project_id, permalink, title, file_path,
                                note_type, folder, tags, content,
                                created_at, updated_at, last_accessed
                         FROM notes WHERE project_id = ?1 AND folder = ?2
                         ORDER BY folder, title",
                    )?;
                    stmt.query_map([&project_id, f], row_to_note)?
                        .collect::<std::result::Result<Vec<_>, _>>()?
                } else {
                    let mut stmt = conn.prepare(
                        "SELECT id, project_id, permalink, title, file_path,
                                note_type, folder, tags, content,
                                created_at, updated_at, last_accessed
                         FROM notes WHERE project_id = ?1
                         ORDER BY folder, title",
                    )?;
                    stmt.query_map([&project_id], row_to_note)?
                        .collect::<std::result::Result<Vec<_>, _>>()?
                };
                Ok(notes)
            })
            .await
    }

    /// Update a note's title, content, and tags. The file is overwritten
    /// in-place (file_path and permalink stay fixed after creation).
    pub async fn update(
        &self,
        id: &str,
        title: &str,
        content: &str,
        tags: &str,
    ) -> Result<Note> {
        // Fetch current note to get file_path and note_type.
        let current = self.get(id).await?.ok_or_else(|| {
            Error::Internal(format!("note not found: {id}"))
        })?;

        write_note_file(
            Path::new(&current.file_path),
            title,
            &current.note_type,
            tags,
            content,
        )?;

        let id = id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let tags = tags.to_owned();

        let note: Note = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE notes SET
                        title   = ?2,
                        content = ?3,
                        tags    = ?4,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![&id, &title, &content, &tags],
                )?;
                Ok(conn.query_row(NOTE_SELECT_WHERE_ID, [&id], row_to_note)?)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::NoteUpdated(note.clone()));
        Ok(note)
    }

    /// Delete a note. Removes the DB row (and FTS5 index via trigger) first,
    /// then attempts to remove the file from disk.
    pub async fn delete(&self, id: &str) -> Result<()> {
        // Fetch file_path before deleting from DB.
        let current = self.get(id).await?.ok_or_else(|| {
            Error::Internal(format!("note not found: {id}"))
        })?;

        let id_owned = id.to_owned();
        let id_for_event = id.to_owned();

        self.db
            .write(move |conn| {
                conn.execute("DELETE FROM notes WHERE id = ?1", [&id_owned])?;
                Ok(())
            })
            .await?;

        // Best-effort file removal — don't fail if file is already gone.
        let _ = std::fs::remove_file(&current.file_path);

        let _ = self.events.send(DjinnEvent::NoteDeleted { id: id_for_event });
        Ok(())
    }

    /// Update `last_accessed` without emitting a change event (read-access
    /// tracking should not flood the SSE stream).
    pub async fn touch_accessed(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE notes SET
                        last_accessed = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE id = ?1",
                    [&id],
                )?;
                Ok(())
            })
            .await
    }

    /// Full-text search with BM25 ranking and content snippets.
    ///
    /// `query` is an FTS5 query string (e.g. `"rust database"`).
    /// Results are ordered by relevance (best match first).
    pub async fn search(
        &self,
        project_id: &str,
        query: &str,
        folder: Option<&str>,
        note_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>> {
        let project_id = project_id.to_owned();
        let query = query.to_owned();
        let folder = folder.unwrap_or("").to_owned();
        let note_type = note_type.unwrap_or("").to_owned();
        let limit = limit as i64;

        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,
                            snippet(notes_fts, 1, '<b>', '</b>', '...', 32)
                     FROM notes_fts
                     JOIN notes n ON notes_fts.rowid = n.rowid
                     WHERE notes_fts MATCH ?1
                       AND n.project_id = ?2
                       AND (?3 = '' OR n.folder = ?3)
                       AND (?4 = '' OR n.note_type = ?4)
                     ORDER BY bm25(notes_fts)
                     LIMIT ?5",
                )?;
                let results = stmt
                    .query_map(
                        rusqlite::params![&query, &project_id, &folder, &note_type, limit],
                        |row| {
                            Ok(NoteSearchResult {
                                id: row.get(0)?,
                                permalink: row.get(1)?,
                                title: row.get(2)?,
                                folder: row.get(3)?,
                                note_type: row.get(4)?,
                                snippet: row.get(5)?,
                            })
                        },
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(results)
            })
            .await
    }

    /// Generate a markdown catalog (table of contents) for all notes in a
    /// project, grouped by folder and sorted alphabetically within each.
    pub async fn catalog(&self, project_id: &str) -> Result<String> {
        let project_id = project_id.to_owned();
        let notes = self
            .db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT folder, title, permalink, updated_at
                     FROM notes WHERE project_id = ?1
                     ORDER BY folder, title",
                )?;
                let rows = stmt
                    .query_map([&project_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;

        Ok(build_catalog(&notes))
    }
}

// ── SQL constant ─────────────────────────────────────────────────────────────

const NOTE_SELECT_WHERE_ID: &str =
    "SELECT id, project_id, permalink, title, file_path,
            note_type, folder, tags, content,
            created_at, updated_at, last_accessed
     FROM notes WHERE id = ?1";

// ── Row mapper ───────────────────────────────────────────────────────────────

fn row_to_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        project_id: row.get(1)?,
        permalink: row.get(2)?,
        title: row.get(3)?,
        file_path: row.get(4)?,
        note_type: row.get(5)?,
        folder: row.get(6)?,
        tags: row.get(7)?,
        content: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        last_accessed: row.get(11)?,
    })
}

// ── Note type helpers ────────────────────────────────────────────────────────

/// Return the storage folder for a given note type.
///
/// Singleton types (`brief`, `roadmap`) map to `""` (project .djinn/ root).
pub fn folder_for_type(note_type: &str) -> &'static str {
    match note_type {
        "adr" => "decisions",
        "pattern" => "patterns",
        "research" => "research",
        "requirement" => "requirements",
        "reference" => "reference",
        "design" => "design",
        "session" => "research/sessions",
        "persona" => "design/personas",
        "journey" => "design/journeys",
        "design_spec" => "design/specs",
        "competitive" => "research/competitive",
        "tech_spike" => "research/technical",
        // Singletons live at the .djinn/ root, no subfolder.
        "brief" | "roadmap" => "",
        // Unknown types fall back to reference.
        _ => "reference",
    }
}

/// Returns `true` for note types that have exactly one instance per project.
pub fn is_singleton(note_type: &str) -> bool {
    matches!(note_type, "brief" | "roadmap")
}

/// Derive the project-scoped permalink for a note.
///
/// Singletons use their type name as the permalink (`"brief"`, `"roadmap"`).
/// Other types use `"{folder}/{slug}"`.
pub fn permalink_for(note_type: &str, title: &str) -> String {
    if is_singleton(note_type) {
        return note_type.to_string();
    }
    let folder = folder_for_type(note_type);
    let slug = slugify(title);
    if folder.is_empty() {
        slug
    } else {
        format!("{folder}/{slug}")
    }
}

/// Return the absolute path where a note's markdown file should be stored.
pub fn file_path_for(project_path: &Path, note_type: &str, title: &str) -> PathBuf {
    let djinn = project_path.join(".djinn");
    if is_singleton(note_type) {
        return djinn.join(format!("{note_type}.md"));
    }
    let folder = folder_for_type(note_type);
    let slug = slugify(title);
    if folder.is_empty() {
        djinn.join(format!("{slug}.md"))
    } else {
        djinn.join(folder).join(format!("{slug}.md"))
    }
}

/// Convert a title into a URL-safe slug.
pub fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    // Collapse repeated dashes and trim leading/trailing.
    slug.split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// ── File I/O ─────────────────────────────────────────────────────────────────

/// Write (or overwrite) a note's markdown file with YAML frontmatter.
fn write_note_file(
    file_path: &Path,
    title: &str,
    note_type: &str,
    tags: &str,
    content: &str,
) -> Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Internal(format!("create_dir_all {}: {e}", parent.display()))
        })?;
    }
    let file_content = format!(
        "---\ntitle: {title}\ntype: {note_type}\ntags: {tags}\n---\n\n{content}",
    );
    std::fs::write(file_path, file_content).map_err(|e| {
        Error::Internal(format!("write note file {}: {e}", file_path.display()))
    })?;
    Ok(())
}

// ── Catalog builder ──────────────────────────────────────────────────────────

fn build_catalog(notes: &[(String, String, String, String)]) -> String {
    if notes.is_empty() {
        return "# Knowledge Base\n\n*No notes yet.*\n".to_string();
    }

    let mut out = String::from("# Knowledge Base\n");
    let mut current_folder = String::new();

    for (folder, title, permalink, _) in notes {
        let header = if folder.is_empty() { "root" } else { folder.as_str() };
        if header != current_folder.as_str() {
            out.push('\n');
            out.push_str(&format!("## {header}\n\n"));
            current_folder = header.to_string();
        }
        out.push_str(&format!("- [{title}]({permalink})\n"));
    }

    out
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::project::ProjectRepository;
    use crate::test_helpers;
    use tokio::sync::broadcast;

    async fn make_project(
        db: &Database,
        tx: broadcast::Sender<DjinnEvent>,
        path: &Path,
    ) -> crate::models::project::Project {
        ProjectRepository::new(db.clone(), tx)
            .create("test-project", path.to_str().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_and_get_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

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

    #[tokio::test]
    async fn singleton_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

        let note = repo
            .create(&project.id, tmp.path(), "Project Brief", "...", "brief", "[]")
            .await
            .unwrap();

        assert_eq!(note.permalink, "brief");
        assert!(note.file_path.ends_with(".djinn/brief.md"));
    }

    #[tokio::test]
    async fn get_by_permalink() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

        let note = repo
            .create(&project.id, tmp.path(), "A Pattern", "body", "pattern", "[]")
            .await
            .unwrap();

        let found = repo
            .get_by_permalink(&project.id, &note.permalink)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, note.id);
    }

    #[tokio::test]
    async fn update_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let _ = rx.recv().await.unwrap(); // ProjectCreated
        let repo = NoteRepository::new(db, tx);

        let note = repo
            .create(&project.id, tmp.path(), "Original", "old content", "research", "[]")
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap(); // NoteCreated

        let updated = repo
            .update(&note.id, "Original", "new content", r#"["updated"]"#)
            .await
            .unwrap();
        assert_eq!(updated.content, "new content");
        assert_eq!(updated.tags, r#"["updated"]"#);

        match rx.recv().await.unwrap() {
            DjinnEvent::NoteUpdated(n) => assert_eq!(n.content, "new content"),
            _ => panic!("expected NoteUpdated"),
        }
    }

    #[tokio::test]
    async fn delete_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let _ = rx.recv().await.unwrap();
        let repo = NoteRepository::new(db, tx);

        let note = repo
            .create(&project.id, tmp.path(), "To Delete", "body", "reference", "[]")
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        let file_path = note.file_path.clone();

        repo.delete(&note.id).await.unwrap();
        assert!(repo.get(&note.id).await.unwrap().is_none());
        assert!(!Path::new(&file_path).exists());

        match rx.recv().await.unwrap() {
            DjinnEvent::NoteDeleted { id } => assert_eq!(id, note.id),
            _ => panic!("expected NoteDeleted"),
        }
    }

    #[tokio::test]
    async fn fts5_search() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

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
            "Authentication Strategy",
            "Using Clerk JWT for all MCP connections.",
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

    #[tokio::test]
    async fn fts5_search_folder_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

        repo.create(&project.id, tmp.path(), "Design Note", "common term", "design", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "Research Note", "common term", "research", "[]")
            .await
            .unwrap();

        let results = repo
            .search(&project.id, "common", Some("design"), None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].folder, "design");
    }

    #[tokio::test]
    async fn catalog_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

        repo.create(&project.id, tmp.path(), "First ADR", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "Second ADR", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "A Pattern", "body", "pattern", "[]")
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

    #[tokio::test]
    async fn list_with_folder_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

        repo.create(&project.id, tmp.path(), "ADR One", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(&project.id, tmp.path(), "Research One", "body", "research", "[]")
            .await
            .unwrap();

        let decisions = repo.list(&project.id, Some("decisions")).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].title, "ADR One");

        let all = repo.list(&project.id, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn create_emits_event() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let _ = rx.recv().await.unwrap();
        let repo = NoteRepository::new(db, tx);

        repo.create(&project.id, tmp.path(), "Event Note", "body", "design", "[]")
            .await
            .unwrap();

        match rx.recv().await.unwrap() {
            DjinnEvent::NoteCreated(n) => assert_eq!(n.title, "Event Note"),
            _ => panic!("expected NoteCreated"),
        }
    }

    #[tokio::test]
    async fn slugify_roundtrip() {
        assert_eq!(slugify("My ADR Title"), "my-adr-title");
        assert_eq!(slugify("Hello  World"), "hello-world");
        assert_eq!(slugify("--leading dashes--"), "leading-dashes");
        assert_eq!(slugify("rust/database"), "rust-database");
    }

    #[tokio::test]
    async fn touch_accessed_does_not_emit_event() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let _ = rx.recv().await.unwrap();
        let repo = NoteRepository::new(db, tx);

        let note = repo
            .create(&project.id, tmp.path(), "Touch Me", "body", "reference", "[]")
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap(); // NoteCreated

        repo.touch_accessed(&note.id).await.unwrap();

        // No event should be in the channel.
        assert!(rx.try_recv().is_err());
    }
}
