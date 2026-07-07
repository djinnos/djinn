-- zkk9 — one-shot monitored reopen prompt injection marker.
--
-- Keep applied migration 96 immutable and add the prompt-consumption flag as a
-- forward migration. The repository atomically flips this flag when the next
-- worker prompt claims the arbiter reopen directive so re-entry cannot inject
-- the directive a second time before terminal completion consumes the row.

ALTER TABLE task_arbitrations
    ADD COLUMN IF NOT EXISTS directive_injected BOOLEAN NOT NULL DEFAULT false;
