-- A NULL high-water is valid for an unfinished empty pass, so it cannot also
-- serve as the reset sentinel.
ALTER TABLE board_health_mismatch_scan_state
    ADD COLUMN IF NOT EXISTS active BOOLEAN NOT NULL DEFAULT FALSE;

-- Preserve pre-migration non-empty in-flight passes during rolling upgrade.
UPDATE board_health_mismatch_scan_state
SET active = TRUE
WHERE eligible_high_water_id IS NOT NULL;
