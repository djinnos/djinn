-- Migration 5: Task runs as first-class entities.
--
-- Shifts the execution model from "per-agent session" to "per-task-run".
-- A task_run is one row per task execution (spanning planner → worker →
-- reviewer → verifier stages); existing `sessions` rows become child records
-- of a task_run via the new `sessions.task_run_id` FK.
--
-- This migration is additive-safe: no existing columns are dropped, so it
-- can be applied before `SessionRecord` consumers are rewritten. The
-- corresponding drop of `sessions.worktree_path` ships in migration 6
-- once all consumers read from `task_runs.workspace_path` instead.

CREATE TABLE IF NOT EXISTS task_runs (
    id                VARCHAR(36)   NOT NULL PRIMARY KEY,
    project_id        VARCHAR(36)   NOT NULL,
    task_id           VARCHAR(36)   NOT NULL,
    trigger_type      VARCHAR(64)   NOT NULL,
    `status`          VARCHAR(64)   NOT NULL,
    started_at        VARCHAR(64)   NOT NULL DEFAULT (DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')),
    ended_at          VARCHAR(64)   NULL,
    workspace_path    VARCHAR(1024) NULL,
    mirror_ref        VARCHAR(255)  NULL,
    CONSTRAINT fk_task_runs_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT fk_task_runs_task
        FOREIGN KEY (task_id)    REFERENCES tasks(id)    ON DELETE CASCADE
);

CREATE INDEX idx_task_runs_task_id_started_at    ON task_runs(task_id, started_at);
CREATE INDEX idx_task_runs_project_id_started_at ON task_runs(project_id, started_at);
CREATE INDEX idx_task_runs_status                ON task_runs(`status`);

ALTER TABLE sessions
    ADD COLUMN task_run_id VARCHAR(36) NULL,
    ADD CONSTRAINT fk_sessions_task_run
        FOREIGN KEY (task_run_id) REFERENCES task_runs(id) ON DELETE SET NULL;

CREATE INDEX idx_sessions_task_run_id ON sessions(task_run_id);
