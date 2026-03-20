-- Add agent_type to tasks so the Planner can route tasks to specialist roles.
ALTER TABLE tasks ADD COLUMN agent_type TEXT;
