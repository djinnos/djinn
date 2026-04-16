use std::collections::BTreeSet;

use djinn_core::events::EventBus;
use djinn_core::models::{
    ConsolidatedNoteProvenance, ConsolidationCandidateEdge, ConsolidationCluster,
    ConsolidationNote, ConsolidationRunMetric, DbNoteGroup, Note, NoteDedupCandidate,
};

use super::{NoteRepository, note_select_where_id};
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
    pub scope_paths: &'a str,
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

    /// Return distinct session IDs that have at least one provenance entry,
    /// ordered by session_id ASC.  Used by the consolidation runner to iterate
    /// over sessions when performing session-scoped duplicate detection.
    pub async fn list_sessions_with_provenance(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;

        sqlx::query_scalar!(
            "SELECT DISTINCT session_id
             FROM consolidated_note_provenance
             ORDER BY session_id ASC"
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    pub async fn list_db_note_groups(&self) -> Result<Vec<DbNoteGroup>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as!(
            DbNoteGroup,
            r#"SELECT project_id, note_type, COUNT(*) AS "note_count!: i64"
             FROM notes
             WHERE storage = 'db'
               AND note_type IN ('case', 'pattern', 'pitfall')
             GROUP BY project_id, note_type
             ORDER BY project_id ASC, note_type ASC"#
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

        sqlx::query_as!(
            ConsolidationNote,
            r#"SELECT id, project_id, permalink, title, note_type, folder, scope_paths, content,
                    `abstract` AS abstract_, overview, confidence
             FROM notes
             WHERE project_id = ?
               AND note_type = ?
               AND storage = 'db'
             ORDER BY permalink ASC, id ASC"#,
            project_id,
            note_type
        )
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

    /// List DB notes in a group that have provenance linking them to a specific
    /// session.  This enables session-scoped consolidation — only notes created
    /// or touched during the given session are returned.
    pub async fn list_db_notes_in_group_for_session(
        &self,
        project_id: &str,
        note_type: &str,
        session_id: &str,
    ) -> Result<Vec<ConsolidationNote>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as!(
            ConsolidationNote,
            r#"SELECT n.id, n.project_id, n.permalink, n.title, n.note_type, n.folder, n.scope_paths, n.content,
                    n.`abstract` AS abstract_, n.overview, n.confidence
             FROM notes n
             JOIN consolidated_note_provenance cnp ON cnp.note_id = n.id
             WHERE n.project_id = ?
               AND n.note_type = ?
               AND n.storage = 'db'
               AND cnp.session_id = ?
             ORDER BY n.permalink ASC, n.id ASC"#,
            project_id,
            note_type,
            session_id
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Return note groups that contain at least one note linked to the given
    /// session via `consolidated_note_provenance`.  Only groups with 2+ notes
    /// in the session are returned (a minimum for duplicate detection).
    pub async fn list_db_note_groups_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<DbNoteGroup>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as!(
            DbNoteGroup,
            r#"SELECT n.project_id, n.note_type, COUNT(*) AS "note_count!: i64"
             FROM notes n
             JOIN consolidated_note_provenance cnp ON cnp.note_id = n.id
             WHERE n.storage = 'db'
               AND n.note_type IN ('case', 'pattern', 'pitfall')
               AND cnp.session_id = ?
             GROUP BY n.project_id, n.note_type
             HAVING COUNT(*) >= 2
             ORDER BY n.project_id ASC, n.note_type ASC"#,
            session_id
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Find likely duplicate clusters scoped to notes from a single session.
    /// Only notes with provenance linking them to `session_id` are considered,
    /// preventing unrelated cross-session notes from being consolidated together.
    pub async fn likely_duplicate_clusters_for_session(
        &self,
        project_id: &str,
        note_type: &str,
        session_id: &str,
    ) -> Result<Vec<ConsolidationCluster>> {
        let notes = self
            .list_db_notes_in_group_for_session(project_id, note_type, session_id)
            .await?;
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
            scope_paths,
        } = params;

        for session_id in source_session_ids {
            let exists: i64 = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM sessions WHERE id = ? AND project_id = ?",
                session_id,
                project_id
            )
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
            .create_db_note_with_scope(project_id, title, content, note_type, tags, scope_paths)
            .await?;

        note_repo.set_confidence(&created.id, confidence).await?;

        sqlx::query!(
            "UPDATE notes SET `abstract` = ?, overview = ? WHERE id = ?",
            abstract_,
            overview,
            created.id
        )
        .execute(self.db.pool())
        .await?;

        let mut provenance = Vec::with_capacity(source_session_ids.len());
        for session_id in source_session_ids {
            provenance.push(self.add_provenance(&created.id, session_id).await?);
        }

        let note = note_select_where_id!(&created.id)
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
        // NOTE: dynamic SQL (IN list built at runtime) — compile-time check not possible
        let note_count_query =
            format!("SELECT COUNT(*) FROM notes WHERE project_id = ? AND id IN ({placeholders})");
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

        // NOTE: dynamic SQL (IN list built at runtime) — compile-time check not possible
        let session_query = format!(
            "SELECT DISTINCT cnp.session_id
             FROM consolidated_note_provenance cnp
             JOIN notes n ON n.id = cnp.note_id
             WHERE n.project_id = ?
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

        // Threshold retuned: MySQL MATCH() scores are positive; use 0.0 floor.
        let mysql_threshold: f64 = 0.0;
        let _ = DEDUP_SCORE_THRESHOLD;

        sqlx::query_as!(
            NoteDedupCandidate,
            r#"SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.`abstract` AS abstract_, n.overview,
                    CAST(MATCH(n.title, n.content, n.tags) AGAINST (? IN NATURAL LANGUAGE MODE) AS DOUBLE) AS "score!: f64"
             FROM notes n
             WHERE MATCH(n.title, n.content, n.tags) AGAINST (? IN NATURAL LANGUAGE MODE)
               AND n.project_id = ?
               AND n.folder = ?
               AND n.note_type = ?
               AND n.storage = 'db'
               AND MATCH(n.title, n.content, n.tags) AGAINST (? IN NATURAL LANGUAGE MODE) > ?
             ORDER BY MATCH(n.title, n.content, n.tags) AGAINST (? IN NATURAL LANGUAGE MODE) DESC
             LIMIT ?"#,
            safe_query,
            safe_query,
            project_id,
            folder,
            note_type,
            safe_query,
            mysql_threshold,
            safe_query,
            DEDUP_LIMIT
        )
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

        sqlx::query!(
            "INSERT INTO consolidated_note_provenance (note_id, session_id)
             VALUES (?, ?)",
            note_id,
            session_id
        )
        .execute(self.db.pool())
        .await?;

        self.get_provenance_entry(note_id, session_id).await
    }

    pub async fn list_provenance(&self, note_id: &str) -> Result<Vec<ConsolidatedNoteProvenance>> {
        self.db.ensure_initialized().await?;

        sqlx::query_as!(
            ConsolidatedNoteProvenance,
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = ?
             ORDER BY created_at ASC, session_id ASC",
            note_id
        )
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

        let scanned_i32 = params.scanned_note_count as i32;
        let candidate_i32 = params.candidate_cluster_count as i32;
        let consolidated_cluster_i32 = params.consolidated_cluster_count as i32;
        let consolidated_note_i32 = params.consolidated_note_count as i32;
        let source_i32 = params.source_note_count as i32;
        sqlx::query!(
            "INSERT INTO consolidation_run_metrics (
                id, project_id, `status`, note_type,
                scanned_note_count, candidate_cluster_count,
                consolidated_cluster_count, consolidated_note_count,
                source_note_count, started_at, completed_at, error_message
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            params.project_id,
            params.status,
            params.note_type,
            scanned_i32,
            candidate_i32,
            consolidated_cluster_i32,
            consolidated_note_i32,
            source_i32,
            params.started_at,
            params.completed_at,
            params.error_message
        )
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

        sqlx::query_as!(
            ConsolidationRunMetric,
            r#"SELECT id, project_id, note_type, `status` AS "status!",
                    CAST(scanned_note_count AS SIGNED) AS "scanned_note_count!: i64",
                    CAST(candidate_cluster_count AS SIGNED) AS "candidate_cluster_count!: i64",
                    CAST(consolidated_cluster_count AS SIGNED) AS "consolidated_cluster_count!: i64",
                    CAST(consolidated_note_count AS SIGNED) AS "consolidated_note_count!: i64",
                    CAST(source_note_count AS SIGNED) AS "source_note_count!: i64",
                    started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE project_id = ?
               AND (? = '' OR note_type = ?)
             ORDER BY started_at DESC, id DESC
             LIMIT ?"#,
            project_id,
            note_type,
            note_type,
            limit
        )
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

        sqlx::query_as!(
            ConsolidatedNoteProvenance,
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = ? AND session_id = ?",
            note_id,
            session_id
        )
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

        sqlx::query_as!(
            ConsolidationRunMetric,
            r#"SELECT id, project_id, note_type, `status` AS "status!",
                    CAST(scanned_note_count AS SIGNED) AS "scanned_note_count!: i64",
                    CAST(candidate_cluster_count AS SIGNED) AS "candidate_cluster_count!: i64",
                    CAST(consolidated_cluster_count AS SIGNED) AS "consolidated_cluster_count!: i64",
                    CAST(consolidated_note_count AS SIGNED) AS "consolidated_note_count!: i64",
                    CAST(source_note_count AS SIGNED) AS "source_note_count!: i64",
                    started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE id = ?"#,
            id
        )
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

fn sql_placeholders(count: usize, _start_index: usize) -> String {
    std::iter::repeat_n("?", count).collect::<Vec<_>>().join(", ")
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
