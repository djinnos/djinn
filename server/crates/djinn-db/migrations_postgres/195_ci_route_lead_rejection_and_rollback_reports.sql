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
        -- A `repair` reopen on a run-absent route. A route whose `run_id` is
        -- NULL never named a run, so there is nothing to re-run and no CI
        -- evidence to ground a verification command in: the only honest
        -- adjudication left is a diagnose. Enforced twice, deliberately --
        -- in the agent-side validator (which can explain itself to Lead) and
        -- in `resolve_tier2_lease` (which is the last writer before the row
        -- becomes durable).
        'repair_unavailable_for_route',
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
-- The run-absent evidence identity.
-- ---------------------------------------------------------------------------
--
-- `run_id` is the provider's run identifier and the discriminating part of the
-- `provider_action_key`. Wave 5 fabricated a placeholder identity with
-- `run_id = 0` for lane-level captures that could not resolve a real run, which
-- collapsed two distinct dequeues of the same head onto one key: the second
-- route silently reused the first route's row and therefore silently never
-- existed.
--
-- The fabrication is now gone, and this migration makes it unrepresentable:
-- the column carries the ABSENCE of a run rather than a stand-in for one.
-- `NULL` is the only spelling of "this capture never named a run"; the
-- positivity CHECK means no future caller can revive the sentinel by writing
-- `0` (or a negative id) instead.
--
-- Three producers remain that legitimately have no run to name, all on the
-- PR-head lane, and all of them now write NULL:
--
--   1. `MaxPagesTruncated` -> `CheckEnumerationUnavailable`, which is
--      deliberately NOT an enumeration failure and so takes a Tier-2 row.
--   2. The four `blocking_evidence_completeness` timestamp reasons
--      (`MissingStartTimestamp`, `MissingCompletionTimestamp`,
--      `MalformedExecutionInterval`, `NonPositiveExecutionInterval`), all of
--      which are answered BEFORE runs are attributed.
--   3. `AmbiguousMergeGroupCorrelation`, which is spec-mandated Tier 2 and
--      where "ambiguous" means no single run exists to name.
--
-- A run-absent route is therefore DIAGNOSE-ONLY: with no run there is nothing
-- to re-run and no run-scoped evidence to ground a repair's verification
-- command in. That is enforced in `resolve_tier2_lease` (which rejects a
-- `repair_reopened` resolution on a row whose `run_id IS NULL`) and by the
-- `repair_unavailable_for_route` rejection above. It is not expressible as a
-- CHECK on this table, because the reopen mode and the run id are written by
-- two different statements at two different times and a CHECK sees only the
-- post-image of one row -- a row that legally passes through
-- `reopen_mode IS NULL` first.
ALTER TABLE ci_route_attempts ALTER COLUMN run_id DROP NOT NULL;
ALTER TABLE ci_route_attempts ADD CONSTRAINT ci_route_attempts_run_id_positive_check
    CHECK (run_id IS NULL OR run_id > 0);

-- The database's own witness of the caller-computed `provider_action_key`.
--
-- `provider_action_key` is a hash the CALLER computes over the expanded
-- identity plus the action, and the primary key on it is the only thing that
-- makes a duplicate poll idempotent. That leaves one unprovable step: if the
-- caller's hash function ever stopped covering a field, the primary key would
-- keep accepting the row and nothing would notice. This index computes the same
-- collapse from the STORED columns, so the two agree or the insert fails.
--
-- `NULLS NOT DISTINCT` (PostgreSQL 15+; this schema targets 16) is the load
-- bearing clause. Under the default `NULLS DISTINCT`, every run-absent row is
-- unique to itself no matter how many of them there are: two different
-- irrecoverable reasons on ONE lane/PR/head/dequeue would each take a row, each
-- open its own route, and the "at most one provider-call episode per evidence
-- identity" invariant would hold only for identities that happened to name a
-- run. With `NULLS NOT DISTINCT` the run-absent rows COLLAPSE onto one, which
-- is the correct reading: the evidence identity is what the route acts on, and
-- two captures that could not name a run are not two identities.
--
-- The honest limit: this index cannot see the reason. Two genuinely different
-- irrecoverable reasons on one identity report the FIRST route's row back to
-- the second caller (`CiReserveOutcome::AlreadyPresent`), so the second
-- reason is not durably recorded anywhere on this table. That is deliberate --
-- one identity earns one route and one Lead adjudication - but a wave that
-- needs both reasons must add a child table rather than widen this index.
CREATE UNIQUE INDEX ci_route_attempts_evidence_identity_uniq
    ON ci_route_attempts (subject_kind, subject_id, lane, pr_number, pr_head_sha,
                          run_id, run_head_sha, dequeue_id, action)
    NULLS NOT DISTINCT;

-- ---------------------------------------------------------------------------
-- Budget monotonicity, as a trigger rather than a convention.
-- ---------------------------------------------------------------------------
--
-- 193 says the budgets are monotonic and backs it with `charged_count >= 0`
-- plus "the repository never issues a decrement". Those are two different
-- claims and only the first was enforced: a non-negativity CHECK is satisfied
-- by 5 -> 3 -> 1, so the actual monotonicity rested entirely on every present
-- and future writer remembering not to decrement. That is the shape of
-- invariant that survives review and dies to the first refund path someone
-- adds "just for the cancelled action".
--
-- A `BEFORE UPDATE` trigger is the only enforcement that a raw UPDATE cannot
-- walk around. It raises with SQLSTATE 23514 (`check_violation`) so callers and
-- tests classify it exactly as they classify the CHECKs beside it.
--
-- Deliberately NOT covered: DELETE. Deleting a counter row is how a subject's
-- routing history is cascaded away, and blocking it here would make a task
-- delete fail. The claim is "a live counter never goes backwards", not "a
-- counter is immortal".
CREATE FUNCTION ci_route_budget_counters_forbid_decrease() RETURNS trigger AS $$
BEGIN
    IF NEW.charged_count < OLD.charged_count THEN
        RAISE EXCEPTION
            'ci_route_budget_counters.charged_count is monotonic: % -> % for (%,%,%,%)',
            OLD.charged_count, NEW.charged_count,
            OLD.subject_kind, OLD.subject_id, OLD.scope, OLD.counter_key
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ci_route_budget_counters_monotonic
    BEFORE UPDATE ON ci_route_budget_counters
    FOR EACH ROW EXECUTE FUNCTION ci_route_budget_counters_forbid_decrease();

-- ---------------------------------------------------------------------------
-- A park must cite its cause.
-- ---------------------------------------------------------------------------
--
-- The repository already refuses `CiTier2Resolution::park` without a
-- justification, and reporting counts `parks_with_cited_cause`. Both were
-- reading the same Rust guard: the metric restated the label `parked` and
-- called the restatement a citation. This CHECK is the independent witness --
-- a park row without a non-blank justification cannot exist at all, whatever
-- wrote it, so the metric now measures a fact the database enforces rather
-- than a promise the repository makes.
--
-- Stated over `COALESCE(tier2_resolution, terminal_outcome)` because a park is
-- an ADJUDICATION and terminalization is write-once: a route that already
-- terminalized on `action_failed` records its park in `tier2_resolution` and
-- never touches `terminal_outcome`. A CHECK on `terminal_outcome` alone would
-- miss exactly the population that reaches Lead through a provider failure.
ALTER TABLE ci_route_attempts ADD CONSTRAINT ci_route_attempts_park_cited_check
    CHECK (COALESCE(tier2_resolution, terminal_outcome) IS DISTINCT FROM 'parked'
           OR (park_justification IS NOT NULL AND btrim(park_justification) <> ''));

-- ---------------------------------------------------------------------------
-- Lead sessions are counted, not overwritten.
-- ---------------------------------------------------------------------------
--
-- `lead_session_id` is a single slot and `attach_lead_session` used to blind-
-- overwrite it, so a route that spent TWO Lead sessions (a respawn after a
-- provider transport failure, a re-dispatch after an empty turn) reported one.
-- `lead_invocations` counted rows with a non-NULL session id, so the cost
-- metric the proposal exists to bound was structurally incapable of exceeding
-- one per route.
--
-- The fix is append-only counting: `lead_session_id` retains the FIRST session
-- (it is the audit handle for the adjudication that opened) and this column
-- counts every attach. The count is what metrics read; the id is what an
-- operator greps.
ALTER TABLE ci_route_attempts ADD COLUMN lead_session_count BIGINT NOT NULL DEFAULT 0;

ALTER TABLE ci_route_attempts
    ADD CONSTRAINT ci_route_attempts_lead_session_count_check
    CHECK (lead_session_count >= 0);

-- A count without an id is a session nobody can find. The converse is legal
-- and transient only in the sense that the two are written by one statement.
ALTER TABLE ci_route_attempts
    ADD CONSTRAINT ci_route_attempts_lead_session_pairing_check
    CHECK (lead_session_id IS NOT NULL OR lead_session_count = 0);

-- ---------------------------------------------------------------------------
-- The bounded incomplete-evidence hold.
-- ---------------------------------------------------------------------------
--
-- When a lane's check enumeration comes back RECOVERABLY incomplete (a run
-- still in progress, a check suite not yet reported), the correct response is
-- neither to route nor to drop it: it is to hold and poll again. Unbounded
-- holding is a silent stall, so the hold is bounded -- after
-- `CI_INCOMPLETE_HOLD_MAX_POLLS` consecutive incomplete polls the streak
-- escalates ONCE and the caller opens one diagnose-only route.
--
-- Everything difficult here is ordering. Polls run concurrently (two
-- coordinator ticks, a restart overlapping a live poll), a provider
-- enumeration takes seconds, and NO transaction may span it -- holding a row
-- lock across a provider round trip is how a slow GitHub becomes a database
-- outage. So the poll is two short transactions with the provider call in the
-- gap, and ARRIVAL ORDER after that gap is meaningless: a poll that started
-- first can finish last.
--
-- `next_poll_sequence` is the authority order. Transaction #1 assigns a
-- sequence under the row lock and commits; transaction #2 compares that
-- reserved sequence against `last_applied_poll_sequence` and applies only if
-- it is strictly greater. A late poll's stale answer is therefore SUPERSEDED,
-- not counted -- it neither increments the streak nor resets it. Without this,
-- two concurrent pollers of one still-incomplete lane would count two polls
-- toward the bound and escalate in half the time, and a late INCOMPLETE answer
-- arriving after a COMPLETE one would resurrect a streak that had legitimately
-- reset.
--
-- Identity is `(repository_id, pr_number, pr_head_sha, lane, dequeue_id)`,
-- scoped additionally by the route subject like every other relation here.
-- `repository_id` is `owner/repo`, which is MUTABLE: a repository rename
-- mid-flight starts a fresh streak with a zero count. That is safe HERE in a
-- way it would not be on `ci_route_attempts`, because a streak only authorizes
-- COUNTING -- it never authorizes a provider call, so a duplicated streak
-- costs at most a few extra polls before the bound is reached again. A route
-- row carries a provider-call authorization and could not tolerate the same
-- laxity.
CREATE TABLE ci_incomplete_hold_streaks (
    id                          VARCHAR(36)  NOT NULL PRIMARY KEY,
    subject_kind                VARCHAR(32)  NOT NULL,
    subject_id                  VARCHAR(36)  NOT NULL,
    -- `owner/repo`. See the note above on why a rename is tolerable here.
    repository_id               VARCHAR(255) NOT NULL,
    pr_number                   BIGINT       NOT NULL,
    pr_head_sha                 VARCHAR(64)  NOT NULL,
    lane                        VARCHAR(16)  NOT NULL,
    dequeue_id                  VARCHAR(128) NULL,

    -- The reservation counter. Monotonic, and the ONLY source of poll order.
    next_poll_sequence          BIGINT       NOT NULL DEFAULT 0,
    -- The retained high-watermark. An observation whose reserved sequence is
    -- `<=` this has been overtaken and applies nothing.
    last_applied_poll_sequence  BIGINT       NOT NULL DEFAULT 0,
    -- Consecutive incomplete polls. Reset to zero by a complete enumeration;
    -- the ROW and the high-watermark survive that reset, so a later stale
    -- observation still cannot apply.
    poll_count                  BIGINT       NOT NULL DEFAULT 0,
    -- Set exactly once, in the same transaction as the increment that reached
    -- the bound. Non-NULL is what makes every subsequent incomplete poll a
    -- no-op rather than a second escalation.
    escalated_at                TIMESTAMPTZ  NULL,

    created_at                  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at                  TIMESTAMPTZ  NOT NULL DEFAULT now(),

    CONSTRAINT ci_incomplete_hold_streaks_subject_kind_check
        CHECK (subject_kind IN ('task', 'proposal')),
    CONSTRAINT ci_incomplete_hold_streaks_lane_check
        CHECK (lane IN ('pr_head', 'merge_group')),
    CONSTRAINT ci_incomplete_hold_streaks_next_sequence_check
        CHECK (next_poll_sequence >= 0),
    CONSTRAINT ci_incomplete_hold_streaks_applied_sequence_check
        CHECK (last_applied_poll_sequence >= 0),
    CONSTRAINT ci_incomplete_hold_streaks_poll_count_check
        CHECK (poll_count >= 0),
    -- A sequence cannot be applied before it was reserved.
    CONSTRAINT ci_incomplete_hold_streaks_watermark_check
        CHECK (last_applied_poll_sequence <= next_poll_sequence)
);

-- `NULLS NOT DISTINCT` for the same reason as the evidence-identity index: the
-- PR-head lane has no dequeue id, and under the default every PR-head poll
-- would mint a fresh streak, so the bound would never be reached.
CREATE UNIQUE INDEX ci_incomplete_hold_streaks_identity_uniq
    ON ci_incomplete_hold_streaks (subject_kind, subject_id, repository_id,
                                   pr_number, pr_head_sha, lane, dequeue_id)
    NULLS NOT DISTINCT;

-- Both sequence columns are high-watermarks and neither may retreat.
--
-- Strictly, the repository never writes a decrease: `reserve_poll` only
-- increments and `apply_poll` only assigns a value it has already proven
-- greater. The trigger exists because "retained high-watermark" is the
-- property the whole ordering contract rests on, and an invariant enforced
-- only by the code that happens to write it is a convention, not a guarantee.
-- With this, a raw `UPDATE ... SET last_applied_poll_sequence = 0` -- a
-- well-meant operational "reset this stuck streak" -- fails instead of
-- silently re-admitting every superseded observation.
CREATE FUNCTION ci_incomplete_hold_streaks_forbid_sequence_decrease() RETURNS trigger AS $$
BEGIN
    IF NEW.next_poll_sequence < OLD.next_poll_sequence THEN
        RAISE EXCEPTION
            'ci_incomplete_hold_streaks.next_poll_sequence is monotonic: % -> % for streak %',
            OLD.next_poll_sequence, NEW.next_poll_sequence, OLD.id
            USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.last_applied_poll_sequence < OLD.last_applied_poll_sequence THEN
        RAISE EXCEPTION
            'ci_incomplete_hold_streaks.last_applied_poll_sequence is a retained high-watermark: % -> % for streak %',
            OLD.last_applied_poll_sequence, NEW.last_applied_poll_sequence, OLD.id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ci_incomplete_hold_streaks_monotonic
    BEFORE UPDATE ON ci_incomplete_hold_streaks
    FOR EACH ROW EXECUTE FUNCTION ci_incomplete_hold_streaks_forbid_sequence_decrease();

-- One row per poll, keyed by the CALLER's poll observation id.
--
-- This is what makes a poll idempotent across a restart. `reserve_poll` is
-- keyed by `poll_observation_id`, so a retry of the same logical poll reads
-- back the sequence it was already assigned instead of reserving a second one
-- -- otherwise a crash-loop between reserve and apply would burn the streak's
-- sequence space and, worse, make every earlier in-flight observation look
-- superseded.
--
-- `apply_outcome` records what the observation actually did, including the
-- outcomes that did nothing. `superseded_observation` is the important one:
-- an ordering contract whose losers leave no trace cannot be audited, and
-- "the late poll did not count" is precisely the claim a reader needs to
-- check.
CREATE TABLE ci_incomplete_hold_observations (
    poll_observation_id  VARCHAR(36)  NOT NULL PRIMARY KEY,
    streak_id            VARCHAR(36)  NOT NULL
        REFERENCES ci_incomplete_hold_streaks(id) ON DELETE CASCADE,
    poll_sequence        BIGINT       NOT NULL,
    reserved_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    applied_at           TIMESTAMPTZ  NULL,
    apply_outcome        VARCHAR(32)  NULL,

    -- One observation per reserved sequence, per streak. The reservation
    -- transaction holds the streak row lock, so this can only be violated by a
    -- writer that bypassed `reserve_poll`.
    CONSTRAINT ci_incomplete_hold_observations_sequence_uniq
        UNIQUE (streak_id, poll_sequence),
    -- Sequences are assigned from `next_poll_sequence + 1`, so zero is never a
    -- reserved sequence -- which is what lets `last_applied_poll_sequence = 0`
    -- mean "nothing applied yet" without ambiguity.
    CONSTRAINT ci_incomplete_hold_observations_sequence_check
        CHECK (poll_sequence > 0),
    CONSTRAINT ci_incomplete_hold_observations_outcome_check
        CHECK (apply_outcome IS NULL OR apply_outcome IN (
            'applied_complete',
            'applied_incomplete',
            'escalated',
            'superseded_observation',
            'identity_advanced'
        )),
    -- Applied and unapplied are one fact stated twice.
    CONSTRAINT ci_incomplete_hold_observations_applied_pairing_check
        CHECK ((applied_at IS NULL) = (apply_outcome IS NULL))
);

CREATE INDEX ci_incomplete_hold_observations_streak_idx
    ON ci_incomplete_hold_observations(streak_id, poll_sequence);

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
