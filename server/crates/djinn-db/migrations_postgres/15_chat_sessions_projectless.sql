-- ── chat sessions have no project_id ────────────────────────────────────────
-- Global chat (agent_type = 'chat') is a user-scoped conversation that exists
-- outside any project.  Allow sessions.project_id to be NULL and enforce the
-- invariant that every non-chat session still points at a project via a CHECK
-- constraint.

ALTER TABLE sessions ALTER COLUMN project_id DROP NOT NULL;

UPDATE sessions SET project_id = NULL WHERE agent_type = 'chat';

ALTER TABLE sessions ADD CONSTRAINT sessions_project_scope_by_agent_type
  CHECK (
    (agent_type = 'chat' AND project_id IS NULL)
    OR (agent_type <> 'chat' AND project_id IS NOT NULL)
  );
