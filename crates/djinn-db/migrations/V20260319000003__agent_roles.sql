-- Agent roles: configurable per-project role definitions (default + specialist instances).

CREATE TABLE IF NOT EXISTS agent_roles (
    id                       TEXT NOT NULL PRIMARY KEY,
    project_id               TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name                     TEXT NOT NULL,
    base_role                TEXT NOT NULL
                                  CHECK(base_role IN (
                                      'worker', 'lead', 'planner',
                                      'architect', 'reviewer', 'resolver'
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
    UNIQUE(project_id, name)
);

CREATE INDEX agent_roles_project_id     ON agent_roles(project_id);
CREATE INDEX agent_roles_base_role      ON agent_roles(project_id, base_role);
CREATE INDEX agent_roles_is_default     ON agent_roles(project_id, is_default);
