-- Extraction-skipped events represent an extraction session or concrete task run,
-- not a task record alone. Tighten the existing ledger shape check for databases
-- which already applied migration 109.
ALTER TABLE note_revision_events
    DROP CONSTRAINT chk_note_revision_events_shape;

ALTER TABLE note_revision_events
    ADD CONSTRAINT chk_note_revision_events_shape
    CHECK (
        (event_kind = 'created'
            AND note_id IS NOT NULL
            AND content_before IS NULL AND confidence_before IS NULL
            AND content_after IS NOT NULL AND confidence_after IS NOT NULL)
        OR (event_kind = 'updated'
            AND note_id IS NOT NULL
            AND content_before IS NOT NULL AND confidence_before IS NOT NULL
            AND content_after IS NOT NULL AND confidence_after IS NOT NULL)
        OR (event_kind = 'deleted'
            AND note_id IS NOT NULL
            AND content_before IS NOT NULL AND confidence_before IS NOT NULL
            AND content_after IS NULL AND confidence_after IS NULL)
        OR (event_kind = 'confidence_changed'
            AND note_id IS NOT NULL
            AND content_before IS NULL AND content_after IS NULL
            AND confidence_before IS NOT NULL AND confidence_after IS NOT NULL)
        OR (event_kind = 'extraction_skipped'
            AND note_id IS NULL
            AND content_before IS NULL AND content_after IS NULL
            AND confidence_before IS NULL AND confidence_after IS NULL
            AND (session_id IS NOT NULL OR task_run_id IS NOT NULL))
    );
