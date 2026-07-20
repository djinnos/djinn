-- verify_dedup_report_v1
-- Controlled-query artifact. The integration test evaluates the same inclusion
-- rules against events.json without requiring a production database.
WITH declared_infrastructure_wide_outages(start_inclusive, end_exclusive) AS (
  VALUES ('2025-01-04T12:15:00Z'::timestamptz, '2025-01-04T12:45:00Z'::timestamptz)
), eligible AS (
  SELECT event.*
  FROM verification_audit_events AS event
  WHERE NOT event.project_ci
    AND NOT event.merge_queue_ci
    AND NOT event.cancelled_before_first_command
    AND event.verification_input_fingerprint IS NOT NULL
    AND NOT EXISTS (
      SELECT 1
      FROM declared_infrastructure_wide_outages AS outage
      WHERE event.event_timestamp >= outage.start_inclusive
        AND event.event_timestamp < outage.end_exclusive
    )
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
