-- Migration 125: durable singleton state for the bounded board-health scan.
CREATE TABLE IF NOT EXISTS board_health_mismatch_scan_state (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    cursor_id VARCHAR(36) NULL,
    eligible_high_water_id VARCHAR(36) NULL,
    pass_id VARCHAR(36) NULL,
    pass_started_at VARCHAR(64) NULL,
    leader_epoch BIGINT NOT NULL DEFAULT 0,
    completed_at VARCHAR(64) NULL,
    last_pass_duration_ms BIGINT NULL,
    updated_at VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);
