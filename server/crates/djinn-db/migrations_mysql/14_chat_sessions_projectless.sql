-- ── chat sessions have no project_id ────────────────────────────────────────
-- Global chat (agent_type = 'chat') is a user-scoped conversation that exists
-- outside any project.  Allow sessions.project_id to be NULL and enforce the
-- invariant that every non-chat session still points at a project via a CHECK
-- constraint.
--
-- Ordering is important:
--   1. MODIFY the column to nullable so the later UPDATE can NULL it.
--   2. Backfill any pre-existing chat rows to NULL so the CHECK doesn't
--      reject them when it's added.
--   3. Install the CHECK enforcing the agent_type ↔ project_id invariant.

ALTER TABLE sessions MODIFY COLUMN project_id VARCHAR(36) NULL;

UPDATE sessions SET project_id = NULL WHERE agent_type = 'chat';

ALTER TABLE sessions ADD CONSTRAINT sessions_project_scope_by_agent_type
  CHECK (
    (agent_type = 'chat' AND project_id IS NULL)
    OR (agent_type <> 'chat' AND project_id IS NOT NULL)
  );
