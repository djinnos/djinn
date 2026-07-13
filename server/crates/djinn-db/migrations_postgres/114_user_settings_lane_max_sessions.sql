-- Per-user concurrency ceilings for autonomous plan, implement, and review
-- work. Interactive chat is not subject to the plan ceiling.
--
-- Stored as a JSON-object TEXT value:
--   {"plan":1,"implement":3,"review":1}
--
-- NULL deliberately preserves the pre-migration behavior: no lane-specific
-- ceiling. Validation of new writes (1..=10 per lane) happens at the
-- control-plane boundary.

ALTER TABLE user_settings
    ADD COLUMN lane_max_sessions TEXT NULL;
