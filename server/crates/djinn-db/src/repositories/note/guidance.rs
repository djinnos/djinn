//! Discovery support for preservation-first file-era guidance reconciliation.
//!
//! The reconciliation runner must classify every potentially stale claim before
//! mutating it. This repository query deliberately combines a case-insensitive
//! title/body scan with every note linked to the known file-era architecture
//! ADR, so a linked record cannot be missed merely because it uses different
//! vocabulary.

use super::*;

/// A complete, de-duplicated candidate set for DB guidance reconciliation.
#[derive(Debug, Clone)]
pub struct FileEraGuidanceDiscovery {
    /// Notes matched by a file-era claim or connected to the architecture ADR.
    pub notes: Vec<Note>,
}

impl NoteRepository {
    /// Discover all guidance requiring a disposition in the DB retirement
    /// manifest.
    ///
    /// `claims` are matched case-insensitively against both title and content.
    /// The known architecture ADR and all resolved inbound/outbound wikilinks
    /// are always included. Results include non-active records intentionally:
    /// a manifest is an audit surface, not default current-guidance retrieval.
    pub async fn discover_file_era_guidance(
        &self,
        project_id: &str,
        architecture_adr_id: &str,
        claims: &[&str],
    ) -> Result<FileEraGuidanceDiscovery> {
        self.db.ensure_initialized().await?;

        let mut predicates = vec!["n.id = $2".to_owned(),
            "EXISTS (SELECT 1 FROM note_links l WHERE (l.source_id = n.id AND l.target_id = $2) OR (l.target_id = n.id AND l.source_id = $2))".to_owned()];
        let mut patterns = Vec::new();
        for claim in claims
            .iter()
            .map(|claim| claim.trim())
            .filter(|claim| !claim.is_empty())
        {
            let placeholder = patterns.len() + 3;
            predicates.push(format!(
                "(n.title ILIKE ${placeholder} OR n.content ILIKE ${placeholder})"
            ));
            patterns.push(format!("%{claim}%"));
        }
        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path, n.storage, n.note_type, n.folder, n.status, n.tags::text AS tags, n.content, n.retrieval_anchor, n.created_at, n.updated_at, n.last_accessed, n.access_count, n.confidence, n.abstract as abstract_, n.overview, n.scope_paths::text AS scope_paths FROM notes n WHERE n.project_id = $1 AND ({}) ORDER BY n.permalink, n.id",
            predicates.join(" OR ")
        );
        let mut query = sqlx::query_as::<_, Note>(&sql)
            .bind(project_id)
            .bind(architecture_adr_id);
        for pattern in patterns {
            query = query.bind(pattern);
        }

        Ok(FileEraGuidanceDiscovery {
            notes: query.fetch_all(self.db.pool()).await?,
        })
    }
}
