-- Ledger rows are append-only.  Project erasure remains the sole retention
-- exception: the referential-action delete is a nested trigger invocation.
CREATE OR REPLACE FUNCTION reject_note_revision_event_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'note_revision_events is immutable';
    END IF;

    -- A project FK ON DELETE CASCADE invokes this trigger beneath PostgreSQL's
    -- referential-action trigger.  Direct row deletion remains forbidden.
    IF pg_trigger_depth() > 1 THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'note_revision_events may only be erased with its project';
END;
$$;

CREATE TRIGGER note_revision_events_append_only
BEFORE UPDATE OR DELETE ON note_revision_events
FOR EACH ROW EXECUTE FUNCTION reject_note_revision_event_mutation();
