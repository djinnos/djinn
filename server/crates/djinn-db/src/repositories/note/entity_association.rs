//! Heterogeneous typed entity-association substrate (qb9o Wave 3).
//!
//! Lets notes and proposals participate in the same typed association
//! graph without duplicating proposal bodies into `notes`. The substrate is
//! the `memory_entity_associations` table (migration 74); it complements
//! the existing note↔note `note_associations` table (which keeps the
//! undirected Hebbian `co_access` semantics) by carrying the *typed*
//! semantic/provenance edges (`derived_from`, `builds_on`, `contradicts`,
//! `supersedes`, `exemplifies`) over heterogeneous endpoints.
//!
//! Public surface here:
//!
//! * [`MemoryEntityType`]            – the `(note | proposal)` discriminator.
//! * [`MemoryEntityKind`]            – typed-edge kind enum, mirroring
//!   [`NoteAssociationKind`] but trimmed to the heterogeneous-substrate set
//!   (no `co_access`).
//! * [`MemoryEntityRef`]             – small `(entity_type, id)` cursor used
//!   by both endpoints of every call.
//! * [`MemoryEntityAssociation`]     – a row in the table (one typed edge).
//! * [`NoteRepository::upsert_typed_entity_association`] – idempotent
//!   max-weight merge for a `(source, target, kind)` triple.
//! * [`NoteRepository::list_typed_entity_associations_for`] – list all
//!   edges incident on a `(entity_type, id)` reference, ordered by weight.
//!
//! Design constraints honored here:
//!
//! * The primary key includes `kind`, so the same `(source, target)` pair
//!   can carry multiple typed relationships (e.g. both `builds_on` and
//!   `contradicts`). We do NOT collapse a typed edge with a co_access one
//!   — `co_access` is not a legal value of [`MemoryEntityKind`] and lives
//!   exclusively on `note_associations`.
//! * Direction matters. `derived_from(A → B)` is distinct from
//!   `derived_from(B → A)` (a proposal is derived from a note, not the
//!   other way around). The substrate is NOT canonicalized to a sorted
//!   pair — source and target endpoints are stored verbatim.
//! * Merge semantics match the F5 `upsert_typed_association` helper: when
//!   the same `(source, target, kind)` triple is written again, the
//!   greater of the existing and new `weight` is kept (max-weight merge),
//!   `updated_at` is bumped, and the row is NOT duplicated.
//! * SQLx offline cache is NOT consulted here — the
//!   `memory_entity_associations` table is added by migration 74, after
//!   the offline `.sqlx/` cache was last regenerated. We use runtime
//!   `sqlx::query` / `sqlx::query_as` with `.bind(...)` for every column,
//!   matching the pattern established by [`super::association`] for the
//!   F5 typed-kind widening.

use super::NoteRepository;
use crate::error::{DbError as Error, DbResult as Result};

/// The first-class memory entities that can sit on either endpoint of a
/// `memory_entity_associations` row.
///
/// The Postgres `chk_mea_*_entity_type` CHECK constraints pin this enum's
/// string representations; [`Self::as_str`] returns the exact literal.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum MemoryEntityType {
    Note,
    Proposal,
}

impl MemoryEntityType {
    /// Returns the literal stored on the row's `*_entity_type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryEntityType::Note => "note",
            MemoryEntityType::Proposal => "proposal",
        }
    }
}

/// Typed semantic/provenance edges that can connect two memory entities.
///
/// The string literals match the F5 substrate's widened `note_associations.kind`
/// value set (enumerated in migration `70_note_association_kinds.sql`).
/// `co_access` is intentionally excluded — implicit Hebbian co-access edges
/// stay on `note_associations` and are upserted via
/// [`super::NoteAssociationKind`] / [`NoteRepository::upsert_association`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryEntityKind {
    /// Provenance: a note / proposal that was derived from the target.
    DerivedFrom,
    /// Additive dependency: source builds on target.
    BuildsOn,
    /// Tension: source contradicts target.
    Contradicts,
    /// Asymmetric supersession: source supersedes target.
    Supersedes,
    /// Concretization: source exemplifies target.
    Exemplifies,
}

impl MemoryEntityKind {
    /// Returns the string literal stored on the row's `kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryEntityKind::DerivedFrom => "derived_from",
            MemoryEntityKind::BuildsOn => "builds_on",
            MemoryEntityKind::Contradicts => "contradicts",
            MemoryEntityKind::Supersedes => "supersedes",
            MemoryEntityKind::Exemplifies => "exemplifies",
        }
    }
}

/// A pointer to one endpoint of an association: a `(entity_type, id)` pair.
///
/// Small, owned, and cheaply cloneable; this is the shape every upsert /
/// list call takes for its source/target argument. Not `Copy` because the
/// `id` is a heap-allocated `String` — callers should `.clone()` only
/// when a value must outlive the caller's borrow of `&self` (the helpers
/// here bind by reference internally).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntityRef {
    pub entity_type: MemoryEntityType,
    pub id: String,
}

impl MemoryEntityRef {
    /// Construct a note-typed ref.
    pub fn note(id: impl Into<String>) -> Self {
        Self {
            entity_type: MemoryEntityType::Note,
            id: id.into(),
        }
    }

    /// Construct a proposal-typed ref.
    pub fn proposal(id: impl Into<String>) -> Self {
        Self {
            entity_type: MemoryEntityType::Proposal,
            id: id.into(),
        }
    }
}

/// A row from `memory_entity_associations`.
///
/// `weight` is the post-merge value (i.e. the greater of every observation
/// that ever landed on this `(source, target, kind)` triple). `created_at`
/// is the original insert timestamp; `updated_at` is bumped on every
/// re-upsert.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryEntityAssociation {
    pub source: MemoryEntityRef,
    pub target: MemoryEntityRef,
    pub kind: MemoryEntityKind,
    pub weight: f64,
    pub created_at: String,
    pub updated_at: String,
}

/// Raw column projection of a `memory_entity_associations` row, used by
/// the runtime (non-macro) `query_as` path. Kept private to the module —
/// public callers see [`MemoryEntityAssociation`] instead, with the typed
/// enums already decoded from their string forms.
#[derive(sqlx::FromRow)]
struct MemoryEntityAssociationRow {
    source_entity_type: String,
    source_id: String,
    target_entity_type: String,
    target_id: String,
    kind: String,
    weight: f64,
    created_at: String,
    updated_at: String,
}

impl MemoryEntityAssociation {
    fn kind_from_str(s: &str) -> Result<MemoryEntityKind> {
        match s {
            "derived_from" => Ok(MemoryEntityKind::DerivedFrom),
            "builds_on" => Ok(MemoryEntityKind::BuildsOn),
            "contradicts" => Ok(MemoryEntityKind::Contradicts),
            "supersedes" => Ok(MemoryEntityKind::Supersedes),
            "exemplifies" => Ok(MemoryEntityKind::Exemplifies),
            other => Err(Error::InvalidData(format!(
                "unknown memory_entity_associations.kind: {other}"
            ))),
        }
    }

    fn type_from_str(s: &str) -> Result<MemoryEntityType> {
        match s {
            "note" => Ok(MemoryEntityType::Note),
            "proposal" => Ok(MemoryEntityType::Proposal),
            other => Err(Error::InvalidData(format!(
                "unknown memory_entity_associations.*_entity_type: {other}"
            ))),
        }
    }
}

impl NoteRepository {
    /// Upsert a typed association between two memory entities.
    ///
    /// * `source` — originating endpoint.
    /// * `target` — destination endpoint.
    /// * `kind`   — typed-edge kind. See [`MemoryEntityKind`].
    /// * `weight` — confidence in `[0.0, 1.0]`; clamped before insert.
    ///
    /// **Idempotent / max-weight merge**: when a row already exists for the
    /// `(source, target, kind)` primary key, the new row's `weight` is
    /// compared against the stored one and the **greater** is preserved.
    /// `updated_at` is bumped on every merge so freshness probes can sort.
    /// A stronger signal can never be weakened by a later, weaker one — the
    /// same merge contract as
    /// [`super::NoteRepository::upsert_typed_association`].
    ///
    /// Direction is preserved: `source → target` is distinct from
    /// `target → source` for the same kind, and both can coexist (a
    /// proposal can `builds_on` a note while the note `derived_from`
    /// the proposal — different rows, different kinds, no ambiguity).
    ///
    /// **Co_access is not a legal [`MemoryEntityKind`] here** — implicit
    /// Hebbian co-access edges stay on `note_associations` and are
    /// upserted via [`Self::upsert_association`] /
    /// [`Self::upsert_typed_association`]. Re-routing co_access onto this
    /// table would discard the multiplicative-growth semantics that
    /// distinguish it from the typed substrate.
    pub async fn upsert_typed_entity_association(
        &self,
        source: MemoryEntityRef,
        target: MemoryEntityRef,
        kind: MemoryEntityKind,
        weight: f64,
    ) -> Result<()> {
        // Reject self-edges up front with a clear error rather than letting
        // them bounce off the `chk_mea_not_self_edge` CHECK constraint —
        // self-edges are almost always a caller bug and the constraint
        // error message is opaque.
        if source.entity_type == target.entity_type && source.id == target.id {
            return Err(Error::InvalidData(format!(
                "memory_entity_associations: self-edge rejected ({} {})",
                source.entity_type.as_str(),
                source.id,
            )));
        }

        // Runtime (non-macro) query: `memory_entity_associations` is added by
        // migration 74, which is not in the offline `.sqlx/` cache, so a
        // compile-checked `query!` would fail under `SQLX_OFFLINE=true`.
        // Mirrors the established pattern in `super::association` for the
        // typed-kind widening.
        let weight = weight.clamp(0.0, 1.0);
        sqlx::query(
            r#"INSERT INTO memory_entity_associations
                 (source_entity_type, source_id,
                  target_entity_type, target_id,
                  kind, weight,
                  created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6,
                       to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                       to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
               ON CONFLICT (source_entity_type, source_id, target_entity_type, target_id, kind)
               DO UPDATE SET
                   weight = GREATEST(memory_entity_associations.weight, EXCLUDED.weight),
                   updated_at = EXCLUDED.updated_at"#,
        )
        .bind(source.entity_type.as_str())
        .bind(&source.id)
        .bind(target.entity_type.as_str())
        .bind(&target.id)
        .bind(kind.as_str())
        .bind(weight)
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// List typed associations incident on `entity`, in either direction.
    ///
    /// Returns every row where `entity` appears as either the source or
    /// the target endpoint, optionally filtered by `min_weight` and
    /// `limit`. Ordered by `weight DESC` so the strongest edges surface
    /// first — the same access pattern the F5 typed-edge helpers use.
    ///
    /// * `entity`     – the reference whose incident edges to fetch.
    /// * `min_weight` – exclude rows with `weight < min_weight`. Pass `0.0`
    ///   to include every typed edge.
    /// * `limit`      – cap result count. `0` means "unbounded" — the
    ///   helper normalizes that to `i64::MAX` so the SQL stays uniform.
    ///
    /// The result set is heterogeneous: a single call returns note↔note,
    /// note↔proposal, and proposal↔proposal edges all together, each row
    /// carrying both endpoints and their entity types. Downstream callers
    /// (graph traversal, retrieval fan-out) can filter / group as needed.
    pub async fn list_typed_entity_associations_for(
        &self,
        entity: MemoryEntityRef,
        min_weight: f64,
        limit: i64,
    ) -> Result<Vec<MemoryEntityAssociation>> {
        self.db.ensure_initialized().await?;

        let effective_limit: i64 = if limit <= 0 { i64::MAX } else { limit };

        // Runtime (non-macro) query for the same offline-cache reason as
        // `upsert_typed_entity_association`.
        //
        // Parenthesization matters: SQL evaluates `AND` before `OR`, so a
        // naive write-up like
        //     `WHERE (a AND b) OR (c AND d) AND weight >= $3`
        // would skip the `weight` filter on the source-side branch and
        // over-return low-weight rows. The fully-parenthesized form below
        // applies the `weight >= $3` predicate to BOTH the source-side and
        // target-side branches.
        let rows: Vec<MemoryEntityAssociationRow> = sqlx::query_as(
            r#"SELECT
                 source_entity_type,
                 source_id,
                 target_entity_type,
                 target_id,
                 kind,
                 weight,
                 created_at,
                 updated_at
               FROM memory_entity_associations
               WHERE (
                     (source_entity_type = $1 AND source_id = $2)
                  OR (target_entity_type = $1 AND target_id = $2)
                 )
                 AND weight >= $3
               ORDER BY weight DESC
               LIMIT $4"#,
        )
        .bind(entity.entity_type.as_str())
        .bind(&entity.id)
        .bind(min_weight)
        .bind(effective_limit)
        .fetch_all(self.db.pool())
        .await?;

        rows.into_iter()
            .map(|row| {
                let kind = MemoryEntityAssociation::kind_from_str(&row.kind)?;
                let source_type = MemoryEntityAssociation::type_from_str(&row.source_entity_type)?;
                let target_type = MemoryEntityAssociation::type_from_str(&row.target_entity_type)?;
                Ok(MemoryEntityAssociation {
                    source: MemoryEntityRef {
                        entity_type: source_type,
                        id: row.source_id,
                    },
                    target: MemoryEntityRef {
                        entity_type: target_type,
                        id: row.target_id,
                    },
                    kind,
                    weight: row.weight,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    }
}
// Database is unused here but kept in scope so future direct-pool helpers
// (e.g. test seeding, debug deletes) have an import anchor without a
// follow-on cleanup commit.
