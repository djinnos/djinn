-- Add missing indexes on the agents table.
-- These were originally appended to V20260409000001 after it had already been
-- applied, which broke refinery's checksum validation.

CREATE INDEX IF NOT EXISTS agents_project_id ON agents(project_id);
CREATE INDEX IF NOT EXISTS agents_base_role ON agents(project_id, base_role);
CREATE INDEX IF NOT EXISTS agents_is_default ON agents(project_id, is_default);
CREATE UNIQUE INDEX IF NOT EXISTS agents_one_default_per_base_role
    ON agents(project_id, base_role)
    WHERE is_default = 1;
