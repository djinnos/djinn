-- verify_dedup_report_v1
-- Controlled-query artifact. The integration test evaluates the same inclusion
-- rules against events.json without requiring a production database.
WITH eligible AS (
  SELECT *
  FROM verification_audit_events
  WHERE NOT project_ci
    AND NOT merge_queue_ci
    AND NOT cancelled_before_first_command
    AND NOT infrastructure_wide_outage
    AND verification_input_fingerprint IS NOT NULL
), cohort_measurement AS (
  SELECT
    cohort_name,
    COUNT(*) FILTER (WHERE completed_task_run) AS completed_task_runs,
    COUNT(*) FILTER (WHERE event_kind = 'canonical' AND executes_build_command)
      AS canonical_build_executions,
    COUNT(DISTINCT (project_id, task_id, verification_input_fingerprint))
      AS distinct_fingerprints
  FROM eligible
  GROUP BY cohort_name
)
SELECT
  'verify_dedup_report_v1' AS query_version,
  SUM(canonical_build_executions) AS numerator,
  SUM(distinct_fingerprints) AS denominator,
  SUM(canonical_build_executions)::double precision / SUM(distinct_fingerprints) AS ratio,
  jsonb_agg(cohort_measurement) AS cohort_counts,
  jsonb_build_object(
    'project_ci', true,
    'merge_queue_ci', true,
    'cancelled_before_first_command', true,
    'infrastructure_wide_outage', true,
    'missing_fingerprint', true
  ) AS exclusions
FROM cohort_measurement;
