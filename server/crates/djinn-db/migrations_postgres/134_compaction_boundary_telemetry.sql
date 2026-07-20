-- Migration 134: nullable occupancy telemetry for compaction boundaries.
--
-- This is storage-only and additive. Historic boundary rows intentionally remain
-- NULL, and existing reply-loop writers continue to omit these columns until a
-- later compatibility cutover supplies telemetry.
--
-- Token counts are current-context occupancy snapshots, not lifetime spend.

ALTER TABLE session_compaction_boundaries
    ADD COLUMN trigger VARCHAR(19) NULL,
    ADD COLUMN current_context_tokens_before BIGINT NULL,
    ADD COLUMN current_context_tokens_after BIGINT NULL,
    ADD CONSTRAINT session_compaction_boundaries_trigger_check
        CHECK (
            trigger IS NULL
            OR trigger IN (
                'proactive',
                'context-error',
                'orphan-repair',
                'oversized-transport',
                'manual/test',
                'fallback'
            )
        ),
    ADD CONSTRAINT session_compaction_boundaries_current_context_tokens_before_check
        CHECK (
            current_context_tokens_before IS NULL
            OR current_context_tokens_before >= 0
        ),
    ADD CONSTRAINT session_compaction_boundaries_current_context_tokens_after_check
        CHECK (
            current_context_tokens_after IS NULL
            OR current_context_tokens_after >= 0
        );
