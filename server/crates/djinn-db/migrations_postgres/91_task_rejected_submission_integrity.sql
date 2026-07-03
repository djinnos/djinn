-- Migration 91: Task-level rejected submission integrity storage.
--
-- The live submit-work guard in `djinn-slot` must reload the *latest* rejected
-- submission fingerprint by `task_id` across redispatch / new task-run
-- boundaries. The task-run-scoped `auto_submit_reviews` table (migration 87)
-- is insufficient on its own: a rejected review's fingerprint is scoped to the
-- `task_run_id` that produced it, so a new task run cannot look up the prior
-- rejection.
--
-- This migration adds an *additive*, focused integrity table that records
-- the latest rejected submission fingerprint at the task level. Each row
-- carries:
--
--   * `task_id`              — the durable task identity (FK to `tasks(id)`).
--   * `task_run_id`          — the task-run that produced the rejection
--                              (FK to `task_runs(id)`); nullable for defensive
--                              callers that only know the task id.
--   * `review_id`            — optional association with the
--                              `auto_submit_reviews.id` row that recorded the
--                              verdict (kept as a plain VARCHAR, not an FK, so
--                              deleting the review row cannot orphan the
--                              durable integrity state).
--   * `verdict_kind`         — short label describing the rejection origin
--                              (e.g. `no_progress`, `reviewer_reject`,
--                              `looping`). Kept free-form so downstream tasks
--                              can introduce new verdict labels without a
--                              migration.
--   * `activity_id`          — optional activity/reject linkage (e.g. the
--                              activity row that captured the rejection event).
--   * `rejected_at`          — ISO-8601 UTC timestamp of the rejection.
--   * `diff_fingerprint`     — the rejected submission's diff fingerprint.
--                              Stored as `TEXT` so it accommodates both the
--                              shared `sha256:<hex>` helper digest and any
--                              legacy short fingerprint without truncation.
--   * `no_progress_streak`   — task-level consecutive no-progress count as of
--                              this rejection. The live guard reloads this to
--                              drive first-bounce vs second-strike behavior.
--   * `created_at`           — row creation timestamp; latest-wins ordering
--                              uses this in tie-breaks.
--
-- Historical tasks that have never had a rejected submission fingerprint
-- recorded produce *no* row here, so `latest_for_task` returns `NULL` rather
-- than fabricating comparison state — the explicit no-comparison path.
--
-- Additive only: no existing objects are dropped or renamed.

CREATE TABLE IF NOT EXISTS task_rejected_submission_integrity (
    id                  VARCHAR(36)  NOT NULL PRIMARY KEY,
    task_id             VARCHAR(36)  NOT NULL,
    task_run_id         VARCHAR(36)  NULL,
    review_id           VARCHAR(36)  NULL,
    verdict_kind        VARCHAR(64)  NOT NULL DEFAULT 'no_progress',
    activity_id         VARCHAR(36)  NULL,
    rejected_at         VARCHAR(64)  NOT NULL,
    diff_fingerprint    TEXT         NOT NULL,
    no_progress_streak  INTEGER      NOT NULL DEFAULT 0,
    created_at          VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z"'),
    CONSTRAINT fk_task_rejected_submission_integrity_task
        FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_rejected_submission_integrity_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE SET NULL
);

-- Latest-by-task lookup: the live guard queries `latest_for_task(task_id)`
-- on every finalize. A composite index on (task_id, created_at DESC) serves
-- that path; a secondary index on (task_id, rejected_at DESC) backs the
-- tie-break `ORDER BY` used by `latest_for_task` when multiple rows share an
-- identical `created_at`.
CREATE INDEX idx_task_rejected_submission_integrity_task_created
    ON task_rejected_submission_integrity(task_id, created_at DESC);
CREATE INDEX idx_task_rejected_submission_integrity_task_rejected_at
    ON task_rejected_submission_integrity(task_id, rejected_at DESC);
CREATE INDEX idx_task_rejected_submission_integrity_task_streak
    ON task_rejected_submission_integrity(task_id, no_progress_streak DESC);
