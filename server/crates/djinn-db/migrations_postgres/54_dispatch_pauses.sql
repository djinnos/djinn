-- Persist project- and user-scoped dispatch emergency-stop pauses.
--
-- Global pause state is stored in settings.raw as part of DjinnSettings so old
-- settings rows without that field continue to represent an unpaused default.
CREATE TABLE IF NOT EXISTS dispatch_pauses (
    scope      VARCHAR(32)  NOT NULL,
    target_id  VARCHAR(255) NOT NULL,
    paused_by  VARCHAR(255) NOT NULL,
    paused_at  VARCHAR(64)  NOT NULL,
    reason     TEXT         NOT NULL,
    expires_at VARCHAR(64)  NULL,
    PRIMARY KEY (scope, target_id),
    CONSTRAINT dispatch_pauses_scope_check CHECK (scope IN ('project', 'user'))
);

CREATE INDEX IF NOT EXISTS dispatch_pauses_scope_idx ON dispatch_pauses(scope);
