-- 11_drop_verification_rules.sql
--
-- Drop `projects.verification_rules` now that it is no longer
-- authoritative. The P5 boot reseed hook copied every row's contents
-- into `environment_config.verification.rules` verbatim, so the data
-- already lives on the new column.

ALTER TABLE projects DROP COLUMN verification_rules;
