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
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    source_id: &str,
    project_id: &str,
    content: &str,
) -> Result<()> {
    sqlx::query!("DELETE FROM note_links WHERE source_id = ?", source_id)
        .execute(&mut **tx)
        .await?;

    let links = extract_wikilinks(content);
    if links.is_empty() {
        return Ok(());
    }

    for (target_raw, display_text) in links {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT IGNORE INTO note_links (id, source_id, target_id, target_raw, display_text)
             VALUES (?, ?,
                     (SELECT id FROM notes
                      WHERE project_id = ? AND (title = ? OR permalink = ?)
                      LIMIT 1),
                     ?, ?)",
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
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    note_id: &str,
    title: &str,
    permalink: &str,
    project_id: &str,
) -> Result<()> {
    sqlx::query!(
        "UPDATE note_links
         SET target_id = ?
         WHERE target_id IS NULL
           AND (target_raw = ? OR target_raw = ?)
           AND source_id IN (SELECT id FROM notes WHERE project_id = ?)",
        note_id,
        title,
        permalink,
        project_id
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}
