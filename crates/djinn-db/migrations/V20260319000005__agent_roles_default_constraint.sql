-- Enforce: at most one default role per base_role per project.
-- SQLite supports partial unique indexes, so this is fully DB-enforced.

CREATE UNIQUE INDEX agent_roles_one_default_per_base_role
    ON agent_roles(project_id, base_role)
    WHERE is_default = 1;
