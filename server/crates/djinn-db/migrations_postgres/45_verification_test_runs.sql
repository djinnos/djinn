-- 45_verification_test_runs.sql
--
-- Backing store for the verification "test-before-save" gate. A broken/typo'd
-- verification rule fails verification on EVERY in-flight task, so the UI (and
-- the API) must prove candidate rules actually pass before they can be saved.
--
-- Verification runs in the project's image (the worker pod — that's where the
-- toolchain lives; the server host has none), so the test runs the candidate
-- commands in a one-shot Job in that image and writes its result here. The MCP
-- layer polls this row for the outcome, and `project_verification_set` refuses
-- to persist rules unless a matching `passed` row exists (rules_hash match).

CREATE TABLE IF NOT EXISTS verification_test_runs (
    id              VARCHAR(36)  NOT NULL PRIMARY KEY,
    project_id      VARCHAR(36)  NOT NULL,
    -- sha256 of the canonical candidate rules JSON — the save-gate matches the
    -- rules being saved against the rules that were actually tested.
    rules_hash      VARCHAR(128) NOT NULL,
    -- The candidate rules the test Job runs (array of {match_pattern, commands[]}).
    candidate_rules JSONB        NOT NULL DEFAULT '[]'::jsonb,
    -- pending | running | passed | failed | error
    status          VARCHAR(32)  NOT NULL DEFAULT 'pending',
    -- Per-command results: [{name, command, exit_code, stdout, stderr, duration_ms}].
    results         JSONB        NOT NULL DEFAULT '[]'::jsonb,
    error           TEXT         NULL,
    created_at      VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    completed_at    VARCHAR(64)  NULL,
    CONSTRAINT fk_verification_test_runs_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX verification_test_runs_project ON verification_test_runs(project_id);
CREATE INDEX verification_test_runs_status ON verification_test_runs(status);
