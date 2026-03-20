-- ADR-034 §1: rename PM → Lead in task status values.
-- Existing tasks with 'needs_pm_intervention' or 'in_pm_intervention' are migrated
-- to the new 'needs_lead_intervention' / 'in_lead_intervention' string values.

UPDATE tasks SET status = 'needs_lead_intervention' WHERE status = 'needs_pm_intervention';
UPDATE tasks SET status = 'in_lead_intervention'    WHERE status = 'in_pm_intervention';
