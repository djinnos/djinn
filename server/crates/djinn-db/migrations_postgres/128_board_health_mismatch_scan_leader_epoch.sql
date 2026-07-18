-- Migration 128: durable allocator for coordinator fencing epochs.
-- Initialize above any state written before this sequence existed, then let
-- every coordinator lifetime claim a distinct, restart-safe epoch.
CREATE SEQUENCE board_health_mismatch_scan_leader_epoch_seq AS BIGINT;
SELECT setval(
    'board_health_mismatch_scan_leader_epoch_seq',
    COALESCE((SELECT MAX(leader_epoch) FROM board_health_mismatch_scan_state), 0) + 1,
    FALSE
);
