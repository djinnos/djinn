-- Extend proposal_debate_trail with structured metadata + needs-evidence kinds.
--
-- Two changes land here so the substrate can persist Judge `needs_evidence`
-- demands and spike `evidence_findings` as first-class typed rows without
-- crowding the human-readable `body` column:
--
-- 1. Widen the `kind` CHECK constraint to admit the two new kinds
--    (`needs_evidence`, `evidence_findings`) alongside the existing
--    `objection`, `rebuttal`, `verdict`. Existing rows keep their kind; the
--    CHECK is replaced (not appended) to avoid a duplicate-object error on
--    migration replay.
--
-- 2. Add a `body_metadata JSONB` column for structured linkage payload:
--    - `needs_evidence` rows carry the linking metadata (proposal id, Judge
--      task id, spike task id, round, revision, etc.) so the structured
--      NeedsEvidenceClaim is recoverable without re-reading
--      `proposals.needs_evidence_claim`.
--    - `evidence_findings` rows carry the full structured findings
--      (answer, evidence, code_paths_inspected, confidence, residual_risks,
--      recommendation_for_advocate).
--
-- `body_metadata` is nullable; pre-existing rows stay NULL until a future
-- edit rewrites them. This preserves existing objection/rebuttal/verdict
-- behavior with no migration of historic data.
--
-- The partial index on open blocking rows stays intact and continues to
-- drive readiness checks: `needs_evidence` entries are written with
-- `blocking = true` so they participate in that index, while
-- `evidence_findings` are written with `blocking = false`.

ALTER TABLE proposal_debate_trail
    DROP CONSTRAINT proposal_debate_trail_kind_check;

ALTER TABLE proposal_debate_trail
    ADD CONSTRAINT proposal_debate_trail_kind_check
        CHECK (kind IN ('objection', 'rebuttal', 'verdict', 'needs_evidence', 'evidence_findings'));

ALTER TABLE proposal_debate_trail
    ADD COLUMN body_metadata JSONB NULL;

-- Index the structured payload so control-plane code can fetch by metadata
-- keys without scanning every row. btree on the JSONB column is cheap because
-- the column is small and we only need to filter by metadata kind.
CREATE INDEX proposal_debate_trail_body_metadata_kind
    ON proposal_debate_trail ((body_metadata ->> 'kind'))
    WHERE body_metadata IS NOT NULL;