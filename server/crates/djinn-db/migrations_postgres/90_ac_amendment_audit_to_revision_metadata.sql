-- AC-amendment audit moves from the FEEDBACK pane to the revision History.
--
-- `proposal_ac_amend` used to write the machine-oriented change list as an
-- `author_kind = 'ai'` `proposal_feedback` row ("Acceptance criteria amended
-- \nreason: ...\nrevision: N -> M\namendments: <json>"). That surfaced raw
-- audit JSON as unresolved reviewer feedback with "Address with djinn" /
-- "Dismiss" buttons — wrong, it is a spec-change audit, not review feedback.
--
-- The amendment audit now rides on the bumped spec revision's
-- `event_metadata` (`{"kind":"ac_amendment","reason":...,"amendments":[...]}`).
-- This migration (1) best-effort backfills that metadata onto the matching
-- revision for existing feedback rows, then (2) deletes the feedback rows.

-- (1) Best-effort backfill. Per-row EXCEPTION guard: any row whose body doesn't
--     parse cleanly (unexpected shape, invalid amendments JSON) is skipped, not
--     fatal. Only fills revisions that don't already carry event_metadata.
DO $$
DECLARE
    r        RECORD;
    v_seq    INT;
    v_reason TEXT;
    v_amend  TEXT;
    v_meta   JSONB;
BEGIN
    FOR r IN
        SELECT id, proposal_id, body
          FROM proposal_feedback
         WHERE author_kind = 'ai'
           AND body LIKE 'Acceptance criteria amended%'
    LOOP
        BEGIN
            -- Target revision seq from the "revision: N -> M" line.
            v_seq := NULLIF((regexp_match(r.body, 'revision:\s*\d+\s*->\s*(\d+)'))[1], '')::INT;
            -- Reason: text after "reason: " up to the "\nrevision:" line.
            v_reason := (regexp_match(r.body, 'reason:\s*(.*?)\nrevision:'))[1];
            -- Amendments: the compact JSON array after "amendments: ".
            v_amend := (regexp_match(r.body, 'amendments:\s*(.*)$'))[1];

            IF v_seq IS NULL OR v_amend IS NULL THEN
                CONTINUE;
            END IF;

            v_meta := jsonb_build_object(
                'kind', 'ac_amendment',
                'reason', COALESCE(v_reason, ''),
                'amendments', v_amend::jsonb
            );

            UPDATE proposal_revisions
               SET event_metadata = v_meta
             WHERE proposal_id = r.proposal_id
               AND seq = v_seq
               AND event_kind = 'spec_revision'
               AND event_metadata IS NULL;
        EXCEPTION WHEN others THEN
            -- Unparseable body / invalid JSON — skip this row, keep going.
            CONTINUE;
        END;
    END LOOP;
END $$;

-- (2) Delete the stale audit-as-feedback rows. proposal_feedback has no FK
--     children (parent_id has no FK; resolution is columns on the row itself),
--     so a plain DELETE is safe.
DELETE FROM proposal_feedback
 WHERE author_kind = 'ai'
   AND body LIKE 'Acceptance criteria amended%';
