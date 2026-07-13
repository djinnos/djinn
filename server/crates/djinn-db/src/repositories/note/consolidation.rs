use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use djinn_core::events::EventBus;
use djinn_memory::{
    ConsolidatedNoteProvenance, ConsolidationCandidateEdge, ConsolidationCluster,
    ConsolidationNote, ConsolidationRunMetric, DbNoteGroup, Note, NoteDedupCandidate,
};

use super::{
    CONFIDENCE_CEILING, CONFIDENCE_FLOOR, NoteRepository, NoteRevisionCreateState,
    NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation, NoteRevisionReason,
    NoteRevisionSubsystem, TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance,
    folder_for_type, note_select_where_id, permalink_for,
};
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
    pub decayed_note_count: i64,
    pub archived_note_count: i64,
    pub superseded_source_note_count: i64,
    pub admission_dropped_note_count: i64,
    pub started_at: &'a str,
    pub completed_at: Option<&'a str>,
    pub error_message: Option<&'a str>,
}

impl<'a> Default for CreateConsolidationRunMetric<'a> {
    fn default() -> Self {
        Self {
            project_id: "",
            note_type: "",
            status: "completed",
            scanned_note_count: 0,
            candidate_cluster_count: 0,
            consolidated_cluster_count: 0,
            consolidated_note_count: 0,
            source_note_count: 0,
            decayed_note_count: 0,
            archived_note_count: 0,
            superseded_source_note_count: 0,
            admission_dropped_note_count: 0,
            started_at: "",
            completed_at: None,
            error_message: None,
        }
    }
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
    /// Immutable explanation for the canonical creation revision.
    pub reason: NoteRevisionReason,
    pub source_session_ids: &'a [&'a str],
    pub scope_paths: &'a str,
    /// Cluster source note ids that the canonical note supersedes.
    ///
    /// When provided, a `kind = 'supersedes'` edge is recorded from the
    /// canonical note to each source id after the canonical is created.
    /// Legacy callers may pass `&[]` (or omit the field via `..Default::default()`)
    /// to preserve the old behavior — no edges recorded, no error.
    pub source_note_ids: &'a [String],
}

pub struct CreatedCanonicalConsolidatedNote {
    pub note: Note,
    pub provenance: Vec<ConsolidatedNoteProvenance>,
    /// Number of `supersedes` edges recorded in this call. Zero when
    /// `source_note_ids` is empty (legacy callers).
    pub superseded_source_note_count: usize,
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
            r#"SELECT id, project_id, permalink, title, note_type, folder, scope_paths::text AS "scope_paths!", content,
                    abstract AS abstract_, overview, confidence
             FROM notes
             WHERE project_id = $1
               AND note_type = $2
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
            r#"SELECT n.id, n.project_id, n.permalink, n.title, n.note_type, n.folder, n.scope_paths::text AS "scope_paths!", n.content,
                    n.abstract AS abstract_, n.overview, n.confidence
             FROM notes n
             JOIN consolidated_note_provenance cnp ON cnp.note_id = n.id
             WHERE n.project_id = $1
               AND n.note_type = $2
               AND n.storage = 'db'
               AND cnp.session_id = $3
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
               AND cnp.session_id = $1
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
            reason,
            source_session_ids,
            scope_paths,
            source_note_ids,
        } = params;

        for session_id in source_session_ids {
            let exists: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "count!: i64" FROM sessions WHERE id = $1 AND project_id = $2"#,
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
        let note_id = uuid::Uuid::now_v7().to_string();
        let created = note_repo
            .mutate_with_revision(NoteRevisionMutation {
                project_id: project_id.to_owned(),
                note_id: Some(note_id),
                event_kind: NoteRevisionEventKind::Created,
                desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                    title: title.to_owned(),
                    permalink: permalink_for(note_type, title),
                    content: content.to_owned(),
                    note_type: note_type.to_owned(),
                    folder: folder_for_type(note_type).to_owned(),
                    status: "active".to_owned(),
                    tags: tags.to_owned(),
                    retrieval_anchor: None,
                    scope_paths: scope_paths.to_owned(),
                    confidence: confidence.clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING),
                }),
                attribution: TrustedNoteRevisionAttribution::system(
                    NoteRevisionSubsystem::Consolidation,
                ),
                provenance: TrustedNoteRevisionProvenance::default(),
                reason,
            })
            .await?;
        let created = created.note.ok_or_else(|| {
            Error::Internal("created canonical mutation returned no note".to_owned())
        })?;

        sqlx::query!(
            "UPDATE notes SET abstract = $1, overview = $2 WHERE id = $3",
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

        // Record a `supersedes` edge from the canonical note to each source
        // note id. Legacy callers pass an empty slice, in which case no edges
        // are recorded and the count is zero.
        let mut superseded_source_note_count = 0usize;
        for source_id in source_note_ids {
            note_repo
                .record_supersedes(&created.id, source_id, 1.0)
                .await?;
            superseded_source_note_count += 1;
        }

        let note = note_select_where_id!(&created.id)
            .fetch_one(self.db.pool())
            .await?;

        Ok(CreatedCanonicalConsolidatedNote {
            note,
            provenance,
            superseded_source_note_count,
        })
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
            format!("SELECT COUNT(*) FROM notes WHERE project_id = $1 AND id IN ({placeholders})");
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
             WHERE n.project_id = $1
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
        use std::collections::HashMap;

        if notes.len() < 2 {
            return Ok(Vec::new());
        }

        let notes_by_id: HashMap<String, ConsolidationNote> = notes
            .iter()
            .cloned()
            .map(|note| (note.id.clone(), note))
            .collect();

        let (adjacency, edge_scores) = self.build_graph(project_id, notes, &notes_by_id).await?;

        if edge_scores.is_empty() {
            return Ok(Vec::new());
        }

        let components = Self::traverse_components(&adjacency, &edge_scores, &notes_by_id);
        Ok(components)
    }

    async fn build_graph(
        &self,
        project_id: &str,
        notes: &[ConsolidationNote],
        notes_by_id: &HashMap<String, ConsolidationNote>,
    ) -> Result<(
        BTreeMap<String, BTreeSet<String>>,
        BTreeMap<(String, String), f64>,
    )> {
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

        Ok((adjacency, edge_scores))
    }

    fn traverse_components(
        adjacency: &BTreeMap<String, BTreeSet<String>>,
        edge_scores: &BTreeMap<(String, String), f64>,
        notes_by_id: &HashMap<String, ConsolidationNote>,
    ) -> Vec<ConsolidationCluster> {
        let mut visited = HashSet::new();
        let mut components = Vec::new();

        for start in adjacency.keys() {
            if !visited.insert(start.clone()) {
                continue;
            }

            let component_ids = Self::collect_component(start, adjacency, &mut visited);

            if component_ids.len() < 2 {
                continue;
            }

            let component = Self::assemble_component(&component_ids, edge_scores, notes_by_id);
            components.push(component);
        }

        components.sort_by(|left, right| left.note_ids.cmp(&right.note_ids));
        components
    }

    fn collect_component(
        start: &str,
        adjacency: &BTreeMap<String, BTreeSet<String>>,
        visited: &mut HashSet<String>,
    ) -> BTreeSet<String> {
        let mut queue = VecDeque::from([start.to_string()]);
        let mut component_ids = BTreeSet::from([start.to_string()]);

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

        component_ids
    }

    fn assemble_component(
        component_ids: &BTreeSet<String>,
        edge_scores: &BTreeMap<(String, String), f64>,
        notes_by_id: &HashMap<String, ConsolidationNote>,
    ) -> ConsolidationCluster {
        let component_id_set: HashSet<_> = component_ids.iter().cloned().collect();
        let notes = sorted_component_notes(component_ids, notes_by_id);
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

        ConsolidationCluster {
            note_ids,
            notes,
            edges,
        }
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

        // ts_rank() scores are strictly positive for real matches, so a 0.0
        // floor keeps every genuine hit while dropping non-matches.
        let score_threshold: f64 = 0.0;
        let _ = DEDUP_SCORE_THRESHOLD;

        // Consolidation runs as a latency-insensitive background sweep that
        // fans this exact `ts_rank()` scan out across every (project,
        // note_type) group for every session with provenance. When dozens of
        // sessions finish together this stampedes the sqlx pool (each scan
        // holds a connection ~1s) and starves dispatch / PR-poller / SSE. Hold
        // a background-search permit for the duration so the fan-out can never
        // consume more than the configured slice of the pool. Best-effort: if
        // the semaphore is somehow unavailable we still run (correctness over
        // throttling).
        let _permit = self.db.background_search_permit().await;

        // Compute `websearch_to_tsquery(...)` and `ts_rank(...)` exactly once in
        // a CTE, then apply the rank threshold + ordering on the materialized
        // column. The previous form evaluated the tsquery + a full `ts_rank`
        // three times per row (SELECT, WHERE > $5, ORDER BY), which dominated
        // the ~1s cost. The `@@` membership test in the CTE stays GIN-index
        // eligible.
        // Runtime query: the complete candidate body is intentionally added to
        // this bounded projection, but the offline SQLx cache predates it.
        sqlx::query_as::<_, NoteDedupCandidate>(
            r#"WITH ranked AS (
                 SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.content,
                        n.abstract AS abstract_, n.overview,
                        ts_rank(n.search_vector, websearch_to_tsquery('english', $1))::float8 AS score
                 FROM notes n
                 WHERE n.search_vector @@ websearch_to_tsquery('english', $1)
                   AND n.project_id = $2
                   AND n.folder = $3
                   AND n.note_type = $4
                   AND n.storage = 'db'
               )
               SELECT id, permalink, title, folder, note_type, content, abstract_, overview,
                      score
               FROM ranked
               WHERE score > $5
               ORDER BY score DESC
               LIMIT $6"#,
        )
        .bind(safe_query)
        .bind(project_id)
        .bind(folder)
        .bind(note_type)
        .bind(score_threshold)
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

        sqlx::query!(
            "INSERT INTO consolidated_note_provenance (note_id, session_id)
             VALUES ($1, $2)",
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
             WHERE note_id = $1
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

        // Runtime (non-macro) query: metric columns added after the initial
        // schema are not yet present in the offline `.sqlx` cache, so a
        // compile-checked `query!` would fail under `SQLX_OFFLINE=true`.
        sqlx::query(
            "INSERT INTO consolidation_run_metrics (
                id, project_id, status, note_type,
                scanned_note_count, candidate_cluster_count,
                consolidated_cluster_count, consolidated_note_count,
                source_note_count, decayed_note_count, archived_note_count,
                superseded_source_note_count, admission_dropped_note_count,
                started_at, completed_at, error_message
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(&id)
        .bind(params.project_id)
        .bind(params.status)
        .bind(params.note_type)
        .bind(params.scanned_note_count)
        .bind(params.candidate_cluster_count)
        .bind(params.consolidated_cluster_count)
        .bind(params.consolidated_note_count)
        .bind(params.source_note_count)
        .bind(params.decayed_note_count)
        .bind(params.archived_note_count)
        .bind(params.superseded_source_note_count)
        .bind(params.admission_dropped_note_count)
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

        // NOTE: dynamic SQL — compile-time check not possible (new column not in
        // offline cache + runtime note_type filter pattern).
        sqlx::query_as::<_, ConsolidationRunMetric>(
            r#"SELECT id, project_id, note_type, status,
                    CAST(scanned_note_count AS BIGINT) AS scanned_note_count,
                    CAST(candidate_cluster_count AS BIGINT) AS candidate_cluster_count,
                    CAST(consolidated_cluster_count AS BIGINT) AS consolidated_cluster_count,
                    CAST(consolidated_note_count AS BIGINT) AS consolidated_note_count,
                    CAST(source_note_count AS BIGINT) AS source_note_count,
                    CAST(decayed_note_count AS BIGINT) AS decayed_note_count,
                    CAST(archived_note_count AS BIGINT) AS archived_note_count,
                    CAST(superseded_source_note_count AS BIGINT) AS superseded_source_note_count,
                    CAST(admission_dropped_note_count AS BIGINT) AS admission_dropped_note_count,
                    started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE project_id = $1
               AND ($2 = '' OR note_type = $3)
             ORDER BY started_at DESC, id DESC
             LIMIT $4"#,
        )
        .bind(project_id)
        .bind(note_type)
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

        sqlx::query_as!(
            ConsolidatedNoteProvenance,
            "SELECT note_id, session_id, created_at
             FROM consolidated_note_provenance
             WHERE note_id = $1 AND session_id = $2",
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

    pub async fn get_run_metric(&self, id: &str) -> Result<ConsolidationRunMetric> {
        self.db.ensure_initialized().await?;

        // NOTE: dynamic SQL — compile-time check not possible (new column not in
        // offline cache).
        sqlx::query_as::<_, ConsolidationRunMetric>(
            r#"SELECT id, project_id, note_type, status,
                    CAST(scanned_note_count AS BIGINT) AS scanned_note_count,
                    CAST(candidate_cluster_count AS BIGINT) AS candidate_cluster_count,
                    CAST(consolidated_cluster_count AS BIGINT) AS consolidated_cluster_count,
                    CAST(consolidated_note_count AS BIGINT) AS consolidated_note_count,
                    CAST(source_note_count AS BIGINT) AS source_note_count,
                    CAST(decayed_note_count AS BIGINT) AS decayed_note_count,
                    CAST(archived_note_count AS BIGINT) AS archived_note_count,
                    CAST(superseded_source_note_count AS BIGINT) AS superseded_source_note_count,
                    CAST(admission_dropped_note_count AS BIGINT) AS admission_dropped_note_count,
                    started_at, completed_at, error_message
             FROM consolidation_run_metrics
             WHERE id = $1"#,
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
    // Postgres positional binds ($N) honoring the caller's start offset.
    // Previously emitted MySQL `?` and ignored start_index, which 500'd
    // with "syntax error at or near \",\"" on Postgres.
    crate::repositories::pg_placeholders(count, start_index)
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
