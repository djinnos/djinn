-- Migration 119: Additive `rollout_label` + trace-level `outcome` columns on
-- the existing `retrieval_traces` table for epic u9hc / proposal 01mm.
--
-- This is the schema/backfill foundation only. Repository APIs that surface
-- these columns live in sibling task yfww; rollout gating / writers live in
-- sibling epic iaip; the outcomes report lives in sibling epic fv7a.
--
-- Design invariants (see design/u9hc-roadmap):
--   * Extend the existing table — do NOT create a parallel injection table.
--   * `rollout_label` is persisted verbatim as TEXT. The repository may write
--     `enabled`, `off`, `kill_switch`, `cohort:<label>`, and legacy labels
--     without this migration collapsing cohort/legacy values.
--   * `outcome` is a single trace-level vocabulary, distinct from the
--     candidate-level `CandidateOutcome` and `skipped_reason` fields inside the
--     `candidates` JSONB array. Candidate DTO / drop-reason semantics are
--     unchanged.
--   * Historical classification is conservative and deterministic: a historical
--     row is only `injected` when `estimated_injected_tokens > 0` AND the
--     `candidates` JSONB is a well-formed array containing at least one
--     candidate object whose `skipped_reason` is JSON-null. `empty` is only
--     assigned for reliable, well-formed zero-injection evidence. Absent,
--     malformed, non-array, or contradictory evidence becomes
--     `legacy_unknown`.
--   * The historical predicate inspects JSON shape (jsonb_typeof) before
--     expanding array elements, so malformed JSON shapes never cause the
--     migration to fail or optimistically classify a row.
--   * Defaults for future legacy inserts remain truthful: future rows written
--     by existing writers (which do not yet set `rollout_label`/`outcome`)
--     read as `rollout_label = 'legacy'` and `outcome = 'legacy_unknown'`,
--     never `injected`.
--   * No existing index is dropped or renamed; the new indexes support later
--     project-scoped time-window grouping by entry point, rollout label, and
--     trace outcome.
--
-- Trace `outcome` vocabulary (stable, machine-readable):
--   injected, empty, error, legacy_unknown,
--   disabled_off, disabled_kill_switch, disabled_legacy

-- ── rollout_label column ──────────────────────────────────────────────────────
-- Verbatim rollout label. Historical rows that predate rollout labelling read
-- as `'legacy'`; future writes set this explicitly. There is intentionally no
-- CHECK constraint on this column so `cohort:<label>` and any legacy labels can
-- be stored verbatim.
ALTER TABLE retrieval_traces
    ADD COLUMN IF NOT EXISTS rollout_label VARCHAR(64) NOT NULL DEFAULT 'legacy';

-- ── outcome column ────────────────────────────────────────────────────────────
-- Trace-level injection outcome. Added NOT NULL with a conservative
-- `legacy_unknown` default so existing rows and future legacy writes can never
-- be misread as `injected`; the deterministic backfill below refines the value
-- for historical rows whose evidence reliably proves a stronger outcome.
ALTER TABLE retrieval_traces
    ADD COLUMN IF NOT EXISTS outcome VARCHAR(32) NOT NULL DEFAULT 'legacy_unknown'
        CONSTRAINT retrieval_traces_outcome_check
            CHECK (outcome IN (
                'injected',
                'empty',
                'error',
                'legacy_unknown',
                'disabled_off',
                'disabled_kill_switch',
                'disabled_legacy'
            ));

-- ── Deterministic historical classifier ──────────────────────────────────────
-- Every pre-migration row currently reads `outcome = 'legacy_unknown'`. This
-- single UPDATE reclassifies rows whose stored evidence reliably proves a
-- stronger outcome. The classification is intentionally one-pass and
-- conservative: any row that does not satisfy a stricter predicate keeps
-- `legacy_unknown`.
--
-- The JSON shape is inspected (jsonb_typeof) BEFORE jsonb_array_elements is
-- called, so malformed shapes (objects, scalars, SQL NULL) never raise. The
-- CASE WHEN ... ELSE '[]'::jsonb END guard mirrors the pattern already used by
-- the health-rollup SQL so non-array candidates never cause a failure.

UPDATE retrieval_traces
SET outcome = subq.classified_outcome
FROM (
    SELECT
        rt.id,
        CASE
            -- injected: positive tokens AND a well-formed candidates array
            -- containing at least one valid injected-candidate object. Every
            -- array element must have the core TraceCandidate identity and
            -- classification shape; otherwise even a superficially matching
            -- object is malformed historical evidence. jsonb_typeof guards
            -- ensure malformed JSON shapes (object, scalar, SQL NULL)
            -- short-circuit to legacy_unknown instead of raising from
            -- jsonb_array_elements.
            WHEN rt.estimated_injected_tokens > 0
                 AND jsonb_typeof(rt.candidates) = 'array'
                 AND NOT EXISTS (
                     SELECT 1
                     FROM jsonb_array_elements(
                         CASE WHEN jsonb_typeof(rt.candidates) = 'array'
                              THEN rt.candidates ELSE '[]'::jsonb END
                     ) AS cand
                     WHERE jsonb_typeof(cand) <> 'object'
                        OR jsonb_typeof(cand -> 'note_id') IS DISTINCT FROM 'string'
                        OR (
                            (cand ->> 'outcome') IS DISTINCT FROM 'injected'
                            AND (cand ->> 'outcome') IS DISTINCT FROM 'skipped'
                        )
                        OR (
                            (cand ->> 'outcome') = 'injected'
                            AND (cand -> 'skipped_reason') IS DISTINCT FROM 'null'::jsonb
                        )
                        OR (
                            (cand ->> 'outcome') = 'skipped'
                            AND (
                                jsonb_typeof(cand -> 'skipped_reason') IS DISTINCT FROM 'string'
                                OR (cand ->> 'skipped_reason') NOT IN (
                                    'not_top_k',
                                    'min_confidence',
                                    'budget_pruned',
                                    'superseded_pruned',
                                    'dedupe',
                                    'search_error'
                                )
                            )
                        )
                 )
                 AND EXISTS (
                     SELECT 1
                     FROM jsonb_array_elements(
                         CASE WHEN jsonb_typeof(rt.candidates) = 'array'
                              THEN rt.candidates ELSE '[]'::jsonb END
                     ) AS cand
                     WHERE jsonb_typeof(cand) = 'object'
                       AND jsonb_typeof(cand -> 'note_id') = 'string'
                       AND (cand ->> 'outcome') = 'injected'
                       AND (cand -> 'skipped_reason') = 'null'::jsonb
                 )
            THEN 'injected'

            -- empty: reliable, well-formed zero-injection evidence. The row
            -- must prove zero injected candidates AND zero estimated injected
            -- tokens. A well-formed empty array is reliable zero-injection
            -- evidence. For a non-empty array, every element must be a
            -- well-formed skipped-candidate object: note_id is a string,
            -- outcome is 'skipped', and skipped_reason is a known non-null
            -- drop reason. This rejects strings, scalars, partial objects,
            -- injected candidates, and unknown candidate vocabulary rather
            -- than treating insufficient evidence as empty. A non-array value
            -- is also NOT reliable (it may be a truncated/malformed payload)
            -- and falls through to legacy_unknown below.
            WHEN rt.estimated_injected_tokens = 0
                 AND jsonb_typeof(rt.candidates) = 'array'
                 AND NOT EXISTS (
                     SELECT 1
                     FROM jsonb_array_elements(
                         CASE WHEN jsonb_typeof(rt.candidates) = 'array'
                              THEN rt.candidates ELSE '[]'::jsonb END
                     ) AS cand
                     WHERE jsonb_typeof(cand) <> 'object'
                        OR jsonb_typeof(cand -> 'note_id') IS DISTINCT FROM 'string'
                        OR (cand ->> 'outcome') IS DISTINCT FROM 'skipped'
                        OR jsonb_typeof(cand -> 'skipped_reason') IS DISTINCT FROM 'string'
                        OR (cand ->> 'skipped_reason') NOT IN (
                            'not_top_k',
                            'min_confidence',
                            'budget_pruned',
                            'superseded_pruned',
                            'dedupe',
                            'search_error'
                        )
                 )
            THEN 'empty'

            -- Everything else: absent, malformed, non-array, or contradictory
            -- evidence. The column default already covers this, but the ELSE
            -- is explicit so the classifier is self-documenting and resilient
            -- to future default changes.
            ELSE 'legacy_unknown'
        END AS classified_outcome
    FROM retrieval_traces rt
) AS subq
WHERE retrieval_traces.id = subq.id
  AND retrieval_traces.outcome <> subq.classified_outcome;

-- ── Indexes for project/time grouping ─────────────────────────────────────────
-- These support later project-scoped time-window grouping by entry point,
-- rollout label, and trace outcome without removing or replacing any existing
-- index on retrieval_traces.

-- (project_id, entry_point, rollout_label, outcome, created_at desc) — the
-- canonical grouping path for the later outcomes report and per-cohort rollups.
CREATE INDEX IF NOT EXISTS idx_retrieval_traces_project_entry_rollout_outcome_created
    ON retrieval_traces (project_id, entry_point, rollout_label, outcome, created_at DESC);

-- (project_id, rollout_label, created_at desc) — recent traces for a rollout
-- label within a project, used for cohort-scoped listing.
CREATE INDEX IF NOT EXISTS idx_retrieval_traces_project_rollout_created
    ON retrieval_traces (project_id, rollout_label, created_at DESC);

-- (project_id, outcome, created_at desc) — recent traces for a trace-level
-- outcome within a project, used for outcome-scoped listing and the report.
CREATE INDEX IF NOT EXISTS idx_retrieval_traces_project_outcome_created
    ON retrieval_traces (project_id, outcome, created_at DESC);
