-- Persist the current-head required-CI snapshot for PR-backed tasks.
--
-- This table intentionally stores only the durable snapshot. Repository APIs,
-- DTO exposure, pr_poller write-through, and lifecycle policy are wired by
-- follow-up tasks in the CI gate foundation epic.

CREATE TABLE IF NOT EXISTS task_pr_ci_snapshots (
    task_id                       VARCHAR(36) NOT NULL,
    pr_number                     BIGINT      NOT NULL,
    head_sha                      VARCHAR(64) NULL DEFAULT NULL,
    ci_status                     TEXT        NOT NULL DEFAULT 'unknown',
    blocking_required_check_names JSONB       NOT NULL DEFAULT '[]'::jsonb,
    failure_fingerprint           TEXT        NULL DEFAULT NULL,
    first_seen_at                 VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    last_seen_at                  VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    same_signature_count          BIGINT      NOT NULL DEFAULT 0,
    last_remediation_base_sha     VARCHAR(64) NULL DEFAULT NULL,

    CONSTRAINT task_pr_ci_snapshots_pkey
        PRIMARY KEY (task_id, pr_number),
    CONSTRAINT task_pr_ci_snapshots_task_id_fkey
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT task_pr_ci_snapshots_ci_status_check
        CHECK (ci_status IN ('passing', 'failing', 'pending', 'unknown')),
    CONSTRAINT task_pr_ci_snapshots_blocking_names_array_check
        CHECK (jsonb_typeof(blocking_required_check_names) = 'array'),
    CONSTRAINT task_pr_ci_snapshots_same_signature_count_check
        CHECK (same_signature_count >= 0)
);

CREATE INDEX IF NOT EXISTS task_pr_ci_snapshots_ci_status_idx
    ON task_pr_ci_snapshots(ci_status);

CREATE INDEX IF NOT EXISTS task_pr_ci_snapshots_last_seen_at_idx
    ON task_pr_ci_snapshots(last_seen_at);

-- Backfill existing open PR-backed tasks to the safe unknown state. The PR
-- number is parsed the same way as the Rust pr_poller helper: GitHub URLs of
-- the form https://github.com/{owner}/{repo}/pull/{number}, allowing optional
-- query strings/fragments after the number.
INSERT INTO task_pr_ci_snapshots (task_id, pr_number)
SELECT t.id,
       parsed.pr_number::BIGINT
  FROM tasks AS t
  CROSS JOIN LATERAL (
      SELECT substring(
          t.pr_url FROM '^https://github\.com/[^/]+/[^/]+/pull/([0-9]+)(?:[?#].*)?$'
      ) AS pr_number
  ) AS parsed
 WHERE t.pr_url IS NOT NULL
   AND t.status <> 'closed'
   AND parsed.pr_number IS NOT NULL
ON CONFLICT (task_id, pr_number) DO NOTHING;
