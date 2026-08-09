-- Attempt-wide fencing is separate from task-delivery leases because branch
-- graduation and reconciliation precede creation of a delivery generation.
CREATE TABLE proposal_build_attempt_leases (
    build_attempt_id VARCHAR(36) NOT NULL PRIMARY KEY
        REFERENCES proposal_build_attempts(id) ON DELETE RESTRICT,
    owner_incarnation_id VARCHAR(128) NOT NULL,
    generation BIGINT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT proposal_build_attempt_leases_owner_nonblank
        CHECK (btrim(owner_incarnation_id) <> ''),
    CONSTRAINT proposal_build_attempt_leases_generation_positive CHECK (generation > 0)
);

-- Retired attempts are historical evidence: neither their branch/PR identity
-- nor their lifecycle may be rewritten by a future call site.
CREATE FUNCTION prevent_retired_proposal_build_attempt_rewrite() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.lifecycle = 'retired' AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION 'retired proposal build attempt is immutable';
    END IF;
    IF OLD.lifecycle = 'reserved' AND NEW.lifecycle NOT IN ('reserved', 'active', 'retired')
       OR OLD.lifecycle = 'active' AND NEW.lifecycle NOT IN ('active', 'retired') THEN
        RAISE EXCEPTION 'illegal proposal build attempt lifecycle transition from % to %', OLD.lifecycle, NEW.lifecycle;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER proposal_build_attempts_retired_immutable
    BEFORE UPDATE ON proposal_build_attempts
    FOR EACH ROW EXECUTE FUNCTION prevent_retired_proposal_build_attempt_rewrite();
