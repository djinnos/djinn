-- ADR-042 §4a: Rename issue_type "decomposition" → "planning".
-- The broader "planning" type covers wave decomposition, epic metadata updates,
-- memory-ref attachment, and re-prioritization — all Planner work.
UPDATE tasks SET issue_type = 'planning' WHERE issue_type = 'decomposition';
