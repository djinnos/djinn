-- Migration 7: Add projects.stack column for Phase 3 stack detection.
--
-- Populated by the mirror-fetcher hook after each successful fetch.
-- Default `'{}'::jsonb` is an empty JSON object; readers treat it as "no
-- detection yet" and the first mirror-fetcher tick overwrites it.

ALTER TABLE projects
    ADD COLUMN stack JSONB NOT NULL DEFAULT '{}'::jsonb;
