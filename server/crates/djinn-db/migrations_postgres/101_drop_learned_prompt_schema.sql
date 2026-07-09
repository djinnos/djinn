-- Drop learned_prompt_history table and agents.learned_prompt column.
-- Final cutover for proposal z5f9: prerequisite epics t8p8 (harvest),
-- 3x0w (runtime removal), and 3sle (generated/UI cleanup) are closed.
--
-- Order: drop dependent history table first, then the column on agents.
DROP TABLE IF EXISTS learned_prompt_history;

ALTER TABLE agents
    DROP COLUMN IF EXISTS learned_prompt;
