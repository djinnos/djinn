-- Durable, typed task execution metadata. NULL preserves legacy task rows.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS execution_context JSONB NULL;
