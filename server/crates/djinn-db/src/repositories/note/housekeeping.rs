use std::collections::HashSet;
use std::path::Path;

use djinn_core::models::Note;

use super::*;
use crate::note_hash::note_content_hash;

fn merge_orphan_tag(tags_json: &str, orphan_tag: &str) -> String {
    let mut tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
    let mut seen = HashSet::new();
    tags.retain(|tag| seen.insert(tag.clone()));
    if !tags.iter().any(|tag| tag == orphan_tag) {
        tags.push(orphan_tag.to_string());
    }
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

#[derive(sqlx::FromRow)]
struct BrokenLinkCandidateRow {
    source_id: String,
    source_title: String,
    source_tags: String,
    source_content: String,
    target_raw: String,
}

impl NoteRepository {
    pub async fn rebuild_missing_content_hashes(&self, project_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT id, content FROM notes WHERE project_id = ?1 AND content_hash IS NULL",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut tx = self.db.pool().begin().await?;
        for (id, content) in &rows {
            let hash = note_content_hash(content);
            sqlx::query("UPDATE notes SET content_hash = ?2 WHERE id = ?1")
                .bind(id)
                .bind(hash)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    pub async fn flag_orphan_notes(
        &self,
        project_id: &str,
        project_path: &Path,
        orphan_tag: &str,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let notes = sqlx::query_as::<_, Note>(
            "SELECT id, project_id, permalink, title, file_path,
                        storage, note_type, folder, tags, content,
                        created_at, updated_at, last_accessed,
                        access_count, confidence, abstract as abstract_, overview,
                        scope_paths
             FROM notes n
             WHERE n.project_id = ?1
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               AND n.last_accessed < datetime('now', '-30 days')
               AND n.access_count = 0
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut flagged = 0_u64;
        for note in notes {
            let merged_tags = merge_orphan_tag(&note.tags, orphan_tag);
            if merged_tags != note.tags {
                self.update(&note.id, &note.title, &note.content, &merged_tags)
                    .await?;
                flagged += 1;
            }
        }

        let _ = project_path;
        Ok(flagged)
    }

    pub async fn repair_broken_wikilinks(
        &self,
        project_id: &str,
        _project_path: &Path,
        min_score: f64,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let broken = sqlx::query_as::<_, BrokenLinkCandidateRow>(
            "SELECT src.id as source_id, src.title as source_title, src.storage as source_storage,
                    src.note_type as source_note_type, src.tags as source_tags, src.content as source_content,
                    l.target_raw as target_raw
             FROM note_links l
             JOIN notes src ON src.id = l.source_id
             WHERE src.project_id = ?1
               AND l.target_id IS NULL
             ORDER BY src.id, l.target_raw",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut repaired = 0_u64;
        for row in broken {
            if let Some(replacement) = self
                .best_broken_link_repair(project_id, &row.source_id, &row.target_raw, min_score)
                .await?
            {
                let current = format!("[[{}]]", row.target_raw);
                let new = format!("[[{}]]", replacement);
                if row.source_content.contains(&current) {
                    let updated = row.source_content.replacen(&current, &new, 1);
                    if updated != row.source_content {
                        self.update(
                            &row.source_id,
                            &row.source_title,
                            &updated,
                            &row.source_tags,
                        )
                        .await?;
                        repaired += 1;
                    }
                }
            }
        }

        Ok(repaired)
    }

    async fn best_broken_link_repair(
        &self,
        project_id: &str,
        source_id: &str,
        target_raw: &str,
        min_score: f64,
    ) -> Result<Option<String>> {
        let exact_title = sqlx::query_scalar::<_, String>(
            "SELECT title FROM notes WHERE project_id = ?1 AND title = ?2 LIMIT 1",
        )
        .bind(project_id)
        .bind(target_raw)
        .fetch_optional(self.db.pool())
        .await?;
        if exact_title.is_some() {
            return Ok(exact_title);
        }

        let results: Vec<_> = self
            .search(project_id, target_raw, None, None, None, 5)
            .await?
            .into_iter()
            .filter(|r| r.id != source_id)
            .collect();
        let Some(best) = results.first() else {
            return Ok(None);
        };
        if best.score < min_score {
            return Ok(None);
        }
        if results
            .get(1)
            .is_some_and(|next| (best.score - next.score).abs() < 5.0)
        {
            return Ok(None);
        }

        Ok(Some(best.title.clone()))
    }
}
