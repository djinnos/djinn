-- Per-user preferences. Foundation for future per-user toggles (the first
-- consumer is `auto_approve_prs` driving pr_poller's auto-approve branch;
-- spend limits and other Phase 1+ per-user features will land additional
-- typed columns here rather than a JSON blob, mirroring the schema shape
-- of `projects` and `settings`).
--
-- Identity comes from `users.id` (added in migration 3). One row per user,
-- created lazily on first set; missing row is read as "all defaults off".
-- FK CASCADE means user removal cleans up automatically.

CREATE TABLE IF NOT EXISTS user_settings (
    user_id           VARCHAR(36)  NOT NULL PRIMARY KEY,
    auto_approve_prs  BOOLEAN      NOT NULL DEFAULT FALSE,
    created_at        VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    updated_at        VARCHAR(64)  NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    CONSTRAINT fk_user_settings_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
