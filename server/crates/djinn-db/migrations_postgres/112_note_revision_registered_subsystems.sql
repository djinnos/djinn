-- System attribution is repository-owned. Keep its persisted identity in the
-- same closed set exposed by `NoteRevisionSubsystem`.
ALTER TABLE note_revision_events
    ADD CONSTRAINT chk_note_revision_events_system_subsystem
    CHECK (
        actor_kind <> 'system'
        OR subsystem IN ('mcp', 'dedup', 'consolidation', 'enrichment', 'extraction')
    );
