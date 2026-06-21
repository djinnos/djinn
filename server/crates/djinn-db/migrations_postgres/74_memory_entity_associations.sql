-- qb9o Wave 3: heterogeneous typed entity association substrate.
--
-- Adds `memory_entity_associations`, a typed-edge table that lets notes and
-- proposals participate in the same association graph WITHOUT duplicating
-- proposal bodies into `notes`. The existing `note_associations` table stays
-- intact for backward compatibility (canonical-pair, undirected, Hebbian
-- co_access substrate); the new table is the substrate for *typed* edges
-- (builds_on / contradicts / supersedes / exemplifies / derived_from) that
-- span heterogeneous endpoints.
--
-- Why a separate table (rather than extending `note_associations`):
--   * `note_associations` is keyed by `(note_a_id < note_b_id)` with a CHECK
--     constraint that enforces canonical ordering AND foreign keys into
--     `notes(id)`. Widening it to proposals would either require relaxing the
--     FKs (losing ON DELETE CASCADE behavior) or duplicating proposal rows
--     as note-shaped rows, which is exactly what we must NOT do.
--   * Typed semantic edges are directional (a `derived_from` edge from a
--     proposal to a source note is NOT the same relationship as the
--     reverse). The new table preserves `(source → target)` direction and
--     includes `kind` in the primary key so the same pair can carry
--     multiple typed relationships (e.g. `(note, proposal, builds_on)` and
--     `(note, proposal, contradicts)` are both legal and distinct rows).
--
-- Co_access is intentionally EXCLUDED from this table's kind CHECK — that
-- edge stays on `note_associations` where the Hebbian multiplicative
-- growth model lives. We only need the *typed* semantic/provenance set
-- (`derived_from`, `builds_on`, `contradicts`, `supersedes`,
-- `exemplifies`) here, matching the F5 substrate's widened kind set
-- enumerated in `70_note_association_kinds.sql`.
--
-- Entity type values are pinned to the two first-class memory entities:
-- `note` and `proposal`. A future epic↔note edge would widen the CHECK
-- rather than introduce a parallel table; we deliberately avoid that
-- follow-on in this task per the scope ("do not duplicate proposal bodies
-- into notes").
--
-- Merge semantics: idempotent UPSERT on the full primary-key tuple, with
-- the greater of (existing.weight, new.weight) preserved — same max-weight
-- merge as `note_associations.upsert_typed_association` (vrn9). `updated_at`
-- is bumped on every merge so freshness probes can sort.

CREATE TABLE IF NOT EXISTS memory_entity_associations (
    source_entity_type VARCHAR(16)  NOT NULL,
    source_id          VARCHAR(36)  NOT NULL,
    target_entity_type VARCHAR(16)  NOT NULL,
    target_id          VARCHAR(36)  NOT NULL,
    kind               VARCHAR(32)  NOT NULL,
    weight             DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    created_at         VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at         VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    PRIMARY KEY (source_entity_type, source_id, target_entity_type, target_id, kind),
    CONSTRAINT chk_mea_source_entity_type
        CHECK (source_entity_type IN ('note', 'proposal')),
    CONSTRAINT chk_mea_target_entity_type
        CHECK (target_entity_type IN ('note', 'proposal')),
    CONSTRAINT chk_mea_kind
        CHECK (kind IN ('derived_from', 'builds_on', 'contradicts', 'supersedes', 'exemplifies')),
    -- A self-edge (source == target) would degenerate to a vertex
    -- annotation rather than an association; reject it so callers can't
    -- accidentally paper over misrouted writes.
    CONSTRAINT chk_mea_not_self_edge
        CHECK (NOT (source_entity_type = target_entity_type AND source_id = target_id))
);

-- Traversal indexes: lookup by either endpoint, filtered by kind, is the
-- dominant access pattern for graph-walk tasks (memory_graph,
-- memory_associations). Both `(source_entity_type, source_id, kind)` and
-- `(target_entity_type, target_id, kind)` need to be cheap.
CREATE INDEX IF NOT EXISTS idx_mea_source
    ON memory_entity_associations(source_entity_type, source_id, kind);
CREATE INDEX IF NOT EXISTS idx_mea_target
    ON memory_entity_associations(target_entity_type, target_id, kind);
-- Edge-weight scan (for top-K typed-edge retrieval during retrieval /
-- spreading activation).
CREATE INDEX IF NOT EXISTS idx_mea_weight
    ON memory_entity_associations(weight);
