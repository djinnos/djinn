-- Remove the dead "resolver" role. Conflict resolution now routes to Worker.

-- Delete any existing resolver agents (default or user-created).
DELETE FROM agents WHERE base_role = 'resolver';

-- Recreate table without "resolver" in the CHECK constraint.
-- SQLite does not support ALTER TABLE … DROP CONSTRAINT, so we rebuild.
CREATE TABLE agents_new (
    id                       TEXT NOT NULL PRIMARY KEY,
    project_id               TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name                     TEXT NOT NULL,
    base_role                TEXT NOT NULL
                                  CHECK(base_role IN (
                                      'worker', 'lead', 'planner',
                                      'architect', 'reviewer'
                                  )),
    description              TEXT NOT NULL DEFAULT '',
    system_prompt_extensions TEXT NOT NULL DEFAULT '',
    model_preference         TEXT,
    verification_command     TEXT,
    mcp_servers              TEXT NOT NULL DEFAULT '[]',
    skills                   TEXT NOT NULL DEFAULT '[]',
    is_default               INTEGER NOT NULL DEFAULT 0,
    created_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    learned_prompt           TEXT
);

INSERT INTO agents_new SELECT * FROM agents;
DROP TABLE agents;
ALTER TABLE agents_new RENAME TO agents;

CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name);
CREATE INDEX IF NOT EXISTS agents_project_id ON agents(project_id);
CREATE INDEX IF NOT EXISTS agents_base_role ON agents(project_id, base_role);
CREATE INDEX IF NOT EXISTS agents_is_default ON agents(project_id, is_default);
CREATE UNIQUE INDEX IF NOT EXISTS agents_one_default_per_base_role
    ON agents(project_id, base_role)
    WHERE is_default = 1;
