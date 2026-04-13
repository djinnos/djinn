use super::*;
use file_helpers::{infer_note_type, title_from_permalink};

// ── NoteRepository indexing methods ──────────────────────────────────────────

impl NoteRepository {
    /// Reconcile the note index against note files on disk for one project.
    ///
    /// Detects updates, creations, and deletions by checksum comparison against
    /// indexed note fields and emits NoteUpdated/NoteCreated/NoteDeleted events.
    pub async fn reindex_from_disk(
        &self,
        project_id: &str,
        project_path: &Path,
    ) -> Result<ReindexSummary> {
        self.db.ensure_initialized().await?;

        let scanned = scan_project_notes(project_path)?;
        let mut summary = ReindexSummary::default();
        let mut seen_permalinks = HashSet::new();

        let existing = self.list(project_id, None).await?;
        let mut existing_by_permalink: HashMap<String, Note> = existing
            .into_iter()
            .map(|note| (note.permalink.clone(), note))
            .collect();

        for scanned_note in scanned {
            seen_permalinks.insert(scanned_note.permalink.clone());

            match existing_by_permalink.remove(&scanned_note.permalink) {
                Some(current) => {
                    let indexed_checksum = semantic_checksum(
                        &current.permalink,
                        &current.title,
                        &current.note_type,
                        &current.tags,
                        &current.content,
                    );

                    if indexed_checksum == scanned_note.checksum {
                        summary.unchanged += 1;
                        continue;
                    }

                    let updated = self
                        .update_index_entry(UpdateNoteIndexParams {
                            id: &current.id,
                            file_path: &scanned_note.file_path,
                            permalink: &scanned_note.permalink,
                            title: &scanned_note.title,
                            note_type: &scanned_note.note_type,
                            folder: &scanned_note.folder,
                            tags: &scanned_note.tags,
                            content: &scanned_note.content,
                            project_id,
                        })
                        .await?;
                    self.events.send(DjinnEventEnvelope::note_updated(&updated));
                    summary.updated += 1;
                }
                None => {
                    // Warn if a db-only row with the same title+note_type
                    // already exists. This is the dual-row conflict that
                    // the pre-fix MCP write path could produce: a db-only
                    // row with `file_path=""` and a slightly-different
                    // permalink coexisting with the real file on disk.
                    if let Ok(Some((existing_id, existing_permalink))) =
                        sqlx::query_as::<_, (String, String)>(
                            "SELECT id, permalink FROM notes
                             WHERE project_id = ?1
                               AND storage = 'db'
                               AND title = ?2
                               AND note_type = ?3
                             LIMIT 1",
                        )
                        .bind(project_id)
                        .bind(&scanned_note.title)
                        .bind(&scanned_note.note_type)
                        .fetch_optional(self.db.pool())
                        .await
                    {
                        tracing::warn!(
                            target: "djinn_db::note::reindex",
                            db_note_id = %existing_id,
                            db_permalink = %existing_permalink,
                            disk_permalink = %scanned_note.permalink,
                            disk_file_path = %scanned_note.file_path,
                            "dual-row conflict during reindex: db-only note coexists with file on disk for same title+type; next edit of the db-only row will heal it to file storage"
                        );
                    }

                    let created = self.insert_index_entry(project_id, &scanned_note).await?;
                    self.events.send(DjinnEventEnvelope::note_created(&created));
                    summary.created += 1;
                }
            }
        }

        // Any indexed file-backed note that no longer exists on disk is deleted.
        for (_permalink, stale_note) in existing_by_permalink {
            if stale_note.storage != "file" {
                continue;
            }
            if seen_permalinks.contains(&stale_note.permalink) {
                continue;
            }
            self.delete(&stale_note.id).await?;
            summary.deleted += 1;
        }

        if let Err(error) = self.repair_project_embeddings(project_id).await {
            tracing::warn!(project_id, %error, "failed to repair stale or missing note embeddings after reindex");
        }

        Ok(summary)
    }

    pub(super) async fn insert_index_entry(
        &self,
        project_id: &str,
        scanned_note: &ScannedNote,
    ) -> Result<Note> {
        let id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "INSERT INTO notes
                (id, project_id, permalink, title, file_path,
                 storage, note_type, folder, tags, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(&scanned_note.permalink)
        .bind(&scanned_note.title)
        .bind(&scanned_note.file_path)
        .bind("file")
        .bind(&scanned_note.note_type)
        .bind(&scanned_note.folder)
        .bind(&scanned_note.tags)
        .bind(&scanned_note.content)
        .execute(&mut *tx)
        .await?;

        index_links_for_note(&mut tx, &id, project_id, &scanned_note.content).await?;
        resolve_links_for_note(
            &mut tx,
            &id,
            &scanned_note.title,
            &scanned_note.permalink,
            project_id,
        )
        .await?;

        let note = sqlx::query_as::<_, Note>(super::NOTE_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;
        self.sync_note_embedding(
            &note.id,
            &note.title,
            &note.note_type,
            &note.tags,
            &note.content,
        )
        .await;
        Ok(note)
    }

    pub(super) async fn update_index_entry(
        &self,
        params: UpdateNoteIndexParams<'_>,
    ) -> Result<Note> {
        let UpdateNoteIndexParams {
            id,
            file_path,
            permalink,
            title,
            note_type,
            folder,
            tags,
            content,
            project_id,
        } = params;
        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "UPDATE notes SET
                title = ?2,
                file_path = ?3,
                note_type = ?4,
                folder = ?5,
                tags = ?6,
                content = ?7,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(title)
        .bind(file_path)
        .bind(note_type)
        .bind(folder)
        .bind(tags)
        .bind(content)
        .execute(&mut *tx)
        .await?;

        index_links_for_note(&mut tx, id, project_id, content).await?;
        resolve_links_for_note(&mut tx, id, title, permalink, project_id).await?;

        let note = sqlx::query_as::<_, Note>(super::NOTE_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;
        self.sync_note_embedding(
            &note.id,
            &note.title,
            &note.note_type,
            &note.tags,
            &note.content,
        )
        .await;
        Ok(note)
    }
}

pub struct UpdateNoteIndexParams<'a> {
    pub id: &'a str,
    pub file_path: &'a str,
    pub permalink: &'a str,
    pub title: &'a str,
    pub note_type: &'a str,
    pub folder: &'a str,
    pub tags: &'a str,
    pub content: &'a str,
    pub project_id: &'a str,
}

// ── Scanned note type ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(super) struct ScannedNote {
    pub(super) permalink: String,
    pub(super) title: String,
    pub(super) file_path: String,
    pub(super) note_type: String,
    pub(super) folder: String,
    pub(super) tags: String,
    pub(super) content: String,
    pub(super) checksum: String,
}

// ── Disk scanning ─────────────────────────────────────────────────────────────

pub(super) fn scan_project_notes(project_path: &Path) -> Result<Vec<ScannedNote>> {
    let djinn_root = project_path.join(".djinn");

    let mut notes = vec![];
    let mut seen = HashSet::new();

    if djinn_root.exists() {
        scan_note_tree(&djinn_root, &djinn_root, &mut notes, &mut seen)?;
    }

    Ok(notes)
}

fn scan_note_tree(
    root: &Path,
    dir: &Path,
    out: &mut Vec<ScannedNote>,
    seen_paths: &mut HashSet<PathBuf>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            return Err(Error::InvalidData(format!(
                "read_dir {}: {e}",
                dir.display()
            )));
        }
    };

    for entry in entries {
        let entry = entry.map_err(|e| Error::InvalidData(format!("read_dir entry error: {e}")))?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|v| v.to_str())
                && matches!(name, "logs" | "tasks" | "worktrees")
            {
                continue;
            }
            scan_note_tree(root, &path, out, seen_paths)?;
            continue;
        }

        if path.extension().and_then(|v| v.to_str()) != Some("md") {
            continue;
        }
        if !seen_paths.insert(path.clone()) {
            continue;
        }

        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::InvalidData(format!("read note file {}: {e}", path.display())))?;

        if let Some(scanned) = parse_scanned_note(root, &path, &raw) {
            out.push(scanned);
        }
    }

    Ok(())
}

fn parse_scanned_note(root: &Path, file_path: &Path, raw: &str) -> Option<ScannedNote> {
    let rel = file_path.strip_prefix(root).ok()?;
    let mut permalink = rel.to_string_lossy().replace('\\', "/");
    if !permalink.ends_with(".md") {
        return None;
    }
    permalink.truncate(permalink.len().saturating_sub(3));
    if permalink.is_empty() {
        return None;
    }

    // Catalog is generated content; don't index it as a regular note.
    if permalink == "catalog" {
        return None;
    }

    let folder = permalink
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default();

    let (title, note_type, tags, content) = parse_note_file(raw, &permalink);
    let checksum = semantic_checksum(&permalink, &title, &note_type, &tags, &content);

    Some(ScannedNote {
        permalink,
        title,
        file_path: file_path.to_string_lossy().to_string(),
        note_type,
        folder,
        tags,
        content,
        checksum,
    })
}

pub(super) fn parse_note_file(raw: &str, permalink: &str) -> (String, String, String, String) {
    let mut title = title_from_permalink(permalink);
    let mut note_type = infer_note_type(permalink);
    let mut tags = "[]".to_string();
    let mut content = raw.to_string();

    if let Some((frontmatter, body)) = split_frontmatter(raw) {
        content = body;
        for line in frontmatter.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("title:") {
                let value = value.trim();
                if !value.is_empty() {
                    title = value.to_string();
                }
                continue;
            }
            if let Some(value) = line.strip_prefix("type:") {
                let value = value.trim();
                if !value.is_empty() {
                    note_type = value.to_string();
                }
                continue;
            }
            if let Some(value) = line.strip_prefix("tags:") {
                let value = value.trim();
                if value.starts_with('[') && value.ends_with(']') {
                    tags = value.to_string();
                }
            }
        }
    } else if let Some(first_heading) = raw
        .lines()
        .find(|line| line.starts_with("# "))
        .map(|line| line.trim_start_matches("# ").trim())
        && !first_heading.is_empty()
    {
        title = first_heading.to_string();
    }

    (title, note_type, tags, content)
}

fn split_frontmatter(raw: &str) -> Option<(String, String)> {
    if !raw.starts_with("---\n") {
        return None;
    }
    let rest = &raw[4..];
    let end = rest.find("\n---\n")?;
    let frontmatter = rest[..end].to_string();
    let body = rest[end + 5..].to_string();
    Some((frontmatter, body))
}

pub(super) fn semantic_checksum(
    permalink: &str,
    title: &str,
    note_type: &str,
    tags: &str,
    content: &str,
) -> String {
    let payload = serde_json::json!({
        "permalink": permalink,
        "title": title,
        "note_type": note_type,
        "tags": tags,
        "content": content,
    });
    let serialized = serde_json::to_string(&payload).unwrap_or_default();
    let digest = ring::digest::digest(&ring::digest::SHA256, serialized.as_bytes());
    let mut out = String::with_capacity(digest.as_ref().len() * 2);
    for b in digest.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ── Wikilink helpers ──────────────────────────────────────────────────────────

/// Extract `(target_raw, display_text)` pairs from `[[...]]` wikilinks in content.
///
/// Handles `[[Target]]` and `[[Target|Display Text]]`.
/// Empty targets and malformed links are silently skipped.
pub(super) fn extract_wikilinks(content: &str) -> Vec<(String, Option<String>)> {
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
                let display_opt = if display.is_empty() {
                    None
                } else {
                    Some(display.to_string())
                };
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
pub(super) async fn index_links_for_note(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    source_id: &str,
    project_id: &str,
    content: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM note_links WHERE source_id = ?1")
        .bind(source_id)
        .execute(&mut **tx)
        .await?;

    let links = extract_wikilinks(content);
    if links.is_empty() {
        return Ok(());
    }

    for (target_raw, display_text) in links {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT OR IGNORE INTO note_links (id, source_id, target_id, target_raw, display_text)
             VALUES (?1, ?2,
                     (SELECT id FROM notes
                      WHERE project_id = ?3 AND (title = ?4 OR permalink = ?4)
                      LIMIT 1),
                     ?4, ?5)",
        )
        .bind(&id)
        .bind(source_id)
        .bind(project_id)
        .bind(&target_raw)
        .bind(display_text.as_deref())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// After a note is created or its title/permalink changes, resolve any
/// previously-broken links in the project that now match this note.
pub(super) async fn resolve_links_for_note(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    note_id: &str,
    title: &str,
    permalink: &str,
    project_id: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE note_links
         SET target_id = ?1
         WHERE target_id IS NULL
           AND (target_raw = ?2 OR target_raw = ?3)
           AND source_id IN (SELECT id FROM notes WHERE project_id = ?4)",
    )
    .bind(note_id)
    .bind(title)
    .bind(permalink)
    .bind(project_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
