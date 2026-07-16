use super::*;
use crate::repositories::note::NoteRepository;
use djinn_memory::{NoteAssociation, canonical_pair};

/// A resolved association entry: the "other" note's identity plus the link weight.
#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize)]
pub struct NoteAssociationEntry {
    pub note_permalink: String,
    pub note_title: String,
    pub weight: f64,
    /// Association edge kind as stored on `note_associations.kind` (e.g. `"co_access"`).
    /// Today the only value written is `"co_access"`; future wave-1 graph-typed edges
    /// (builds_on / contradicts / supersedes / exemplifies) will widen the value set
    /// — see Epic 2chl. Exposed here so the MCP `memory_associations` response can
    /// carry it without a follow-up contract change.
    pub kind: String,
    pub co_access_count: i64,
    pub last_co_access: String,
}

/// The typed-edge kinds that can be written via
/// [`NoteRepository::upsert_typed_association`] or the provenance-rich
/// [`NoteRepository::upsert_provenance_association`].
///
/// These are the values the F5 `note_associations.kind` substrate accepts for
/// semantic / provenance edges. The LLM enrichment pass (diei) writes these
/// directly; the consolidation pipeline writes `DerivedFrom` as a provenance
/// edge when a canonical note is synthesized from source notes.
///
/// `co_access` is now a member of this enum (needed by the provenance-rich
/// substrate so callers can reason about all edge kinds uniformly), but
/// implicit Hebbian co-access upserts continue to use
/// [`NoteRepository::upsert_association`], which uses multiplicative weight
/// growth and a distinct event model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum NoteAssociationKind {
    /// A note that builds on the related note (additive dependency).
    BuildsOn,
    /// A note that contradicts the related note (warning, not score boost).
    Contradicts,
    /// A note that supersedes the related note (asymmetric: prefer newer).
    Supersedes,
    /// A note that exemplifies the related note (concrete instance of a pattern).
    Exemplifies,
    /// A note that was derived from the related note (provenance).
    DerivedFrom,
    /// Implicit Hebbian co-access edge. Included so the provenance-rich
    /// substrate can represent co-access edges with full provenance metadata;
    /// existing Hebbian upsert paths continue to use
    /// [`NoteRepository::upsert_association`] unchanged.
    CoAccess,
    /// A manually authored or human-curated edge.
    Authored,
    /// An edge minted from embedding similarity analysis.
    EmbeddingRelated,
}

impl NoteAssociationKind {
    /// Returns the string literal stored in `note_associations.kind`.
    ///
    /// This is the canonical wire format the F5 migration allows; values match
    /// the per-kind multipliers documented in the vrn9 / diei designs.
    pub fn as_str(self) -> &'static str {
        match self {
            NoteAssociationKind::BuildsOn => "builds_on",
            NoteAssociationKind::Contradicts => "contradicts",
            NoteAssociationKind::Supersedes => "supersedes",
            NoteAssociationKind::Exemplifies => "exemplifies",
            NoteAssociationKind::DerivedFrom => "derived_from",
            NoteAssociationKind::CoAccess => "co_access",
            NoteAssociationKind::Authored => "authored",
            NoteAssociationKind::EmbeddingRelated => "embedding_related",
        }
    }

    /// Parse a kind string from the database back into a typed enum variant.
    ///
    /// Returns `None` for unrecognized kind strings so callers can degrade
    /// gracefully if a future migration adds new kinds.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "builds_on" => Some(NoteAssociationKind::BuildsOn),
            "contradicts" => Some(NoteAssociationKind::Contradicts),
            "supersedes" => Some(NoteAssociationKind::Supersedes),
            "exemplifies" => Some(NoteAssociationKind::Exemplifies),
            "derived_from" => Some(NoteAssociationKind::DerivedFrom),
            "co_access" => Some(NoteAssociationKind::CoAccess),
            "authored" => Some(NoteAssociationKind::Authored),
            "embedding_related" => Some(NoteAssociationKind::EmbeddingRelated),
            _ => None,
        }
    }
}

/// Known provenance sources for note↔note associations.
///
/// These correspond to the `source` column on `note_associations` introduced
/// by migration 97. Callers of the provenance-rich upsert API pass one of
/// these (or a custom string via [`NoteAssociationSource::Custom`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum NoteAssociationSource {
    /// Implicit Hebbian co-access, written by session co-access tracking.
    /// This is the default `source` for legacy rows (migration 97 backfill).
    SessionCoAccess,
    /// Edges minted by embedding similarity analysis.
    EmbeddingSimilarity,
    /// Edges minted by the LLM enrichment pass (diei).
    LlmEnrichment,
    /// Edges minted by the consolidation pipeline.
    ConsolidationPipeline,
    /// A custom/unknown source. The inner string is stored verbatim.
    Custom(String),
}

impl NoteAssociationSource {
    /// Returns the string stored in `note_associations.source`.
    pub fn as_str(&self) -> &str {
        match self {
            NoteAssociationSource::SessionCoAccess => "session_co_access",
            NoteAssociationSource::EmbeddingSimilarity => "embedding_similarity",
            NoteAssociationSource::LlmEnrichment => "llm_enrichment",
            NoteAssociationSource::ConsolidationPipeline => "consolidation_pipeline",
            NoteAssociationSource::Custom(s) => s.as_str(),
        }
    }
}

/// Input parameters for a provenance-rich note association upsert.
///
/// Combines the edge kind, provenance source, weight, and optional
/// embedding-provenance metadata into a single struct so callers of
/// [`NoteRepository::upsert_provenance_association`] pass structured data
/// rather than a long positional argument list.
#[derive(Clone, Debug)]
pub struct NoteAssociationProvenanceUpsert {
    /// The semantic kind of the edge (e.g. `EmbeddingRelated`, `CoAccess`).
    pub kind: NoteAssociationKind,
    /// The provenance source that minted this edge.
    pub source: NoteAssociationSource,
    /// Edge confidence/weight in `[0.0, 1.0]`; clamped before insert.
    pub weight: f64,
    /// Optional confidence score (may differ from weight for embedding edges).
    pub confidence: Option<f64>,
    /// Optional algorithm version string.
    pub algorithm_version: Option<String>,
    /// Optional embedding model identifier.
    pub embedding_model: Option<String>,
    /// Optional embedding dimensionality.
    pub embedding_dim: Option<i32>,
}

/// A fully-materialized provenance-rich note association row.
///
/// Returned by [`NoteRepository::get_provenance_association`] and
/// [`NoteRepository::list_provenance_associations_for_note`]. Carries every
/// column from the provenance-ready schema (migration 97) so downstream
/// consumers (embedding refresh, health metrics, pruning) can inspect
/// provenance metadata without a second query.
#[derive(Clone, Debug, serde::Serialize)]
pub struct NoteAssociationProvenanceRow {
    pub note_a_id: String,
    pub note_b_id: String,
    pub kind: NoteAssociationKind,
    pub source: NoteAssociationSource,
    pub weight: f64,
    pub co_access_count: i64,
    pub last_co_access: String,
    pub confidence: Option<f64>,
    pub algorithm_version: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_dim: Option<i32>,
    pub last_refreshed_at: Option<String>,
}

/// Raw column projection from `note_associations` for the provenance-rich
/// read path. Decoded into [`NoteAssociationProvenanceRow`] after fetch.
#[derive(sqlx::FromRow)]
struct NoteAssociationProvenanceRaw {
    note_a_id: String,
    note_b_id: String,
    kind: String,
    source: String,
    weight: f64,
    co_access_count: i64,
    last_co_access: String,
    confidence: Option<f64>,
    algorithm_version: Option<String>,
    embedding_model: Option<String>,
    embedding_dim: Option<i32>,
    last_refreshed_at: Option<String>,
}

impl NoteAssociationProvenanceRaw {
    fn into_typed(self) -> crate::error::DbResult<NoteAssociationProvenanceRow> {
        let kind = NoteAssociationKind::from_str_opt(&self.kind).ok_or_else(|| {
            crate::error::DbError::InvalidData(format!(
                "unknown note_associations.kind: {}",
                self.kind
            ))
        })?;
        Ok(NoteAssociationProvenanceRow {
            note_a_id: self.note_a_id,
            note_b_id: self.note_b_id,
            kind,
            source: match self.source.as_str() {
                "session_co_access" => NoteAssociationSource::SessionCoAccess,
                "embedding_similarity" => NoteAssociationSource::EmbeddingSimilarity,
                "llm_enrichment" => NoteAssociationSource::LlmEnrichment,
                "consolidation_pipeline" => NoteAssociationSource::ConsolidationPipeline,
                other => NoteAssociationSource::Custom(other.to_owned()),
            },
            weight: self.weight,
            co_access_count: self.co_access_count,
            last_co_access: self.last_co_access,
            confidence: self.confidence,
            algorithm_version: self.algorithm_version,
            embedding_model: self.embedding_model,
            embedding_dim: self.embedding_dim,
            last_refreshed_at: self.last_refreshed_at,
        })
    }
}

impl NoteRepository {
    /// Upsert a co-access association between two notes.
    ///
    /// * `note_a_id` and `note_b_id` - The two note IDs that were co-accessed.
    /// * `n_co_accesses` - Number of co-access events to record (typically 1, or higher
    ///   for batch session processing).
    ///
    /// The note IDs are canonicalized internally (min < max) to satisfy the
    /// CHECK constraint.
    ///
    /// Returns the updated (or newly created) association.
    pub async fn upsert_association(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        n_co_accesses: u32,
    ) -> Result<NoteAssociation> {
        self.db.ensure_initialized().await?;

        // Canonical ordering to satisfy CHECK constraint
        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);

        let growth_factor = (0..n_co_accesses).fold(1.0_f64, |acc, _| acc * 1.01);
        let new_co_accesses = i64::from(n_co_accesses);
        // Runtime (non-macro) query for the same offline-cache reason as
        // `upsert_typed_association` below: the wider uniqueness tuple
        // `(note_a_id, note_b_id, kind, source)` introduced by the
        // provenance-ready migration (97_note_association_provenance.sql)
        // isn't reflected in the committed `.sqlx/` cache, so a `query!`
        // would fail under `SQLX_OFFLINE=true`. The co-access substrate
        // writes `kind='co_access'` and `source='session_co_access'` so it
        // occupies its own slot in the wider primary key and never collides
        // with typed edges written by `upsert_typed_association`.
        sqlx::query(
            r#"INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source)
             VALUES ($1, $2, 0.01, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), 'co_access', 'session_co_access')
             ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET
                 weight = LEAST(1.0, note_associations.weight * $4),
                 co_access_count = note_associations.co_access_count + EXCLUDED.co_access_count,
                 last_co_access = EXCLUDED.last_co_access"#,
        )
        .bind(a_id)
        .bind(b_id)
        .bind(new_co_accesses)
        .bind(growth_factor)
        .execute(self.db.pool())
        .await?;

        Ok::<NoteAssociation, crate::error::DbError>(
            sqlx::query_as(
                "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
                 FROM note_associations
                 WHERE note_a_id = $1 AND note_b_id = $2
                   AND kind = 'co_access' AND source = 'session_co_access'",
            )
            .bind(a_id)
            .bind(b_id)
            .fetch_one(self.db.pool())
            .await?,
        )
    }

    /// Upsert a semantic association with a minimum target weight.
    ///
    /// Unlike `upsert_association` (which uses multiplicative growth from 0.01),
    /// this method sets the weight to at least `min_weight`. Used for
    /// LLM-classified semantic relationships (contradiction, supersedes, elaborates).
    ///
    /// The note IDs are canonicalized internally (min < max).
    pub async fn upsert_association_min_weight(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        min_weight: f64,
    ) -> Result<NoteAssociation> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);
        let min_weight = min_weight.clamp(0.0, 1.0);

        // Runtime (non-macro) query for the same offline-cache reason as
        // `upsert_association` above: the wider uniqueness tuple
        // `(note_a_id, note_b_id, kind, source)` introduced by the
        // provenance-ready migration (97_note_association_provenance.sql)
        // isn't reflected in the committed `.sqlx/` cache, so a `query!`
        // would fail under `SQLX_OFFLINE=true`. The co-access substrate
        // writes `kind='co_access'` and `source='session_co_access'` so it
        // occupies its own slot in the wider primary key.
        sqlx::query(
            r#"INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source)
             VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), 'co_access', 'session_co_access')
             ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET
                 weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                 co_access_count = note_associations.co_access_count + 1,
                 last_co_access = EXCLUDED.last_co_access"#,
        )
        .bind(a_id)
        .bind(b_id)
        .bind(min_weight)
        .execute(self.db.pool())
        .await?;

        Ok::<NoteAssociation, crate::error::DbError>(
            sqlx::query_as(
                "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
                 FROM note_associations
                 WHERE note_a_id = $1 AND note_b_id = $2
                   AND kind = 'co_access' AND source = 'session_co_access'",
            )
            .bind(a_id)
            .bind(b_id)
            .fetch_one(self.db.pool())
            .await?,
        )
    }

    /// Record a typed `derived_from` provenance edge between two notes.
    ///
    /// Used when a note is created *from* another — e.g. a `pattern` note
    /// consolidated from a cluster of `case` notes records that it was derived
    /// from each source note. The edge is tagged `kind = 'derived_from'` so it
    /// is distinguishable from the implicit Hebbian `co_access` edges.
    ///
    /// Merge semantics: when an edge between the same `(source, target)` pair is
    /// recorded again, the **greater** of the existing and new `weight` is kept
    /// (max-weight merge) rather than overwritten — a stronger provenance signal
    /// is never weakened by a later, weaker one. The `kind` is promoted to
    /// `derived_from` on conflict so an edge first seen as `co_access` is
    /// upgraded once real provenance is known.
    ///
    /// The note IDs are canonicalized internally (min < max) to satisfy the
    /// `note_a_id < note_b_id` CHECK constraint, so this records an undirected
    /// provenance link between the two notes.
    ///
    /// Implemented with a runtime (non-macro) query: the `kind` column is added
    /// by a migration not yet present in the offline `.sqlx` cache, so a
    /// compile-checked `query!` would fail under `SQLX_OFFLINE=true`.
    pub async fn record_derived_from(
        &self,
        source_note_id: &str,
        target_note_id: &str,
        weight: f64,
    ) -> Result<()> {
        self.upsert_typed_association(
            source_note_id,
            target_note_id,
            NoteAssociationKind::DerivedFrom,
            weight,
        )
        .await
    }

    /// Upsert a typed semantic association between two notes.
    ///
    /// Supports the typed kinds surfaced by the F5 substrate (`note_associations.kind`
    /// `VARCHAR(32)`, widened by Epic 3/vrn9 to accept): `builds_on`,
    /// `contradicts`, `supersedes`, `exemplifies`, and `derived_from`. These
    /// are the values the diei (LLM enrichment pass) writes for implicit edges
    /// detected in note prose; `derived_from` is also reused by the consolidation
    /// pipeline as a provenance edge.
    ///
    /// **Idempotent / max-weight merge**: when an edge between the same `(a, b)`
    /// pair already exists, the `GREATEST(old.weight, new.weight)` is kept and
    /// `kind` is set to the explicitly supplied kind. The stronger weight wins,
    /// and the most-recently-asserted typed kind is recorded so callers can
    /// distinguish a freshly-classified enrichment edge from a stale implicit
    /// `co_access` one. `co_access` is never written by this helper — implicit
    /// Hebbian upserts remain on [`Self::upsert_association`].
    ///
    /// `weight` is clamped to `[0.0, 1.0]`. The note IDs are canonicalized
    /// internally (`min < max`) to satisfy the `note_a_id < note_b_id` CHECK
    /// constraint, so this writes an undirected typed edge between the two notes.
    ///
    /// Implemented with a runtime (non-macro) query: the widened `kind` value
    /// set accepted by this helper was added after the offline `.sqlx` cache
    /// was generated, so a compile-checked `query!` would fail under
    /// `SQLX_OFFLINE=true`. The query string lists the allowed kinds via a
    /// runtime-format helper rather than baking them into a macro.
    pub async fn upsert_typed_association(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        kind: NoteAssociationKind,
        weight: f64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);
        let weight = weight.clamp(0.0, 1.0);
        let kind_str = kind.as_str();
        // Fixed source for typed edges minted by this helper. The provenance-
        // ready migration (97_note_association_provenance.sql) widens the
        // primary key to `(note_a_id, note_b_id, kind, source)` and seeds
        // the `source` column default to `'session_co_access'` for every
        // pre-existing row (including the legacy typed edges this helper
        // wrote before the migration landed). Reusing that same default
        // here means the new typed upsert lands on the same primary-key
        // slot the legacy typed rows occupy, so repeated writes of the
        // same `(a, b, kind)` continue to hit `ON CONFLICT … DO UPDATE`
        // and keep the max-weight merge — a follow-up epic (ixib) will
        // widen this helper's surface so callers can pass provenance-rich
        // sources (e.g. `embedding_similarity`, `consolidation_pipeline`,
        // `llm_enrichment`).
        let source_str = "session_co_access";

        // Upgrade semantics: when this helper writes a typed edge for a
        // canonical pair that already carries an implicit `co_access` row
        // (minted by `upsert_association`), the typed edge is the stronger
        // signal. Remove the implicit co_access row first so the typed edge
        // replaces it in place — `get_association_kind` and the
        // `list_associations_*` readers (which filter by `note_a_id` /
        // `note_b_id` only) keep returning a single row per pair for
        // existing tests and call sites that pre-date the wider substrate.
        sqlx::query(
            "DELETE FROM note_associations
             WHERE note_a_id = $1 AND note_b_id = $2
               AND kind = 'co_access' AND source = 'session_co_access'",
        )
        .bind(a_id)
        .bind(b_id)
        .execute(self.db.pool())
        .await?;

        // Runtime (non-macro) query for the same offline-cache reason as
        // `record_derived_from` and `get_association_kind` below: the typed
        // kind widening (vrn9) accepts values beyond the offline `.sqlx`
        // cache's compile-checked projection. The `kind` column is bound
        // rather than interpolated so this stays safe even if a future
        // `NoteAssociationKind` variant is added without touching this
        // function.
        sqlx::query(
            r#"INSERT INTO note_associations
                 (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source)
               VALUES ($1, $2, $3, 0, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), $4, $5)
               ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET
                   weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                   kind = EXCLUDED.kind,
                   last_co_access = EXCLUDED.last_co_access"#,
        )
        .bind(a_id)
        .bind(b_id)
        .bind(weight)
        .bind(kind_str)
        .bind(source_str)
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// Record a typed `supersedes` edge marking `source_note_id` as superseded
    /// by `canonical_note_id` (yk9t task dm4w). The F5 substrate is undirected
    /// (canonical-pair normalization), so the direction is provisional pending
    /// cik0; retrieval filters either endpoint of a `supersedes` edge so the
    /// canonical/source split is recovered at the result-set layer.
    pub async fn record_supersedes(
        &self,
        canonical_note_id: &str,
        source_note_id: &str,
        weight: f64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        self.record_supersedes_in_transaction(&mut tx, canonical_note_id, source_note_id, weight)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn record_supersedes_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        canonical_note_id: &str,
        source_note_id: &str,
        weight: f64,
    ) -> Result<super::NoteSupersedesAssociation> {
        if self
            .association_failure
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::error::DbError::Internal(
                "forced supersedes association insertion failure".to_owned(),
            ));
        }
        let (a_id, b_id) = canonical_pair(canonical_note_id, source_note_id);
        let weight = weight.clamp(0.0, 1.0);
        sqlx::query("DELETE FROM note_associations WHERE note_a_id = $1 AND note_b_id = $2 AND kind = 'co_access' AND source = 'session_co_access'")
            .bind(a_id).bind(b_id).execute(&mut **tx).await?;
        sqlx::query(r#"INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source) VALUES ($1, $2, $3, 0, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), 'supersedes', 'session_co_access') ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET weight = GREATEST(note_associations.weight, EXCLUDED.weight), last_co_access = EXCLUDED.last_co_access"#)
            .bind(a_id).bind(b_id).bind(weight).execute(&mut **tx).await?;
        Ok(super::NoteSupersedesAssociation {
            note_a_id: a_id.to_owned(),
            note_b_id: b_id.to_owned(),
            kind: NoteAssociationKind::Supersedes,
            weight,
        })
    }

    /// Read back the `(weight, kind)` of the association between two notes, if
    /// any. The IDs are canonicalized internally. Primarily a provenance probe
    /// for callers (and tests) that need to inspect a typed edge.
    ///
    /// Runtime (non-macro) query for the same offline-cache reason as
    /// [`Self::record_derived_from`].
    pub async fn get_association_kind(
        &self,
        note_a_id: &str,
        note_b_id: &str,
    ) -> Result<Option<(f64, String)>> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);
        let row: Option<(f64, String)> = sqlx::query_as(
            "SELECT weight, kind FROM note_associations
             WHERE note_a_id = $1 AND note_b_id = $2",
        )
        .bind(a_id)
        .bind(b_id)
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Batch-fetch the `supersedes` neighbors for a set of note ids.
    ///
    /// For each input `note_id` that participates in a `kind = 'supersedes'`
    /// edge (as either endpoint), returns the set of note ids on the *other*
    /// side of those edges. Because the substrate is undirected, a note's
    /// `supersedes` neighbors are the notes it either supersedes or is
    /// superseded by.
    ///
    /// Used by [`build_context`](super::context) to post-filter a result set:
    /// when two notes are connected by a `supersedes` edge and both appear in
    /// the same context set, the superseded source is dropped.
    ///
    /// Returns a map keyed by each input id that has at least one `supersedes`
    /// neighbor; ids with no such edge are absent from the map.
    pub async fn supersedes_neighbors(
        &self,
        note_ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        self.db.ensure_initialized().await?;

        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders = crate::repositories::pg_placeholders(note_ids.len(), 1);
        // NOTE: dynamic SQL (IN list built at runtime) — compile-time check not possible.
        let sql = format!(
            r#"SELECT note_a_id, note_b_id
               FROM note_associations
               WHERE kind = 'supersedes'
                 AND (note_a_id IN ({placeholders}) OR note_b_id IN ({placeholders}))"#,
        );

        let mut query = sqlx::query_as::<sqlx::Postgres, (String, String)>(&sql);
        for id in note_ids {
            query = query.bind(id);
        }
        for id in note_ids {
            query = query.bind(id);
        }

        let rows: Vec<(String, String)> = query.fetch_all(self.db.pool()).await?;

        let id_set: HashSet<&str> = note_ids.iter().map(String::as_str).collect();
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (a_id, b_id) in rows {
            if id_set.contains(a_id.as_str()) {
                map.entry(a_id.clone()).or_default().push(b_id.clone());
            }
            if id_set.contains(b_id.as_str()) {
                map.entry(b_id.clone()).or_default().push(a_id.clone());
            }
        }
        // Sort for deterministic test output.
        for neighbors in map.values_mut() {
            neighbors.sort();
        }

        Ok(map)
    }

    /// Get all co-access associations for a given note.
    ///
    /// Returns only `kind = 'co_access'` / `source = 'session_co_access'`
    /// rows where the note is either `note_a_id` or `note_b_id`, ordered by
    /// weight descending.  This is the legacy Hebbian retrieval path — it
    /// intentionally excludes typed edges (`derived_from`, `builds_on`,
    /// `embedding_related`, etc.) so callers that reason about co-access
    /// strength see only the implicit co-access signal and never confuse an
    /// embedding-minted or authored edge for a co-access count.
    ///
    /// For a full-association view across all kinds and sources, use
    /// [`Self::list_associations_for_note`] or
    /// [`Self::list_provenance_associations_for_note`].
    pub async fn get_associations_for_note(&self, note_id: &str) -> Result<Vec<NoteAssociation>> {
        self.db.ensure_initialized().await?;

        // Runtime (non-macro) query: the `kind` and `source` filter columns
        // are added by migration 97, which is not in the offline `.sqlx/`
        // cache, so `query_as!` would fail under `SQLX_OFFLINE=true`.
        let associations: Vec<NoteAssociation> = sqlx::query_as(
            "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
             FROM note_associations
             WHERE (note_a_id = $1 OR note_b_id = $2)
               AND kind = 'co_access' AND source = 'session_co_access'
             ORDER BY weight DESC",
        )
        .bind(note_id)
        .bind(note_id)
        .fetch_all(self.db.pool())
        .await?;

        Ok(associations)
    }

    /// List associations for a note, joining the opposite note to return resolved
    /// permalink and title. Covers both directions (note_a_id = id OR note_b_id = id).
    ///
    /// **Returns all association kinds and sources** — co-access, authored,
    /// embedding, derived_from, etc. — filtered only by `weight >= min_weight`.
    /// Each entry carries its `kind` string so callers can distinguish
    /// co-access edges from typed provenance edges.  For co-access-only
    /// retrieval, use [`Self::get_associations_for_note`].
    ///
    /// * `note_id`    – the note whose associations to fetch.
    /// * `min_weight` – include only associations with weight >= this value.
    /// * `limit`      – cap result count (0 = unlimited).
    pub async fn list_associations_for_note(
        &self,
        note_id: &str,
        min_weight: f64,
        limit: i64,
    ) -> Result<Vec<NoteAssociationEntry>> {
        self.db.ensure_initialized().await?;

        let effective_limit: i64 = if limit <= 0 { i64::MAX } else { limit };
        let entries: Vec<NoteAssociationEntry> = sqlx::query_as!(
            NoteAssociationEntry,
            r#"SELECT
                 CASE WHEN na.note_a_id = $1 THEN nb.permalink ELSE na_.permalink END AS "note_permalink!: String",
                 CASE WHEN na.note_a_id = $2 THEN nb.title    ELSE na_.title    END AS "note_title!: String",
                 na.weight,
                 na.kind AS "kind!: String",
                 na.co_access_count,
                 na.last_co_access
             FROM note_associations na
             JOIN notes na_ ON na_.id = na.note_a_id
             JOIN notes nb  ON nb.id  = na.note_b_id
             WHERE (na.note_a_id = $3 OR na.note_b_id = $4)
               AND na.weight >= $5
             ORDER BY na.weight DESC
             LIMIT $6"#,
            note_id,
            note_id,
            note_id,
            note_id,
            min_weight,
            effective_limit
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(entries)
    }

    /// List co-access associations with weight above a threshold.
    ///
    /// Returns only `kind = 'co_access'` / `source = 'session_co_access'`
    /// rows ordered by weight descending.  Typed provenance edges
    /// (embedding, authored, derived_from, etc.) are excluded so callers
    /// that use this for co-access graph scoring see only the implicit
    /// Hebbian signal.
    pub async fn list_associations_above_weight(
        &self,
        threshold: f64,
    ) -> Result<Vec<NoteAssociation>> {
        self.db.ensure_initialized().await?;

        // Runtime (non-macro) query: the `kind` and `source` filter columns
        // are added by migration 97, which is not in the offline `.sqlx/`
        // cache.
        let associations: Vec<NoteAssociation> = sqlx::query_as(
            "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
             FROM note_associations
             WHERE weight >= $1
               AND kind = 'co_access' AND source = 'session_co_access'
             ORDER BY weight DESC",
        )
        .bind(threshold)
        .fetch_all(self.db.pool())
        .await?;

        Ok(associations)
    }

    /// Delete co-access associations with weight below a threshold.
    ///
    /// Only removes rows where `kind = 'co_access'` and
    /// `source = 'session_co_access'`.  Typed provenance edges
    /// (embedding, authored, derived_from, etc.) are never touched by this
    /// helper, even if their weight falls below the threshold.
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_associations_below_weight(&self, threshold: f64) -> Result<u64> {
        self.db.ensure_initialized().await?;

        // Runtime (non-macro) query: the `kind` and `source` filter columns
        // are added by migration 97, which is not in the offline `.sqlx/`
        // cache.
        let result = sqlx::query(
            "DELETE FROM note_associations
             WHERE weight < $1
               AND kind = 'co_access' AND source = 'session_co_access'",
        )
        .bind(threshold)
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Delete stale co-access associations older than a given timestamp.
    ///
    /// Only removes rows where `kind = 'co_access'` and
    /// `source = 'session_co_access'`.  Typed provenance edges are excluded
    /// so fresh embedding/authored rows sharing the same note pair or low
    /// weight are never deleted by this helper.
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_old_associations(
        &self,
        before_timestamp: &str,
        max_weight: f64,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        // Runtime (non-macro) query: the `kind` and `source` filter columns
        // are added by migration 97, which is not in the offline `.sqlx/`
        // cache.
        let result = sqlx::query(
            "DELETE FROM note_associations
             WHERE last_co_access < $1 AND weight <= $2
               AND kind = 'co_access' AND source = 'session_co_access'",
        )
        .bind(before_timestamp)
        .bind(max_weight)
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Prune low-weight, stale co-access associations for a specific project.
    ///
    /// Deletes only `kind = 'co_access'` / `source = 'session_co_access'`
    /// rows where:
    /// - weight < 0.05 (low weight threshold)
    /// - `last_co_access` is older than 90 days
    /// - `note_a_id` belongs to a note in the specified project
    ///
    /// Typed provenance edges (embedding, authored, derived_from, etc.) are
    /// never deleted by this helper — even if they share a note pair or low
    /// weight with a stale co-access row.  This ensures housekeeping does not
    /// accidentally remove fresh embedding or authored rows.
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_associations(&self, project_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        // Runtime (non-macro) query: the `kind` and `source` filter columns
        // are added by migration 97, which is not in the offline `.sqlx/`
        // cache.
        let result = sqlx::query(
            r#"DELETE FROM note_associations
             WHERE weight < 0.05
               AND last_co_access < to_char((now() at time zone 'utc') - interval '90 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               AND note_a_id IN (SELECT id FROM notes WHERE project_id = $1)
               AND kind = 'co_access' AND source = 'session_co_access'"#,
        )
        .bind(project_id)
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    // ── Provenance-rich note association APIs (ao5x) ────────────────────────

    /// Upsert a provenance-rich note association.
    ///
    /// Canonicalizes note IDs (`note_a_id < note_b_id`) and upserts by the
    /// four-column key `(note_a_id, note_b_id, kind, source)`. This means
    /// the same note pair can carry multiple rows distinguished by
    /// (kind, source) — e.g. a `co_access / session_co_access` row alongside
    /// an `embedding_related / embedding_similarity` row.
    ///
    /// **Idempotent**: repeated writes for the same key update in place.
    /// `weight` uses max-merge (the greater of old/new is kept); provenance
    /// metadata columns (`confidence`, `algorithm_version`, `embedding_model`,
    /// `embedding_dim`) are overwritten with the latest values.
    /// `last_refreshed_at` and `last_co_access` are set to the current UTC
    /// timestamp.
    ///
    /// **Non-co-access isolation**: when `kind != CoAccess`, the upsert does
    /// **not** increment `co_access_count` — it is set to 0 on insert and
    /// left unchanged on update. Only `upsert_association` (Hebbian path)
    /// increments `co_access_count`.
    ///
    /// `weight` and `confidence` are clamped to `[0.0, 1.0]`.
    pub async fn upsert_provenance_association(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        upsert: &NoteAssociationProvenanceUpsert,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);
        let weight = upsert.weight.clamp(0.0, 1.0);
        let confidence = upsert.confidence.map(|c| c.clamp(0.0, 1.0));
        let kind_str = upsert.kind.as_str();
        let source_str = upsert.source.as_str();
        let is_co_access = matches!(upsert.kind, NoteAssociationKind::CoAccess);

        // Runtime (non-macro) query: the wider uniqueness tuple
        // (note_a_id, note_b_id, kind, source) and provenance columns are
        // added by migration 97, which is not in the offline .sqlx/ cache.
        if is_co_access {
            // Co-access upsert: increment co_access_count (same semantics as
            // the Hebbian path, but with full provenance metadata).
            sqlx::query(
                r#"INSERT INTO note_associations
                     (note_a_id, note_b_id, weight, co_access_count, last_co_access,
                      kind, source,
                      confidence, algorithm_version, embedding_model, embedding_dim,
                      last_refreshed_at)
                   VALUES ($1, $2, $3, 1,
                           to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                           $4, $5,
                           $6, $7, $8, $9,
                           to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                   ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET
                       weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                       co_access_count = note_associations.co_access_count + 1,
                       last_co_access = EXCLUDED.last_co_access,
                       confidence = EXCLUDED.confidence,
                       algorithm_version = EXCLUDED.algorithm_version,
                       embedding_model = EXCLUDED.embedding_model,
                       embedding_dim = EXCLUDED.embedding_dim,
                       last_refreshed_at = EXCLUDED.last_refreshed_at"#,
            )
            .bind(a_id)
            .bind(b_id)
            .bind(weight)
            .bind(kind_str)
            .bind(source_str)
            .bind(confidence)
            .bind(upsert.algorithm_version.as_deref())
            .bind(upsert.embedding_model.as_deref())
            .bind(upsert.embedding_dim)
            .execute(self.db.pool())
            .await?;
        } else {
            // Non-co-access upsert: do NOT increment co_access_count.
            // Set co_access_count to 0 on insert; leave unchanged on update.
            sqlx::query(
                r#"INSERT INTO note_associations
                     (note_a_id, note_b_id, weight, co_access_count, last_co_access,
                      kind, source,
                      confidence, algorithm_version, embedding_model, embedding_dim,
                      last_refreshed_at)
                   VALUES ($1, $2, $3, 0,
                           to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                           $4, $5,
                           $6, $7, $8, $9,
                           to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                   ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET
                       weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                       last_co_access = EXCLUDED.last_co_access,
                       confidence = EXCLUDED.confidence,
                       algorithm_version = EXCLUDED.algorithm_version,
                       embedding_model = EXCLUDED.embedding_model,
                       embedding_dim = EXCLUDED.embedding_dim,
                       last_refreshed_at = EXCLUDED.last_refreshed_at"#,
            )
            .bind(a_id)
            .bind(b_id)
            .bind(weight)
            .bind(kind_str)
            .bind(source_str)
            .bind(confidence)
            .bind(upsert.algorithm_version.as_deref())
            .bind(upsert.embedding_model.as_deref())
            .bind(upsert.embedding_dim)
            .execute(self.db.pool())
            .await?;
        }

        Ok(())
    }

    /// Read back a single provenance-rich association by its canonical key
    /// `(note_a_id, note_b_id, kind, source)`.
    ///
    /// Returns `None` when no row exists for the given quadruple. Note IDs
    /// are canonicalized internally.
    pub async fn get_provenance_association(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        kind: NoteAssociationKind,
        source: &NoteAssociationSource,
    ) -> Result<Option<NoteAssociationProvenanceRow>> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);
        let kind_str = kind.as_str();
        let source_str = source.as_str();

        let raw: Option<NoteAssociationProvenanceRaw> = sqlx::query_as(
            r#"SELECT note_a_id, note_b_id, kind, source,
                      weight, co_access_count, last_co_access,
                      confidence, algorithm_version, embedding_model,
                      embedding_dim, last_refreshed_at
               FROM note_associations
               WHERE note_a_id = $1 AND note_b_id = $2
                 AND kind = $3 AND source = $4"#,
        )
        .bind(a_id)
        .bind(b_id)
        .bind(kind_str)
        .bind(source_str)
        .fetch_optional(self.db.pool())
        .await?;

        match raw {
            Some(r) => Ok(Some(r.into_typed()?)),
            None => Ok(None),
        }
    }

    /// List provenance-rich associations for a note, optionally filtered by
    /// `kind` and/or `source`.
    ///
    /// Returns every row where the note appears as either `note_a_id` or
    /// `note_b_id`, carrying full provenance metadata. Results are ordered
    /// by `weight DESC`.
    ///
    /// * `note_id`    – the note whose associations to fetch.
    /// * `kind`       – if `Some`, filter to only this edge kind.
    /// * `source`     – if `Some`, filter to only this provenance source.
    /// * `min_weight` – exclude rows with `weight < min_weight`.
    /// * `limit`      – cap result count (`0` = unlimited).
    ///
    /// This is the primary read path for downstream embedding refresh and
    /// health-metric epics that need provenance fields without a second
    /// query.
    pub async fn list_provenance_associations_for_note(
        &self,
        note_id: &str,
        kind: Option<NoteAssociationKind>,
        source: Option<&NoteAssociationSource>,
        min_weight: f64,
        limit: i64,
    ) -> Result<Vec<NoteAssociationProvenanceRow>> {
        self.db.ensure_initialized().await?;

        let effective_limit: i64 = if limit <= 0 { i64::MAX } else { limit };

        // Build the query dynamically based on optional filters.
        // We always have: note_id (x2), min_weight, limit.
        // Optional: kind, source.
        let mut sql = String::from(
            r#"SELECT note_a_id, note_b_id, kind, source,
                      weight, co_access_count, last_co_access,
                      confidence, algorithm_version, embedding_model,
                      embedding_dim, last_refreshed_at
               FROM note_associations
               WHERE (note_a_id = $1 OR note_b_id = $2)
                 AND weight >= $3"#,
        );

        let mut bind_idx: usize = 4; // next $N placeholder

        if kind.is_some() {
            sql.push_str(&format!(" AND kind = ${bind_idx}"));
            bind_idx += 1;
        }
        if source.is_some() {
            sql.push_str(&format!(" AND source = ${bind_idx}"));
            bind_idx += 1;
        }

        sql.push_str(&format!(" ORDER BY weight DESC LIMIT ${bind_idx}"));

        let mut query = sqlx::query_as::<sqlx::Postgres, NoteAssociationProvenanceRaw>(&sql)
            .bind(note_id)
            .bind(note_id)
            .bind(min_weight);

        if let Some(k) = kind {
            query = query.bind(k.as_str());
        }
        if let Some(s) = source {
            query = query.bind(s.as_str());
        }
        query = query.bind(effective_limit);

        let raw_rows: Vec<NoteAssociationProvenanceRaw> = query.fetch_all(self.db.pool()).await?;

        raw_rows.into_iter().map(|r| r.into_typed()).collect()
    }

    /// List all provenance-rich associations for a note pair.
    ///
    /// Returns every row for the canonical `(note_a_id, note_b_id)` pair,
    /// across all `(kind, source)` combinations. Useful for discovering
    /// what provenance sources have edges between two notes.
    pub async fn list_provenance_associations_for_pair(
        &self,
        note_a_id: &str,
        note_b_id: &str,
    ) -> Result<Vec<NoteAssociationProvenanceRow>> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);

        let raw_rows: Vec<NoteAssociationProvenanceRaw> = sqlx::query_as(
            r#"SELECT note_a_id, note_b_id, kind, source,
                      weight, co_access_count, last_co_access,
                      confidence, algorithm_version, embedding_model,
                      embedding_dim, last_refreshed_at
               FROM note_associations
               WHERE note_a_id = $1 AND note_b_id = $2
               ORDER BY weight DESC"#,
        )
        .bind(a_id)
        .bind(b_id)
        .fetch_all(self.db.pool())
        .await?;

        raw_rows.into_iter().map(|r| r.into_typed()).collect()
    }
}
