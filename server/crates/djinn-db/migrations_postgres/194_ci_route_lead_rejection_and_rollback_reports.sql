-- Wave 5 of proposal `nafu`: the Lead-rejection cause, the rollback quiescence
-- report, and the positive-run-id guard.
--
-- All of this DDL was originally written into `193_ci_route_attempts.sql`
-- directly. That was wrong: 193 merged to main ahead of this branch, so any
-- database that had already applied it stores 193's checksum in
-- `_sqlx_migrations` and refuses to boot once the file diverges. 193 has been
-- restored byte-for-byte and the wave-5 delta lives here instead, as a new
-- migration, which is the only way to change an applied schema.

-- ---------------------------------------------------------------------------
-- Why the delivered Lead result was REPLACED by the diagnostic fallback.
-- ---------------------------------------------------------------------------
--
-- Without this column, reporting cannot tell "Lead diagnosed" from "Lead never
-- answered". The supervisor derives the fallback's `diagnostic_reason` from the
-- route's own Tier-2 reason, so a Lead that timed out on a `causal_failure`
-- route lands as `no_grounded_remedy` -- exactly the spelling a Lead that *did*
-- answer and found no remedy produces. The two are opposite operational facts
-- (one needs a better prompt or a longer deadline, the other needs a human) and
-- used to be indistinguishable in the durable record because the rejection
-- existed only inside a `tracing::warn!`.
--
-- NULL means the delivered result was accepted as submitted.
ALTER TABLE ci_route_attempts ADD COLUMN lead_rejection VARCHAR(48) NULL;

-- The closed rejection vocabulary. Same rule as every other durable vocabulary
-- in 193: spelled once in Rust, once in this CHECK.
ALTER TABLE ci_route_attempts
    ADD CONSTRAINT ci_route_attempts_lead_rejection_check
    CHECK (lead_rejection IS NULL OR lead_rejection IN (
        'approved_non_passing_ci',
        'reopen_plan_ambiguous',
        'verification_command_not_repository_valid',
        'directive_not_grounded',
        'diagnostic_reason_unknown',
        'park_unavailable_for_route',
        'park_not_cited',
        'supersede_without_replacements',
        'unknown_decision',
        'unsupported_finalize_tool',
        'no_result',
        'timed_out'
    ));

-- A rejection means the delivered result was replaced by the single diagnostic
-- fallback. It cannot coexist with an accepted repair, park, or supersede --
-- those are results Lead actually produced.
ALTER TABLE ci_route_attempts
    ADD CONSTRAINT ci_route_attempts_rejection_pairing_check
    CHECK (lead_rejection IS NULL OR reopen_mode = 'diagnose');

-- ---------------------------------------------------------------------------
-- DELIBERATELY ABSENT: `CHECK (run_id > 0)`.
-- ---------------------------------------------------------------------------
--
-- `run_id` is the provider's run identifier and the discriminating part of the
-- `provider_action_key`. Wave 5 fabricated a placeholder identity with
-- `run_id = 0` for lane-level captures that could not resolve a real run, which
-- collapsed two distinct dequeues of the same head onto one key: the second
-- route silently reused the first route's row. That fabrication has been
-- removed from every path where a reason can either name a real run or hold --
-- see the accompanying coordinator change.
--
-- THREE producers remain, all on the PR-head lane, and none can be fixed here.
-- `capture_pr_head_evidence` answers both of the following BEFORE it attributes
-- runs, so no run identity exists at the point it fails closed:
--
--   1. `MaxPagesTruncated` -> `CheckEnumerationUnavailable`, which is
--      deliberately NOT an enumeration failure and so takes a Tier-2 row.
--      Making it hold is the MAX_PAGES-truncation deviation currently before
--      the tribunal (AC5/AC12/AC14); that code is deliberately untouched.
--   2. The four `blocking_evidence_completeness` timestamp reasons
--      (`MissingStartTimestamp`, `MissingCompletionTimestamp`,
--      `MalformedExecutionInterval`, `NonPositiveExecutionInterval`). The
--      honest fix is to attribute runs first and fan the lane out with every
--      run forced incomplete, which changes how many route rows one lane may
--      emit (1 -> N). That is a spec-level behaviour change, not a bug fix.
--   3. `AmbiguousMergeGroupCorrelation` is spec-mandated Tier 2, and
--      "ambiguous" means no single run exists to name. Its route rows no
--      longer collide -- they now carry the REAL `dequeue_id`, which was the
--      actual defect -- but `run_id` still has no honest value.
--
-- Adding the constraint while these survive would convert a silent key
-- collision into a hard INSERT failure -- trading a quiet bug for an outage.
-- The constraint becomes correct once the evidence identity gains an absent
-- encoding for the run fields, or once the PR-head lane fans out. Until then
-- the invariant is pinned in Rust, in
-- `ci_routing::tests::every_incomplete_reason_either_holds_or_names_a_real_identity`,
-- which classifies all twelve reasons through an exhaustive match with no
-- wildcard arm and pins the membership of the placeholder list -- so any NEW
-- fabricated identity fails the build.

-- ---------------------------------------------------------------------------
-- The rollback quiescence report.
-- ---------------------------------------------------------------------------
--
-- The proposal permits a binary rollback "only after one repository-checkable
-- report records zero" across six counts. "One report" and "repository-
-- checkable" are the operative words: an operator must be able to point at a
-- single durable row, taken at a single instant, rather than at six separate
-- queries that were each true at a different moment while a route advanced
-- between them.
--
-- Two of the six counts are NOT knowable from SQL and are therefore attested
-- by the recording coordinator: `registered_provider_futures` comes from the
-- in-process `ProviderActionScope`, and the evidence-advance high-watermark is
-- the coordinator's own count of route identities that are still the current
-- failed evidence for their lane. Both are stored, so a report that permitted
-- a rollback can be re-read afterwards and blamed if it was wrong.
--
-- Append-only: a later report never overwrites an earlier one. The decision to
-- roll back was made against a specific report and that report has to survive
-- it.
CREATE TABLE ci_route_rollback_reports (
    id                          VARCHAR(36)  NOT NULL,
    -- The gate posture at the moment of the report. A report taken while the
    -- gate is still `enabled` proves nothing about rollback safety, because
    -- new routes are still being admitted; the reader enforces this rather
    -- than a CHECK, so the useless-but-honest report is still recordable.
    gate_state                  VARCHAR(16)  NOT NULL,
    reserved_rows               BIGINT       NOT NULL,
    calling_rows                BIGINT       NOT NULL,
    open_tier2_leases           BIGINT       NOT NULL,
    unapplied_lead_results      BIGINT       NOT NULL,
    -- Attested from the in-process provider-action scope of the recording
    -- coordinator. Not derivable from any table.
    registered_provider_futures BIGINT       NOT NULL,
    -- The evidence-advance high-watermark: route identities that remain the
    -- CURRENT failed evidence for their PR or merge-group lane. Zero is the
    -- proposal's "every routed identity must have advanced to distinct newer
    -- provider evidence or reached passing or merged state".
    current_failed_identities   BIGINT       NOT NULL,
    -- Denormalized verdict, written by the same code that computed the counts
    -- so a reader cannot disagree with the recorder about what the row means.
    permits_rollback            BOOLEAN      NOT NULL,
    recorded_by_incarnation     VARCHAR(36)  NOT NULL,
    recorded_at                 TIMESTAMPTZ  NOT NULL DEFAULT now(),

    CONSTRAINT ci_route_rollback_reports_pkey PRIMARY KEY (id),
    CONSTRAINT ci_route_rollback_reports_gate_check
        CHECK (gate_state IN ('enabled', 'quiescing', 'disabled_clean')),
    CONSTRAINT ci_route_rollback_reports_nonneg_check
        CHECK (reserved_rows >= 0 AND calling_rows >= 0 AND open_tier2_leases >= 0
               AND unapplied_lead_results >= 0 AND registered_provider_futures >= 0
               AND current_failed_identities >= 0),
    -- The verdict is a function of the counts, and the database refuses to
    -- store a row where it is not. Without this, a caller that computed
    -- `permits_rollback` from five of the six counts could persist a green
    -- report over a live `calling` row, and the "one repository-checkable
    -- report" would be the thing that authorized stranding it.
    CONSTRAINT ci_route_rollback_reports_verdict_check
        CHECK (permits_rollback = (
            gate_state <> 'enabled'
            AND reserved_rows = 0 AND calling_rows = 0 AND open_tier2_leases = 0
            AND unapplied_lead_results = 0 AND registered_provider_futures = 0
            AND current_failed_identities = 0
        ))
);

CREATE INDEX ci_route_rollback_reports_recorded_idx
    ON ci_route_rollback_reports(recorded_at DESC);
