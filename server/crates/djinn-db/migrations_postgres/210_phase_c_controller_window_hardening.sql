-- Additive hardening for the existing Phase-C controller-window ledger created
-- by 173_model_turn_admission.sql. No second ledger is introduced: every
-- constraint below attaches to `model_turn_controller_windows` itself.
--
-- The coordinator remains the sole catalog authority. Nothing here validates a
-- provider/model route against a models.dev snapshot or treats the pool label
-- pair as proof of catalog membership; the schema only enforces the structural,
-- closed shape that the typed Rust boundary already asserts, so a row that
-- bypasses that boundary still cannot become a trainable learner window.

-- Exact aligned 60-second half-open bounds at second precision.
ALTER TABLE model_turn_controller_windows
    ADD CONSTRAINT model_turn_controller_windows_aligned_minute CHECK (
        (started_at AT TIME ZONE 'UTC') = date_trunc('minute', started_at AT TIME ZONE 'UTC')
        AND ended_at = started_at + interval '60 seconds'
    );

-- `window_sequence` is the aligned minute index of `started_at`.
ALTER TABLE model_turn_controller_windows
    ADD CONSTRAINT model_turn_controller_windows_sequence_matches_start CHECK (
        window_sequence = floor(extract(epoch FROM (started_at AT TIME ZONE 'UTC')) / 60)::bigint
    );

-- Closed typed summary: exactly four top-level keys, typed values, bounded
-- closed diagnostic reason codes, pool-local-or-zero diagnostic identity, and
-- trainable implying no diagnostics.
CREATE OR REPLACE FUNCTION model_turn_controller_window_summary_is_closed(
    owning_pool_id BIGINT,
    summary TEXT
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $fn$
    -- CASE, not a chain of ANDs: `AND` gives no evaluation-order guarantee, so
    -- an unparsable summary would raise 22P02 out of the cast instead of simply
    -- failing the constraint. The guard has to run first, and only CASE
    -- promises that.
    SELECT CASE
        WHEN summary IS NULL THEN false
        WHEN NOT (summary IS JSON OBJECT) THEN false
        ELSE (
            SELECT (parsed.s - 'provider_id' - 'model_id' - 'trainable' - 'diagnostics' = '{}'::jsonb)
               AND (parsed.s ?& array['provider_id', 'model_id', 'trainable', 'diagnostics'])
               AND jsonb_typeof(parsed.s -> 'provider_id') = 'string'
               AND btrim(parsed.s ->> 'provider_id') <> ''
               AND length(parsed.s ->> 'provider_id') <= 191
               AND jsonb_typeof(parsed.s -> 'model_id') = 'string'
               AND btrim(parsed.s ->> 'model_id') <> ''
               AND length(parsed.s ->> 'model_id') <= 191
               AND jsonb_typeof(parsed.s -> 'trainable') = 'boolean'
               AND jsonb_typeof(parsed.s -> 'diagnostics') = 'array'
               AND jsonb_array_length(parsed.s -> 'diagnostics') <= 64
               AND NOT (
                   (parsed.s ->> 'trainable')::boolean
                   AND jsonb_array_length(parsed.s -> 'diagnostics') > 0
               )
               AND NOT EXISTS (
                   SELECT 1
                   FROM jsonb_array_elements(parsed.s -> 'diagnostics') AS element
                   WHERE jsonb_typeof(element) <> 'object'
                      OR element - 'pool_id' - 'code' <> '{}'::jsonb
                      OR NOT (element ?& array['pool_id', 'code'])
                      OR jsonb_typeof(element -> 'pool_id') <> 'number'
                      OR (element ->> 'pool_id') !~ '^(0|[1-9][0-9]*)$'
                      OR (element ->> 'pool_id')::bigint NOT IN (0, owning_pool_id)
                      OR jsonb_typeof(element -> 'code') <> 'string'
                      OR (element ->> 'code') NOT IN (
                             'empty_expected_denominator', 'missing_capability',
                             'unexpected_capability', 'duplicate_capability',
                             'uncovered_capability', 'partial_capability_coverage',
                             'stale_heartbeat', 'unknown_attempt_path', 'missing_usage',
                             'expired_lease', 'open_breaker', 'missing_stage',
                             'duplicate_stage', 'missing_stage_outcome',
                             'stage_outside_window', 'reversed_stages',
                             'invalid_stage_outcome'
                         )
               )
            FROM (SELECT summary::jsonb AS s) AS parsed
        )
    END;
$fn$;

ALTER TABLE model_turn_controller_windows
    ADD CONSTRAINT model_turn_controller_windows_summary_closed CHECK (
        model_turn_controller_window_summary_is_closed(pool_id, summary)
    );
