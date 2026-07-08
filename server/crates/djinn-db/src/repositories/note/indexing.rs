use super::*;

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
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_id: &str,
    project_id: &str,
    content: &str,
) -> Result<()> {
    sqlx::query!("DELETE FROM note_links WHERE source_id = $1", source_id)
        .execute(&mut **tx)
        .await?;

    let links = extract_wikilinks(content);
    if links.is_empty() {
        return Ok(());
    }

    for (target_raw, display_text) in links {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO note_links (id, source_id, target_id, target_raw, display_text)
             VALUES ($1, $2,
                     (SELECT id FROM notes
                      WHERE project_id = $3 AND (title = $4 OR permalink = $5)
                      LIMIT 1),
                     $6, $7)
             ON CONFLICT (source_id, target_raw) DO NOTHING",
            id,
            source_id,
            project_id,
            target_raw,
            target_raw,
            target_raw,
            display_text
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// After a note is created or its title/permalink changes, resolve any
/// previously-broken links in the project that now match this note.
pub(super) async fn resolve_links_for_note(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    note_id: &str,
    title: &str,
    permalink: &str,
    project_id: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE note_links
         SET target_id = $1
         WHERE target_id IS NULL
           AND (target_raw = $2 OR target_raw = $3)
           AND source_id IN (SELECT id FROM notes WHERE project_id = $4)",
        note_id,
        title,
        permalink,
        project_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl NoteRepository {
    /// Upsert a resolved explicit wikilink edge between two notes.
    ///
    /// Most callers should let note creation/update re-index wikilinks from
    /// note content. This small repository-layer escape hatch exists for
    /// control-plane end-to-end fixtures that need a deterministic resolved
    /// wikilink without issuing raw SQL outside `djinn-db`.
    pub async fn upsert_wikilink_edge(
        &self,
        source_id: &str,
        target_id: &str,
        target_raw: &str,
        display_text: Option<&str>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO note_links (id, source_id, target_id, target_raw, display_text)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (source_id, target_raw)
             DO UPDATE SET target_id = EXCLUDED.target_id,
                           display_text = EXCLUDED.display_text",
        )
        .bind(id)
        .bind(source_id)
        .bind(target_id)
        .bind(target_raw)
        .bind(display_text)
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// Load the set of explicit wikilink edges (`note_links` with resolved
    /// `target_id`) that connect any pair of notes within the given set.
    ///
    /// Each pair is canonicalized so the lexicographically smaller note id is
    /// first. Used by the memory enrichment pass (diei) to dedup candidate
    /// implicit edges against existing explicit wikilinks before persisting
    /// typed associations.
    ///
    /// Returns an empty set when `note_ids` is empty.
    pub async fn wikilink_pairs_for_notes(
        &self,
        note_ids: &[String],
    ) -> Result<std::collections::HashSet<(String, String)>> {
        use std::collections::HashSet;

        self.db.ensure_initialized().await?;
        if note_ids.is_empty() {
            return Ok(HashSet::new());
        }

        let placeholders = crate::repositories::pg_placeholders(note_ids.len(), 1);
        let placeholders2 =
            crate::repositories::pg_placeholders(note_ids.len(), note_ids.len() + 1);
        let sql = format!(
            "SELECT source_id, target_id FROM note_links \
             WHERE target_id IS NOT NULL \
               AND (source_id IN ({p1}) OR target_id IN ({p2}))",
            p1 = placeholders,
            p2 = placeholders2,
        );

        let mut query = sqlx::query_as::<sqlx::Postgres, (String, String)>(&sql);
        for id in note_ids {
            query = query.bind(id);
        }
        for id in note_ids {
            query = query.bind(id);
        }
        let rows = query.fetch_all(self.db.pool()).await?;

        let id_set: HashSet<&str> = note_ids.iter().map(String::as_str).collect();
        let mut pairs = HashSet::new();
        for (source, target) in rows {
            if id_set.contains(source.as_str()) && id_set.contains(target.as_str()) {
                let pair = if source <= target {
                    (source, target)
                } else {
                    (target, source)
                };
                pairs.insert(pair);
            }
        }
        Ok(pairs)
    }
}
