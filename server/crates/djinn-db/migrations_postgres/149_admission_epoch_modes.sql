-- Version the admission epoch: the durable handoff row now carries the v0/v1
-- authority modes and the reference cap alongside the phase, so one serialized
-- transaction owns modes, cap, phase, and acknowledgements together. A
-- companion table records per-generation acknowledgements for the current
-- epoch, letting an overlap require that every live generation has confirmed
-- v1 before the InvocationPrimary edge commits.

ALTER TABLE admission_handoff
    ADD COLUMN v0_mode VARCHAR(16) NOT NULL DEFAULT 'enforce',
    ADD COLUMN v1_mode VARCHAR(16) NOT NULL DEFAULT 'off',
    ADD COLUMN cap     BIGINT      NULL;

-- v0 authority (the emergency controller) is enforce / observe / disabled.
ALTER TABLE admission_handoff
    ADD CONSTRAINT admission_handoff_v0_mode_check CHECK (
        v0_mode IN ('enforce', 'observe', 'disabled')
    );

-- v1 authority (the invocation controller) is off / shadow / enforce.
ALTER TABLE admission_handoff
    ADD CONSTRAINT admission_handoff_v1_mode_check CHECK (
        v1_mode IN ('off', 'shadow', 'enforce')
    );

-- A configured cap is a positive concurrency bound; NULL means "unset, defer to
-- the controller default".
ALTER TABLE admission_handoff
    ADD CONSTRAINT admission_handoff_cap_positive_check CHECK (
        cap IS NULL OR cap > 0
    );

-- The seeded emergency_primary baseline keeps v0 enforcing and v1 off, which the
-- NOT NULL DEFAULTs above already applied to the existing singleton row. This
-- statement is an explicit, idempotent restatement of that baseline.
UPDATE admission_handoff
   SET v0_mode = 'enforce', v1_mode = 'off'
 WHERE name = 'build';

-- Per-generation acknowledgements for a handoff epoch. Every generation in the
-- live set captured when overlap was armed records exactly one row here; the
-- primary key makes re-acknowledgement idempotent, and keying on epoch means an
-- acknowledgement for a superseded epoch can never satisfy the current one.
CREATE TABLE admission_handoff_generation_ack (
    epoch          BIGINT      NOT NULL,
    generation_key TEXT        NOT NULL,
    acked_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (epoch, generation_key)
);
