-- Durable singleton coordinating emergency-to-invocation admission authority.
-- The name check makes `build` the only addressable handoff row; the seeded
-- row means normal deployments always have exactly one such record.
CREATE TABLE admission_handoff (
    name                    VARCHAR(32) NOT NULL PRIMARY KEY,
    phase                   VARCHAR(32) NOT NULL,
    epoch                   BIGINT      NOT NULL,
    emergency_ack_epoch     BIGINT      NULL,
    invocation_ack_epoch    BIGINT      NULL,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT admission_handoff_build_name_check CHECK (name = 'build'),
    CONSTRAINT admission_handoff_epoch_nonnegative_check CHECK (epoch >= 0),
    CONSTRAINT admission_handoff_phase_check CHECK (
        phase IN (
            'emergency_primary',
            'forward_overlap',
            'invocation_primary',
            'rollback_overlap'
        )
    )
);

INSERT INTO admission_handoff (name, phase, epoch)
VALUES ('build', 'emergency_primary', 0)
ON CONFLICT (name) DO NOTHING;
