-- Rename agent_roles table to agents and update related structures.

ALTER TABLE agent_roles RENAME TO agents;

-- Recreate indexes with updated names.
DROP INDEX IF EXISTS agent_roles_project_id;
DROP INDEX IF EXISTS agent_roles_base_role;
DROP INDEX IF EXISTS agent_roles_is_default;

CREATE INDEX agents_project_id ON agents(project_id);
CREATE INDEX agents_base_role   ON agents(project_id, base_role);
CREATE INDEX agents_is_default  ON agents(project_id, is_default);

-- Rename role_id column in learned_prompt_history to agent_id.
ALTER TABLE learned_prompt_history RENAME COLUMN role_id TO agent_id;

DROP INDEX IF EXISTS learned_prompt_history_role_id;
CREATE INDEX learned_prompt_history_agent_id ON learned_prompt_history(agent_id);
