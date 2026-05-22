-- Per-user preferences. Foundation for future per-user toggles (the first
-- consumer is `auto_approve_prs` driving pr_poller's auto-approve branch).
--
-- Identity comes from `users.id` (added in migration 3). One row per user,
-- created lazily on first set; missing row is read as "all defaults off".

CREATE TABLE IF NOT EXISTS user_settings (
    user_id           VARCHAR(36)  NOT NULL PRIMARY KEY,
    auto_approve_prs  BOOLEAN      NOT NULL DEFAULT FALSE,
    created_at        VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at        VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT fk_user_settings_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
