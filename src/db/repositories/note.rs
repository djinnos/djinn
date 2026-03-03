use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::db::connection::{Database, OptionalExt};
use crate::error::{Error, Result};
use crate::events::DjinnEvent;
use crate::models::note::{
    BrokenLink, GraphEdge, GraphNode, GraphResponse, HealthReport, Note, NoteCompact,
    NoteSearchResult, OrphanNote, StaleFolder,
};

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
                index_links_for_note(conn, &id, &project_id, &content)?;
                resolve_links_for_note(conn, &id, &title, &permalink, &project_id)?;
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

        let permalink = current.permalink.clone();

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
                index_links_for_note(conn, &id, &current.project_id, &content)?;
                resolve_links_for_note(conn, &id, &title, &permalink, &current.project_id)?;
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

    // ── Wikilink graph ────────────────────────────────────────────────────────

    /// Full knowledge graph for a project: all notes as nodes and all resolved
    /// wikilink edges. `connection_count` = inbound + outbound resolved edges.
    pub async fn graph(&self, project_id: &str) -> Result<GraphResponse> {
        let project_id = project_id.to_owned();
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT n.id, n.permalink, n.title, n.note_type, n.folder,
                            (SELECT COUNT(*) FROM note_links WHERE source_id = n.id
                               AND target_id IS NOT NULL)
                            + (SELECT COUNT(*) FROM note_links WHERE target_id = n.id)
                              AS connection_count
                     FROM notes n
                     WHERE n.project_id = ?1
                     ORDER BY n.folder, n.title",
                )?;
                let nodes = stmt
                    .query_map([&project_id], |row| {
                        Ok(GraphNode {
                            id: row.get(0)?,
                            permalink: row.get(1)?,
                            title: row.get(2)?,
                            note_type: row.get(3)?,
                            folder: row.get(4)?,
                            connection_count: row.get(5)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;

                let mut stmt = conn.prepare(
                    "SELECT l.source_id, l.target_id, l.target_raw
                     FROM note_links l
                     JOIN notes src ON src.id = l.source_id AND src.project_id = ?1
                     WHERE l.target_id IS NOT NULL",
                )?;
                let edges = stmt
                    .query_map([&project_id], |row| {
                        Ok(GraphEdge {
                            source_id: row.get(0)?,
                            target_id: row.get(1)?,
                            raw_text: row.get(2)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;

                Ok(GraphResponse { nodes, edges })
            })
            .await
    }

    /// All wikilinks in a project whose target note does not exist.
    /// Optionally filtered to links whose source note is in `folder`.
    pub async fn broken_links(
        &self,
        project_id: &str,
        folder: Option<&str>,
    ) -> Result<Vec<BrokenLink>> {
        let project_id = project_id.to_owned();
        let folder = folder.map(ToOwned::to_owned);
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT src.id, src.permalink, src.title, l.target_raw
                     FROM note_links l
                     JOIN notes src ON src.id = l.source_id AND src.project_id = ?1
                     WHERE l.target_id IS NULL
                       AND (?2 IS NULL OR src.folder = ?2)
                     ORDER BY src.permalink, l.target_raw",
                )?;
                Ok(stmt
                    .query_map(rusqlite::params![&project_id, &folder], |row| {
                        Ok(BrokenLink {
                            source_id: row.get(0)?,
                            source_permalink: row.get(1)?,
                            source_title: row.get(2)?,
                            raw_text: row.get(3)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await
    }

    /// Notes with zero inbound wikilinks (potential dead-ends).
    /// Singleton types (`brief`, `roadmap`) are excluded.
    /// Optionally filtered by `folder`.
    pub async fn orphans(
        &self,
        project_id: &str,
        folder: Option<&str>,
    ) -> Result<Vec<OrphanNote>> {
        let project_id = project_id.to_owned();
        let folder = folder.map(ToOwned::to_owned);
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT n.id, n.permalink, n.title, n.note_type, n.folder
                     FROM notes n
                     WHERE n.project_id = ?1
                       AND n.note_type NOT IN ('brief', 'roadmap')
                       AND (?2 IS NULL OR n.folder = ?2)
                       AND NOT EXISTS (
                           SELECT 1 FROM note_links l WHERE l.target_id = n.id
                       )
                     ORDER BY n.folder, n.title",
                )?;
                Ok(stmt
                    .query_map(rusqlite::params![&project_id, &folder], |row| {
                        Ok(OrphanNote {
                            id: row.get(0)?,
                            permalink: row.get(1)?,
                            title: row.get(2)?,
                            note_type: row.get(3)?,
                            folder: row.get(4)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?)
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

    /// Move a note to a new location (rename file, update permalink and folder).
    ///
    /// The new position is described by `new_note_type` + `new_title`. If the type
    /// changes the file moves to the new type's folder. The content and tags are
    /// preserved unchanged.
    pub async fn move_note(
        &self,
        id: &str,
        project_path: &Path,
        new_title: &str,
        new_note_type: &str,
    ) -> Result<Note> {
        let current = self.get(id).await?.ok_or_else(|| {
            Error::Internal(format!("note not found: {id}"))
        })?;

        let new_file_path = file_path_for(project_path, new_note_type, new_title);
        let new_permalink = permalink_for(new_note_type, new_title);
        let new_folder = folder_for_type(new_note_type).to_owned();
        let new_file_path_str = new_file_path.to_string_lossy().to_string();

        // Create destination directory and rename the file.
        if let Some(parent) = new_file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Internal(format!("create_dir_all {}: {e}", parent.display()))
            })?;
        }
        std::fs::rename(&current.file_path, &new_file_path).map_err(|e| {
            Error::Internal(format!(
                "rename {} → {}: {e}",
                current.file_path,
                new_file_path.display()
            ))
        })?;
        // Rewrite frontmatter to reflect new title/type.
        write_note_file(&new_file_path, new_title, new_note_type, &current.tags, &current.content)?;

        let id = id.to_owned();
        let new_title = new_title.to_owned();
        let new_note_type = new_note_type.to_owned();
        let project_id = current.project_id.clone();

        let note: Note = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE notes SET
                        title      = ?2,
                        file_path  = ?3,
                        note_type  = ?4,
                        folder     = ?5,
                        permalink  = ?6,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE id = ?1",
                    rusqlite::params![
                        &id,
                        &new_title,
                        &new_file_path_str,
                        &new_note_type,
                        &new_folder,
                        &new_permalink,
                    ],
                )?;
                // Re-resolve previously-broken links that match the new title/permalink.
                resolve_links_for_note(conn, &id, &new_title, &new_permalink, &project_id)?;
                Ok(conn.query_row(NOTE_SELECT_WHERE_ID, [&id], row_to_note)?)
            })
            .await?;

        let _ = self.events.send(DjinnEvent::NoteUpdated(note.clone()));
        Ok(note)
    }

    /// List recently updated notes for a project, ordered by `updated_at` descending.
    ///
    /// `hours` limits to notes updated within the last N hours (0 = no limit).
    pub async fn recent(
        &self,
        project_id: &str,
        hours: i64,
        limit: i64,
    ) -> Result<Vec<NoteCompact>> {
        let project_id = project_id.to_owned();
        self.db
            .call(move |conn| {
                let sql = if hours > 0 {
                    format!(
                        "SELECT id, permalink, title, note_type, folder, updated_at
                         FROM notes
                         WHERE project_id = ?1
                           AND updated_at >= datetime('now', '-{hours} hours')
                         ORDER BY updated_at DESC LIMIT ?2"
                    )
                } else {
                    "SELECT id, permalink, title, note_type, folder, updated_at
                     FROM notes WHERE project_id = ?1
                     ORDER BY updated_at DESC LIMIT ?2"
                        .to_owned()
                };
                let mut stmt = conn.prepare(&sql)?;
                let notes = stmt
                    .query_map(rusqlite::params![&project_id, limit], row_to_compact)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(notes)
            })
            .await
    }

    /// List compact note summaries in a folder with optional depth control.
    ///
    /// `depth`: 1 = exact folder only; 0 = all descendants.
    pub async fn list_compact(
        &self,
        project_id: &str,
        folder: &str,
        depth: i64,
    ) -> Result<Vec<NoteCompact>> {
        let project_id = project_id.to_owned();
        let folder = folder.to_owned();
        self.db
            .call(move |conn| {
                let sql = if depth == 1 {
                    "SELECT id, permalink, title, note_type, folder, updated_at
                     FROM notes WHERE project_id = ?1 AND folder = ?2
                     ORDER BY folder, title"
                        .to_owned()
                } else {
                    // depth=0 or depth>1: return all descendants
                    "SELECT id, permalink, title, note_type, folder, updated_at
                     FROM notes WHERE project_id = ?1
                       AND (folder = ?2 OR folder LIKE ?2 || '/%')
                     ORDER BY folder, title"
                        .to_owned()
                };
                let mut stmt = conn.prepare(&sql)?;
                let notes = stmt
                    .query_map(rusqlite::params![&project_id, &folder], row_to_compact)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(notes)
            })
            .await
    }

    /// Find tasks whose `memory_refs` JSON array contains `permalink`.
    ///
    /// Returns minimal task info: `(id, short_id, title, status)`.
    pub async fn task_refs(&self, permalink: &str) -> Result<Vec<serde_json::Value>> {
        let pattern = format!("%\"{permalink}\"%");
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, short_id, title, status FROM tasks
                     WHERE memory_refs LIKE ?1
                     ORDER BY priority, created_at",
                )?;
                let rows = stmt
                    .query_map([&pattern], |r| {
                        Ok(serde_json::json!({
                            "id":       r.get::<_, String>(0)?,
                            "short_id": r.get::<_, String>(1)?,
                            "title":    r.get::<_, String>(2)?,
                            "status":   r.get::<_, String>(3)?,
                        }))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Aggregate health report for a project's knowledge base.
    ///
    /// Stale threshold: notes not updated in more than 30 days.
    pub async fn health(&self, project_id: &str) -> Result<HealthReport> {
        let project_id = project_id.to_owned();
        self.db
            .call(move |conn| {
                let total_notes: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM notes WHERE project_id = ?1",
                    [&project_id],
                    |r| r.get(0),
                )?;

                let broken_link_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM note_links l
                     JOIN notes src ON src.id = l.source_id AND src.project_id = ?1
                     WHERE l.target_id IS NULL",
                    [&project_id],
                    |r| r.get(0),
                )?;

                let orphan_note_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM notes n
                     WHERE n.project_id = ?1
                       AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
                       AND NOT EXISTS (
                           SELECT 1 FROM note_links l WHERE l.target_id = n.id
                       )",
                    [&project_id],
                    |r| r.get(0),
                )?;

                let mut stmt = conn.prepare(
                    "SELECT folder, COUNT(*) FROM notes
                     WHERE project_id = ?1
                       AND updated_at < datetime('now', '-30 days')
                     GROUP BY folder ORDER BY folder",
                )?;
                let stale_notes_by_folder = stmt
                    .query_map([&project_id], |r| {
                        Ok(StaleFolder {
                            folder: r.get(0)?,
                            count: r.get(1)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;

                Ok(HealthReport {
                    total_notes,
                    broken_link_count,
                    orphan_note_count,
                    stale_notes_by_folder,
                })
            })
            .await
    }
}

// ── Wikilink helpers ──────────────────────────────────────────────────────────

/// Extract `(target_raw, display_text)` pairs from `[[...]]` wikilinks in content.
///
/// Handles `[[Target]]` and `[[Target|Display Text]]`.
/// Empty targets and malformed links are silently skipped.
fn extract_wikilinks(content: &str) -> Vec<(String, Option<String>)> {
    let mut results = Vec::new();
    let mut rest = content;
    while let Some(open) = rest.find("[[") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find("]]") else { break };
        let inner = rest[..close].trim();
        rest = &rest[close + 2..];
        if inner.is_empty() {
            continue;
        }
        if let Some(pipe) = inner.find('|') {
            let target = inner[..pipe].trim();
            let display = inner[pipe + 1..].trim();
            if !target.is_empty() {
                let display_opt = if display.is_empty() { None } else { Some(display.to_string()) };
                results.push((target.to_string(), display_opt));
            }
        } else {
            results.push((inner.to_string(), None));
        }
    }
    results
}

/// Re-index all outbound wikilinks for `source_id` from its current `content`.
///
/// Deletes existing link rows for the note then re-inserts them, resolving
/// each target by title or permalink within the same project.
fn index_links_for_note(
    conn: &rusqlite::Connection,
    source_id: &str,
    project_id: &str,
    content: &str,
) -> Result<()> {
    conn.execute("DELETE FROM note_links WHERE source_id = ?1", [source_id])?;

    let links = extract_wikilinks(content);
    if links.is_empty() {
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO note_links (id, source_id, target_id, target_raw, display_text)
         VALUES (?1, ?2,
                 (SELECT id FROM notes
                  WHERE project_id = ?3 AND (title = ?4 OR permalink = ?4)
                  LIMIT 1),
                 ?4, ?5)",
    )?;

    for (target_raw, display_text) in links {
        let id = uuid::Uuid::now_v7().to_string();
        stmt.execute(rusqlite::params![
            &id,
            source_id,
            project_id,
            &target_raw,
            display_text.as_deref(),
        ])?;
    }
    Ok(())
}

/// After a note is created or its title/permalink changes, resolve any
/// previously-broken links in the project that now match this note.
fn resolve_links_for_note(
    conn: &rusqlite::Connection,
    note_id: &str,
    title: &str,
    permalink: &str,
    project_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE note_links
         SET target_id = ?1
         WHERE target_id IS NULL
           AND (target_raw = ?2 OR target_raw = ?3)
           AND source_id IN (SELECT id FROM notes WHERE project_id = ?4)",
        rusqlite::params![note_id, title, permalink, project_id],
    )?;
    Ok(())
}

// ── SQL constant ─────────────────────────────────────────────────────────────

const NOTE_SELECT_WHERE_ID: &str =
    "SELECT id, project_id, permalink, title, file_path,
            note_type, folder, tags, content,
            created_at, updated_at, last_accessed
     FROM notes WHERE id = ?1";

// ── Row mapper ───────────────────────────────────────────────────────────────

fn row_to_compact(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteCompact> {
    Ok(NoteCompact {
        id: row.get(0)?,
        permalink: row.get(1)?,
        title: row.get(2)?,
        note_type: row.get(3)?,
        folder: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

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

    // ── Wikilink graph tests ──────────────────────────────────────────────────

    #[test]
    fn extract_wikilinks_basic() {
        let links = extract_wikilinks("See [[Rust Database Choice]] for details.");
        assert_eq!(links, vec![("Rust Database Choice".to_string(), None)]);
    }

    #[test]
    fn extract_wikilinks_with_display() {
        let links = extract_wikilinks("See [[Rust DB|the ADR]] for details.");
        assert_eq!(
            links,
            vec![("Rust DB".to_string(), Some("the ADR".to_string()))]
        );
    }

    #[test]
    fn extract_wikilinks_multiple() {
        let links = extract_wikilinks("[[A]] and [[B|Bee]] and [[C]]");
        assert_eq!(links.len(), 3);
        assert_eq!(links[0], ("A".to_string(), None));
        assert_eq!(links[1], ("B".to_string(), Some("Bee".to_string())));
        assert_eq!(links[2], ("C".to_string(), None));
    }

    #[test]
    fn extract_wikilinks_empty_and_none() {
        let links = extract_wikilinks("No links here. [[]] empty.");
        assert!(links.is_empty());
    }

    #[tokio::test]
    async fn wikilink_resolves_on_create() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

        // Create target first.
        let target = repo
            .create(&project.id, tmp.path(), "Auth Strategy", "body", "adr", "[]")
            .await
            .unwrap();

        // Create source with a wikilink to the target by title.
        repo.create(
            &project.id,
            tmp.path(),
            "Overview",
            "See [[Auth Strategy]] for auth details.",
            "research",
            "[]",
        )
        .await
        .unwrap();

        let graph = repo.graph(&project.id).await.unwrap();
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].target_id, target.id);
        assert_eq!(graph.edges[0].raw_text, "Auth Strategy");
    }

    #[tokio::test]
    async fn broken_link_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

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

    #[tokio::test]
    async fn orphan_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

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
        repo.create(&project.id, tmp.path(), "Isolated", "no links", "pattern", "[]")
            .await
            .unwrap();

        let orphans = repo.orphans(&project.id, None).await.unwrap();
        // Target has an inbound link; Source and Isolated do not.
        let orphan_titles: Vec<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
        assert!(!orphan_titles.contains(&target.title.as_str()), "target should not be orphan");
        assert!(orphan_titles.contains(&"Source"), "Source has no inbound links");
        assert!(orphan_titles.contains(&"Isolated"), "Isolated has no inbound links");
    }

    #[tokio::test]
    async fn resolve_previously_broken_links_on_create() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tx.clone(), tmp.path()).await;
        let repo = NoteRepository::new(db, tx);

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
}
