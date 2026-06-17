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
/// [`NoteRepository::upsert_typed_association`].
///
/// These are the values the F5 `note_associations.kind` substrate accepts for
/// semantic / provenance edges. The LLM enrichment pass (diei) writes these
/// directly; the consolidation pipeline writes `DerivedFrom` as a provenance
/// edge when a canonical note is synthesized from source notes.
///
/// `co_access` is intentionally **not** a member of this enum — implicit
/// Hebbian co-access upserts stay on [`NoteRepository::upsert_association`],
/// which uses multiplicative weight growth and a distinct event model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
        }
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
        sqlx::query!(
            r#"INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access)
             VALUES ($1, $2, 0.01, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (note_a_id, note_b_id) DO UPDATE SET
                 weight = LEAST(1.0, note_associations.weight * $4),
                 co_access_count = note_associations.co_access_count + EXCLUDED.co_access_count,
                 last_co_access = EXCLUDED.last_co_access"#,
            a_id,
            b_id,
            new_co_accesses,
            growth_factor
        )
        .execute(self.db.pool())
        .await?;

        Ok::<NoteAssociation, crate::error::DbError>(
            sqlx::query_as!(
                NoteAssociation,
                "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
                 FROM note_associations
                 WHERE note_a_id = $1 AND note_b_id = $2",
                a_id,
                b_id
            )
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

        sqlx::query!(
            r#"INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access)
             VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (note_a_id, note_b_id) DO UPDATE SET
                 weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                 co_access_count = note_associations.co_access_count + 1,
                 last_co_access = EXCLUDED.last_co_access"#,
            a_id,
            b_id,
            min_weight
        )
        .execute(self.db.pool())
        .await?;

        Ok::<NoteAssociation, crate::error::DbError>(
            sqlx::query_as!(
                NoteAssociation,
                "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
                 FROM note_associations
                 WHERE note_a_id = $1 AND note_b_id = $2",
                a_id,
                b_id
            )
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

        // Runtime (non-macro) query for the same offline-cache reason as
        // `record_derived_from` and `get_association_kind` below: the typed
        // kind widening (vrn9) accepts values beyond the offline `.sqlx`
        // cache's compile-checked projection. The `kind` column is bound
        // rather than interpolated so this stays safe even if a future
        // `NoteAssociationKind` variant is added without touching this
        // function.
        sqlx::query(
            r#"INSERT INTO note_associations
                 (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind)
               VALUES ($1, $2, $3, 1, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), $4)
               ON CONFLICT (note_a_id, note_b_id) DO UPDATE SET
                   weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                   kind = EXCLUDED.kind,
                   last_co_access = EXCLUDED.last_co_access"#,
        )
        .bind(a_id)
        .bind(b_id)
        .bind(weight)
        .bind(kind_str)
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
        self.upsert_typed_association(
            canonical_note_id,
            source_note_id,
            NoteAssociationKind::Supersedes,
            weight,
        )
        .await
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

    /// Get all associations for a given note.
    ///
    /// Returns associations where the note is either note_a_id or note_b_id,
    /// ordered by weight descending.
    pub async fn get_associations_for_note(&self, note_id: &str) -> Result<Vec<NoteAssociation>> {
        self.db.ensure_initialized().await?;

        let associations: Vec<NoteAssociation> = sqlx::query_as!(
            NoteAssociation,
            "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
             FROM note_associations
             WHERE note_a_id = $1 OR note_b_id = $2
             ORDER BY weight DESC",
            note_id,
            note_id
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(associations)
    }

    /// List associations for a note, joining the opposite note to return resolved
    /// permalink and title. Covers both directions (note_a_id = id OR note_b_id = id).
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

    /// List all associations with weight above a threshold.
    ///
    /// Returns associations ordered by weight descending.
    pub async fn list_associations_above_weight(
        &self,
        threshold: f64,
    ) -> Result<Vec<NoteAssociation>> {
        self.db.ensure_initialized().await?;

        let associations: Vec<NoteAssociation> = sqlx::query_as!(
            NoteAssociation,
            "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
             FROM note_associations
             WHERE weight >= $1
             ORDER BY weight DESC",
            threshold
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(associations)
    }

    /// Delete associations with weight below a threshold.
    ///
    /// Useful for periodic pruning of low-weight associations.
    /// Returns the number of associations deleted.
    pub async fn prune_associations_below_weight(&self, threshold: f64) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query!(
            "DELETE FROM note_associations
             WHERE weight < $1",
            threshold
        )
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Delete associations older than a given timestamp with weight below threshold.
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_old_associations(
        &self,
        before_timestamp: &str,
        max_weight: f64,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query!(
            "DELETE FROM note_associations
             WHERE last_co_access < $1 AND weight <= $2",
            before_timestamp,
            max_weight
        )
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Prune low-weight, stale associations for a specific project.
    ///
    /// Deletes associations where:
    /// - weight < 0.05 (low weight threshold)
    /// - last_co_access is older than 90 days
    /// - note_a_id belongs to a note in the specified project
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_associations(&self, project_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query!(
            r#"DELETE FROM note_associations
             WHERE weight < 0.05
               AND last_co_access < to_char((now() at time zone 'utc') - interval '90 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               AND note_a_id IN (SELECT id FROM notes WHERE project_id = $1)"#,
            project_id
        )
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }
}
