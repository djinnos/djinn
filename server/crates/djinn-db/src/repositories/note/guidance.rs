//! Discovery support for preservation-first file-era guidance reconciliation.
//!
//! The reconciliation runner must classify every potentially stale claim before
//! mutating it. This repository query deliberately combines a case-insensitive
//! title/body scan with every note linked to the known file-era architecture
//! ADR, so a linked record cannot be missed merely because it uses different
//! vocabulary.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::note_hash::note_content_hash;

/// A human-reviewed reconciliation decision for one discovered note.
///
/// Discovery intentionally does not infer this decision from a text match. A
/// file-era mention can be current migration guidance, historical architecture,
/// or a record that must be retained unchanged. Requiring a decision keyed by
/// the stable UUID prevents an affected record from silently falling out of the
/// retirement manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEraGuidanceClassification {
    pub uuid: String,
    pub classification: String,
    pub disposition: String,
    pub rationale: String,
    pub superseded_by: Option<String>,
    pub supersedes: Option<String>,
}

/// One contract-complete input record for `db-guidance-manifest.json`.
///
/// Field names deliberately match `djinn-retirement-db-guidance/v1` so callers
/// can serialize this value directly for the manifest generator without
/// translating or dropping its preservation/audit metadata.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileEraGuidanceManifestRecord {
    pub uuid: String,
    pub permalink: String,
    pub status: String,
    pub normalized_sha256: String,
    pub classification: String,
    pub disposition: String,
    pub rationale: String,
    pub superseded_by: Option<String>,
    pub supersedes: Option<String>,
    pub source_repository_path: Option<String>,
}

/// The DB-shaped fixture consumed by the retirement manifest generator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileEraGuidanceManifest {
    pub schema: &'static str,
    pub record_count: usize,
    pub records: Vec<FileEraGuidanceManifestRecord>,
}

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

        let mut predicates = vec![
            "n.id = $2".to_owned(),
            "EXISTS (SELECT 1 FROM note_links l WHERE (l.source_id = n.id AND l.target_id = $2) OR (l.target_id = n.id AND l.source_id = $2))".to_owned(),
        ];
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

    /// Build the complete DB-guidance manifest input from discovery plus the
    /// reviewed reconciliation decisions.
    ///
    /// This is the explicit repository-to-manifest bridge. It rejects duplicate
    /// decisions, decisions for non-discovered UUIDs, and (most importantly)
    /// any discovered note without a disposition, rather than allowing a raw
    /// `Vec<Note>` to reach the generator incomplete.
    pub async fn build_file_era_guidance_manifest(
        &self,
        project_id: &str,
        architecture_adr_id: &str,
        claims: &[&str],
        classifications: &[FileEraGuidanceClassification],
    ) -> Result<FileEraGuidanceManifest> {
        let discovery = self
            .discover_file_era_guidance(project_id, architecture_adr_id, claims)
            .await?;
        FileEraGuidanceManifest::from_discovery(discovery, classifications)
    }
}

impl FileEraGuidanceManifest {
    /// Combine the complete candidate set with reviewed decisions.
    pub fn from_discovery(
        discovery: FileEraGuidanceDiscovery,
        classifications: &[FileEraGuidanceClassification],
    ) -> Result<Self> {
        let discovered_ids = discovery
            .notes
            .iter()
            .map(|note| note.id.as_str())
            .collect::<HashSet<_>>();
        let mut decisions = HashMap::with_capacity(classifications.len());
        for decision in classifications {
            validate_classification(decision)?;
            if !discovered_ids.contains(decision.uuid.as_str()) {
                return Err(Error::InvalidData(format!(
                    "file-era guidance classification references non-discovered note: {}",
                    decision.uuid
                )));
            }
            if decisions.insert(decision.uuid.as_str(), decision).is_some() {
                return Err(Error::InvalidData(format!(
                    "duplicate file-era guidance classification: {}",
                    decision.uuid
                )));
            }
        }

        let mut records = Vec::with_capacity(discovery.notes.len());
        for note in discovery.notes {
            let decision = decisions.get(note.id.as_str()).ok_or_else(|| {
                Error::InvalidData(format!(
                    "discovered file-era guidance has no reconciliation disposition: {}",
                    note.id
                ))
            })?;
            records.push(FileEraGuidanceManifestRecord {
                uuid: note.id,
                permalink: note.permalink,
                status: note.status,
                normalized_sha256: note_content_hash(&note.content),
                classification: decision.classification.clone(),
                disposition: decision.disposition.clone(),
                rationale: decision.rationale.trim().to_owned(),
                superseded_by: decision.superseded_by.clone(),
                supersedes: decision.supersedes.clone(),
                source_repository_path: None,
            });
        }
        records.sort_by(|a, b| a.permalink.cmp(&b.permalink).then(a.uuid.cmp(&b.uuid)));

        Ok(Self {
            schema: "djinn-retirement-db-guidance/v1",
            record_count: records.len(),
            records,
        })
    }
}

fn validate_classification(decision: &FileEraGuidanceClassification) -> Result<()> {
    if decision.uuid.trim().is_empty() {
        return Err(Error::InvalidData(
            "file-era guidance classification is missing uuid".to_owned(),
        ));
    }
    if !matches!(
        decision.classification.as_str(),
        "preserve" | "archive" | "deprecate" | "rewrite"
    ) {
        return Err(Error::InvalidData(format!(
            "file-era guidance classification has invalid classification: {}",
            decision.classification
        )));
    }
    if !matches!(
        decision.disposition.as_str(),
        "equivalent" | "db_supersedes_file" | "approved_discard"
    ) {
        return Err(Error::InvalidData(format!(
            "file-era guidance classification has invalid disposition: {}",
            decision.disposition
        )));
    }
    if decision.rationale.trim().is_empty() {
        return Err(Error::InvalidData(
            "file-era guidance classification is missing rationale".to_owned(),
        ));
    }
    Ok(())
}
