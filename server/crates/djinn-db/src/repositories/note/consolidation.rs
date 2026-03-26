use std::collections::BTreeSet;

use djinn_core::events::EventBus;
use djinn_core::models::{
    ConsolidatedNoteProvenance, ConsolidationCandidateEdge, ConsolidationCluster,
    ConsolidationNote, ConsolidationRunMetric, DbNoteGroup, Note, NoteDedupCandidate,
};

use super::{NOTE_SELECT_WHERE_ID, NoteRepository};
use crate::Database;
use crate::error::{DbError as Error, DbResult as Result};

const DEDUP_SCORE_THRESHOLD: f64 = -3.0;
const DEDUP_LIMIT: i64 = 16;

pub struct CreateConsolidationRunMetric<'a> {
    pub project_id: &'a str,
    pub note_type: &'a str,
    pub status: &'a str,
    pub scanned_note_count: i64,
    pub candidate_cluster_count: i64,
    pub consolidated_cluster_count: i64,
    pub consolidated_note_count: i64,
    pub source_note_count: i64,
    pub started_at: &'a str,
    pub completed_at: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

pub struct CreateCanonicalConsolidatedNote<'a> {
    pub project_id: &'a str,
    pub note_type: &'a str,
    pub title: &'a str,
    pub content: &'a str,
    pub tags: &'a str,
    pub abstract_: Option<&'a str>,
    pub overview: Option<&'a str>,
    pub confidence: f64,
    pub source_session_ids: &'a [&'a str],
}

pub struct CreatedCanonicalConsolidatedNote {
    pub note: Note,
    pub provenance: Vec<ConsolidatedNoteProvenance>,
}

pub struct NoteConsolidationRepository {
    db: Database,
}

impl NoteConsolidationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn list_db_note_groups(&self) -> Result<Vec<DbNoteGroup>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, DbNoteGroup>(
            "SELECT project_id, note_type, COUNT(*) as note_count
             FROM notes
             WHERE storage = 'db'
               AND note_type IN ('case', 'pattern', 'pitfall')
             GROUP BY project_id, note_type
             ORDER BY project_id ASC, note_type ASC",
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    pub async fn list_db_notes_in_group(
        &self,
        project_id: &str,
        note_type: &str,
    ) -> Result<Vec<ConsolidationNote>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidationNote>(
            "SELECT id, project_id, permalink, title, note_type, folder, content,
                    abstract as abstract_, overview, confidence
             FROM notes
             WHERE project_id = ?1
               AND note_type = ?2
               AND storage = 'db'
             ORDER BY permalink ASC, id ASC",
        )
        .bind(project_id)
        .bind(note_type)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    pub async fn likely_duplicate_clusters(
        &self,
        project_id: &str,
        note_type: &str,
    ) -> Result<Vec<ConsolidationCluster>> {
        let notes = self.list_db_notes_in_group(project_id, note_type).await?;
        self.clusters_from_notes(project_id, &notes).await
    }

    pub async fn create_canonical_consolidated_note(
        &self,
        params: CreateCanonicalConsolidatedNote<'_>,
    ) -> Result<CreatedCanonicalConsolidatedNote> {
        self.db.ensure_initialized().await?;

        let CreateCanonicalConsolidatedNote {
            project_id,
            note_type,
            title,
            content,
            tags,
            abstract_,
            overview,
            confidence,
            source_session_ids,
        } = params;

        for session_id in source_session_ids {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1 AND project_id = ?2",
            )
            .bind(session_id)
            .bind(project_id)
            .fetch_one(self.db.pool())
            .await?;

            if exists == 0 {
                return Err(Error::InvalidData(format!(
                    "source session not found in project {project_id}: {session_id}"
                )));
            }
        }

        let note_repo = NoteRepository::new(self.db.clone(), EventBus::noop());
        let created = note_repo
            .create_db_note(project_id, title, content, note_type, tags)
            .await?;

        note_repo.set_confidence(&created.id, confidence).await?;

        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind(abstract_)
            .bind(overview)
            .bind(&created.id)
            .execute(self.db.pool())
            .await?;

        let mut provenance = Vec::with_capacity(source_session_ids.len());
        for session_id in source_session_ids {
            provenance.push(self.add_provenance(&created.id, session_id).await?);
        }

        let note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(&created.id)
            .fetch_one(self.db.pool())
            .await?;

        Ok(CreatedCanonicalConsolidatedNote { note, provenance })
    }

    pub async fn resolve_source_session_ids(
        &self,
        project_id: &str,
        source_note_ids: &[String],
    ) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;

        if source_note_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = sql_placeholders(source_note_ids.len(), 2);
        let note_count_query =
            format!("SELECT COUNT(*) FROM notes WHERE project_id = ?1 AND id IN ({placeholders})");
        let mut note_count = sqlx::query_scalar::<_, i64>(&note_count_query).bind(project_id);
        for note_id in source_note_ids {
            note_count = note_count.bind(note_id);
        }
        let note_count = note_count.fetch_one(self.db.pool()).await? as usize;
        let unique_note_count = source_note_ids.iter().collect::<BTreeSet<_>>().len();
        if note_count != unique_note_count {
            return Err(Error::InvalidData(format!(
                "one or more source notes not found in project {project_id}"
            )));
        }

        let session_query = format!(
            "SELECT DISTINCT cnp.session_id
             FROM consolidated_note_provenance cnp
             JOIN notes n ON n.id = cnp.note_id
             WHERE n.project_id = ?1
               AND cnp.note_id IN ({placeholders})
             ORDER BY cnp.session_id ASC"
        );
        let mut session_ids = sqlx::query_scalar::<_, String>(&session_query).bind(project_id);
        for note_id in source_note_ids {
            session_ids = session_ids.bind(note_id);
        }

        session_ids
            .fetch_all(self.db.pool())
            .await
            .map_err(Into::into)
    }

    async fn clusters_from_notes(
        &self,
        project_id: &str,
        notes: &[ConsolidationNote],
    ) -> Result<Vec<ConsolidationCluster>> {
        use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

        if notes.len() < 2 {
            return Ok(Vec::new());
        }

        let notes_by_id: HashMap<String, ConsolidationNote> = notes
            .iter()
            .cloned()
            .map(|note| (note.id.clone(), note))
            .collect();

        let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut edge_scores: BTreeMap<(String, String), f64> = BTreeMap::new();

        for note in notes {
            let query_text = build_query_text(note);
            let candidates = self
                .dedup_candidates_for_group(project_id, &note.folder, &note.note_type, &query_text)
                .await?;

            for candidate in candidates {
                if candidate.id == note.id || !notes_by_id.contains_key(&candidate.id) {
                    continue;
                }

                let (left, right) = canonical_pair(&note.id, &candidate.id);
                adjacency
                    .entry(left.clone())
                    .or_default()
                    .insert(right.clone());
                adjacency
                    .entry(right.clone())
                    .or_default()
                    .insert(left.clone());
                edge_scores
                    .entry((left, right))
                    .and_modify(|existing| *existing = existing.max(candidate.score))
                    .or_insert(candidate.score);
            }
        }

        if edge_scores.is_empty() {
            return Ok(Vec::new());
        }

        let mut visited = HashSet::new();
        let mut components = Vec::new();

        for start in adjacency.keys() {
            if !visited.insert(start.clone()) {
                continue;
            }

            let mut queue = VecDeque::from([start.clone()]);
            let mut component_ids = BTreeSet::from([start.clone()]);

            while let Some(node) = queue.pop_front() {
                if let Some(neighbors) = adjacency.get(&node) {
                    for neighbor in neighbors {
                        if visited.insert(neighbor.clone()) {
                            queue.push_back(neighbor.clone());
                        }
                        component_ids.insert(neighbor.clone());
                    }
                }
            }

            if component_ids.len() < 2 {
                continue;
            }

            let component_id_set: HashSet<_> = component_ids.iter().cloned().collect();
            let notes = sorted_component_notes(&component_ids, &notes_by_id);
            let note_ids = notes.iter().map(|note| note.id.clone()).collect::<Vec<_>>();

            let mut edges = Vec::new();
            for (left, right) in edge_scores.keys() {
                if component_id_set.contains(left) && component_id_set.contains(right) {
                    edges.push(ConsolidationCandidateEdge {
                        left_note_id: left.clone(),
                        right_note_id: right.clone(),
                        score: edge_scores[&(left.clone(), right.clone())],
                    });
                }
            }
            edges.sort_by(|left, right| {
                left.left_note_id
                    .cmp(&right.left_note_id)
                    .then_with(|| left.right_note_id.cmp(&right.right_note_id))
            });

            components.push(ConsolidationCluster {
                note_ids,
                notes,
                edges,
            });
        }

        components.sort_by(|left, right| left.note_ids.cmp(&right.note_ids));
        Ok(components)
    }

    async fn dedup_candidates_for_group(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        query_text: &str,
    ) -> Result<Vec<NoteDedupCandidate>> {
        self.db.ensure_initialized().await?;
        let safe_query = sanitize_fts5_query(query_text);
        let Some(safe_query) = safe_query else {
            return Ok(Vec::new());
        };

        sqlx::query_as::<_, NoteDedupCandidate>(
            "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.abstract as abstract_, n.overview,
                    -bm25(notes_fts, 3.0, 1.0, 2.0) as score
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.project_id = ?2
               AND n.folder = ?3
               AND n.note_type = ?4
               AND n.storage = 'db'
               AND -bm25(notes_fts, 3.0, 1.0, 2.0) > ?5
             ORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)
             LIMIT ?6",
        )
        .bind(&safe_query)
        .bind(project_id)
        .bind(folder)
        .bind(note_type)
        .bind(DEDUP_SCORE_THRESHOLD)
        .bind(DEDUP_LIMIT)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    pub async fn add_provenance(
        &self,
        note_id: &str,
        session_id: &str,
    ) -> Result<ConsolidatedNoteProvenance> {
        self.db.ensure_initialized().await?;

        sqlx::query(
            "INSERT INTO consolidated_note_provenance (note_id, session_id)
             VALUES (?1, ?2)",
        )
        .bind(note_id)
        .bind(session_id)
        .execute(self.db.pool())
        .await?;

        self.get_provenance_entry(note_id, session_id).await
    }

    pub async fn list_provenance(&self, note_id: &str) -> Result<Vec<ConsolidatedNoteProvenance>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidatedNoteProvenance>(
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = ?1
             ORDER BY created_at ASC, session_id ASC",
        )
        .bind(note_id)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    pub async fn create_run_metric(
        &self,
        params: CreateConsolidationRunMetric<'_>,
    ) -> Result<ConsolidationRunMetric> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query(
            "INSERT INTO consolidation_run_metrics (
                id, project_id, note_type, status,
                scanned_note_count, candidate_cluster_count,
                consolidated_cluster_count, consolidated_note_count,
                source_note_count, started_at, completed_at, error_message
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(&id)
        .bind(params.project_id)
        .bind(params.note_type)
        .bind(params.status)
        .bind(params.scanned_note_count)
        .bind(params.candidate_cluster_count)
        .bind(params.consolidated_cluster_count)
        .bind(params.consolidated_note_count)
        .bind(params.source_note_count)
        .bind(params.started_at)
        .bind(params.completed_at)
        .bind(params.error_message)
        .execute(self.db.pool())
        .await?;

        self.get_run_metric(&id).await
    }

    pub async fn list_run_metrics(
        &self,
        project_id: &str,
        note_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConsolidationRunMetric>> {
        self.db.ensure_initialized().await?;
        let note_type = note_type.unwrap_or("");
        let limit = limit as i64;

        sqlx::query_as::<_, ConsolidationRunMetric>(
            "SELECT id, project_id, note_type, status,
                    scanned_note_count, candidate_cluster_count,
                    consolidated_cluster_count, consolidated_note_count,
                    source_note_count, started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE project_id = ?1
               AND (?2 = '' OR note_type = ?2)
             ORDER BY started_at DESC, id DESC
             LIMIT ?3",
        )
        .bind(project_id)
        .bind(note_type)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    async fn get_provenance_entry(
        &self,
        note_id: &str,
        session_id: &str,
    ) -> Result<ConsolidatedNoteProvenance> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidatedNoteProvenance>(
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = ?1 AND session_id = ?2",
        )
        .bind(note_id)
        .bind(session_id)
        .fetch_one(self.db.pool())
        .await
        .map_err(|err| match err {
            sqlx::Error::RowNotFound => Error::InvalidData(format!(
                "consolidated provenance not found for note {note_id} and session {session_id}"
            )),
            other => other.into(),
        })
    }

    async fn get_run_metric(&self, id: &str) -> Result<ConsolidationRunMetric> {
        self.db.ensure_initialized().await?;

        sqlx::query_as::<_, ConsolidationRunMetric>(
            "SELECT id, project_id, note_type, status,
                    scanned_note_count, candidate_cluster_count,
                    consolidated_cluster_count, consolidated_note_count,
                    source_note_count, started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await
        .map_err(|err| match err {
            sqlx::Error::RowNotFound => {
                Error::InvalidData(format!("consolidation run metric not found: {id}"))
            }
            other => other.into(),
        })
    }
}

fn build_query_text(note: &ConsolidationNote) -> String {
    let mut query_text = String::new();
    if let Some(abstract_) = note.abstract_.as_deref() {
        query_text.push_str(abstract_);
        query_text.push(' ');
    }
    if let Some(overview) = note.overview.as_deref() {
        query_text.push_str(overview);
        query_text.push(' ');
    }
    query_text.push_str(&note.title);
    query_text.push(' ');
    query_text.push_str(&note.content);
    query_text
}

fn sorted_component_notes(
    component_ids: &std::collections::BTreeSet<String>,
    notes_by_id: &std::collections::HashMap<String, ConsolidationNote>,
) -> Vec<ConsolidationNote> {
    let mut notes = component_ids
        .iter()
        .filter_map(|id| notes_by_id.get(id).cloned())
        .collect::<Vec<_>>();
    notes.sort_by(|left, right| {
        left.permalink
            .cmp(&right.permalink)
            .then_with(|| left.id.cmp(&right.id))
    });
    notes
}

fn canonical_pair(left: &str, right: &str) -> (String, String) {
    if left <= right {
        (left.to_string(), right.to_string())
    } else {
        (right.to_string(), left.to_string())
    }
}

fn sql_placeholders(count: usize, start_index: usize) -> String {
    (0..count)
        .map(|offset| format!("?{}", start_index + offset))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sanitize_fts5_query(query: &str) -> Option<String> {
    let terms = query
        .split_whitespace()
        .map(|term| {
            term.chars()
                .filter(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-'))
                .collect::<String>()
        })
        .filter(|term| term.len() >= 2)
        .take(8)
        .collect::<Vec<_>>();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}
