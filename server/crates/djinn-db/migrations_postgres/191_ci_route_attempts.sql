-- Migration 191: durable CI evidence-route attempts, monotonic retry budgets,
-- calling-recovery audit, and coordinator provider-action drain proof.
--
-- Proposal `nafu` wave 1 ("Route CI failures through bounded retries and
-- evidence-led remedies"). This migration is deliberately ADDITIVE ONLY: it
-- creates three new relations and adds two nullable columns to
-- `coordinator_incarnations`. No existing row is rewritten and no existing
-- column changes shape, so a binary that predates the feature keeps running
-- against this schema with the tables empty (the "migration only, old
-- binary" row of the proposal's mixed-version matrix).
--
-- The three invariants this schema exists to enforce, in the database rather
-- than in a coordinator that can crash between statements:
--
--  1. AT MOST ONE PROVIDER-CALL EPISODE PER EVIDENCE IDENTITY.
--     `provider_action_key` is a caller-computed hash of the immutable
--     evidence identity (lane + PR number + PR-head SHA + run id + run head
--     SHA + dequeue identity) plus the action. Duplicate polls and restarts
--     collide on it. It is scoped by the route SUBJECT rather than global —
--     see below, that scoping is load bearing.
--
--  2. MONOTONIC RETRY BUDGETS. `ci_route_budget_counters.charged_count` has a
--     CHECK that keeps it non-negative and the repository never issues a
--     decrement: once an attempt reaches `calling` its signature and head
--     slots are spent regardless of what the provider answered or whether the
--     process survived to hear it. There is deliberately no refund path and no
--     reversible accounting column.
--
--  3. AT MOST ONE TIER-2 (Lead) ADJUDICATION PER CURRENT EVIDENCE.
--     A PARTIAL UNIQUE INDEX on the subject-scoped lease key, restricted to
--     `open` leases, makes concurrent openers mutually exclusive; resolving a
--     lease releases the key for genuinely newer evidence. A given ROW may
--     still only ever route to Tier 2 once — that is enforced in the
--     repository, which refuses to re-lease a row whose lease already
--     resolved.
--
-- The `reserved` -> `calling` -> terminal phase column carries the fourth
-- invariant, the one the whole pre-call recovery contract rests on: a
-- `reserved` row is repository-local proof that NO process ever acquired call
-- ownership, therefore no provider mutation happened. Recovery of such a row
-- resumes it, supersedes it, or exhausts it -- there is no abandonment state,
-- which is why no `abandoned` value appears in the terminal vocabulary below.
--
-- ---------------------------------------------------------------------------
-- Why the route subject is polymorphic
-- ---------------------------------------------------------------------------
--
-- Every key on this table is caller-computed, so two routes could in principle
-- collide. The consequence of a collision is NOT a merely-shared budget: with
-- a globally unique `provider_action_key` a colliding reservation returns
-- "already present" for FOREIGN evidence and that route then silently never
-- exists. The same shape applies to the Tier-2 lease: a colliding lease key
-- reports "already leased" and the Lead adjudication never opens. Scoping both
-- by the owning subject makes that class structurally impossible rather than
-- merely unlikely.
--
-- The subject is `(subject_kind, subject_id)` rather than a bare `task_id`
-- because a CI evidence identity has no task in it. A PR opened from a
-- proposal branch has no owning task at all, and its remedy is new work rather
-- than one reopen; Tier 1 applies to it unchanged and only the Tier-2 subject
-- differs. Modelling that now costs two columns. Retrofitting it later means
-- dropping NOT NULL, dropping a foreign key, backfilling, and rebuilding both
-- the primary key and the lease index on a table carrying live routing state.
--
-- Referential integrity is kept for the task case WITHOUT denormalization:
-- `task_id` is `GENERATED ALWAYS ... STORED` from the subject and carries the
-- real foreign key. PostgreSQL enforces the key on the generated value and
-- cascades a task delete through it, and because the column is GENERATED
-- ALWAYS the repository physically cannot write a value that disagrees with
-- `subject_id` (verified on PostgreSQL 16). The honest limit: a non-task
-- subject generates NULL, so it has NO database-enforced referential
-- integrity. Whichever wave first writes a non-task subject owns proving that
-- its subject exists.

CREATE TABLE ci_route_attempts (
    -- ---- subject ----------------------------------------------------------
    -- The thing this route is remediating. `task` today; the vocabulary
    -- reserves `proposal` so a proposal-branch PR route needs no DDL, but
    -- wave 1 has NO writer for it and nothing branches on it.
    subject_kind            VARCHAR(32)  NOT NULL,
    subject_id              VARCHAR(36)  NOT NULL,

    -- ---- identity ---------------------------------------------------------
    -- Caller-computed hash over the immutable evidence identity plus action.
    -- Unique WITHIN a subject: the unique constraint is the idempotency
    -- mechanism, and scoping it is what stops one subject's route from
    -- swallowing another's.
    provider_action_key     VARCHAR(128) NOT NULL,

    -- Immutable evidence identity, stored in expanded form so a recovery
    -- sweep can compare it against a freshly observed identity without having
    -- to recompute the caller's hash function.
    lane                    VARCHAR(16)  NOT NULL,
    pr_number               BIGINT       NOT NULL,
    pr_head_sha             VARCHAR(64)  NOT NULL,
    run_id                  BIGINT       NOT NULL,
    run_head_sha            VARCHAR(64)  NOT NULL,
    dequeue_id              VARCHAR(128) NULL,

    -- Real referential integrity for the task subject, derived rather than
    -- duplicated. See the header note.
    task_id                 VARCHAR(36)
        GENERATED ALWAYS AS (CASE WHEN subject_kind = 'task' THEN subject_id END) STORED
        REFERENCES tasks(id) ON DELETE CASCADE,

    -- The board state the route was opened from, and the state a Lead-driven
    -- `PrCiFailed` must transition out of.
    origin_state            VARCHAR(32)  NOT NULL,

    -- ---- classification ---------------------------------------------------
    class                   VARCHAR(32)  NOT NULL,
    action                  VARCHAR(32)  NOT NULL,
    transient_fingerprint   VARCHAR(128) NOT NULL,

    -- Aggregate budget identity. Deliberately a SEPARATE column from
    -- `provider_action_key`: the budget key excludes run and dequeue ids so
    -- equivalent later evidence shares one budget, while the action key
    -- includes them so a distinct later run gets its own call episode.
    retry_budget_key        VARCHAR(128) NOT NULL,
    head_budget_key         VARCHAR(128) NOT NULL,

    -- ---- action phase -----------------------------------------------------
    -- 'reserved'  : row committed, provider mutation FORBIDDEN.
    -- 'calling'   : exactly one winner owns the provider-call episode.
    -- 'terminal'  : `terminal_outcome` is set and is never rewritten.
    action_phase            VARCHAR(16)  NOT NULL DEFAULT 'reserved',
    terminal_outcome        VARCHAR(32)  NULL,

    reserved_at             TIMESTAMPTZ  NOT NULL DEFAULT now(),
    calling_at              TIMESTAMPTZ  NULL,
    terminalized_at         TIMESTAMPTZ  NULL,

    -- The coordinator incarnation that won the `reserved` -> `calling`
    -- compare-and-set. Only two writers may ever touch it: that CAS (from
    -- NULL) and the owner-handoff CAS in `recover_calling_owner`, which
    -- requires the exact former owner and writes an audit row.
    owner_incarnation_id    VARCHAR(36)  NULL,

    -- How many pre-call recoveries ran against this row. Purely diagnostic:
    -- the charge is governed by the phase CAS, not by this counter, so N
    -- recoveries still produce exactly one charge.
    pre_call_resumptions    BIGINT       NOT NULL DEFAULT 0,

    -- Charged slot counts observed at the moment of the charge. Written once,
    -- with the phase CAS.
    charged_signature_count BIGINT       NULL,
    charged_head_count      BIGINT       NULL,
    -- Set when a still-current `reserved` row could no longer be charged.
    retry_exhausted_at      TIMESTAMPTZ  NULL,

    -- ---- Tier 2 ----------------------------------------------------------
    -- `tier2_lease_key` is the current-evidence scope. It carries the PR
    -- number and PR-head SHA and NOT the lane, the run id, or the dequeue id:
    -- at most one Lead adjudication may be open per PR head ACROSS BOTH LANES,
    -- which is the "head-level hold" the proposal's retry-storm safeguard
    -- names. A lane-scoped key would permit two concurrent adjudications for
    -- one head and defeat it.
    tier2_lease_id          VARCHAR(36)  NULL,
    tier2_lease_key         VARCHAR(128) NULL,
    tier2_lease_state       VARCHAR(16)  NULL,
    tier2_lease_reason      VARCHAR(32)  NULL,
    tier2_leased_at         TIMESTAMPTZ  NULL,
    tier2_resolved_at       TIMESTAMPTZ  NULL,
    tier2_resolution        VARCHAR(32)  NULL,
    lead_session_id         VARCHAR(36)  NULL,

    -- ---- Lead result detail ----------------------------------------------
    reopen_mode             VARCHAR(16)  NULL,
    diagnostic_reason       VARCHAR(32)  NULL,
    park_justification      TEXT         NULL,

    -- ---- provider outcome / supersession evidence ------------------------
    provider_error          JSONB        NULL,
    -- The observed current identity (or newer passing/merged snapshot) that
    -- DEFEATED a compare-and-set. Recorded so a supersession is auditable
    -- without ever making the obsolete evidence authoritative.
    superseded_by_evidence  JSONB        NULL,

    created_at              TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ  NOT NULL DEFAULT now(),

    CONSTRAINT ci_route_attempts_pkey
        PRIMARY KEY (subject_kind, subject_id, provider_action_key),

    CONSTRAINT ci_route_attempts_subject_kind_check
        CHECK (subject_kind IN ('task', 'proposal')),
    CONSTRAINT ci_route_attempts_lane_check
        CHECK (lane IN ('pr_head', 'merge_group')),
    CONSTRAINT ci_route_attempts_origin_state_check
        CHECK (origin_state IN ('pr_draft', 'pr_review')),
    CONSTRAINT ci_route_attempts_class_check
        CHECK (class IN ('inconclusive', 'causal_failure', 'unknown')),
    CONSTRAINT ci_route_attempts_action_check
        CHECK (action IN ('rerun_run', 'reenqueue', 'ask_lead', 'hold', 'discard')),
    CONSTRAINT ci_route_attempts_phase_check
        CHECK (action_phase IN ('reserved', 'calling', 'terminal')),

    -- The closed terminal vocabulary. Note what is NOT here: there is no
    -- `abandoned` outcome, because a row proven not to have called the
    -- provider is resumed, superseded, or exhausted -- never dropped.
    CONSTRAINT ci_route_attempts_terminal_outcome_check
        CHECK (terminal_outcome IS NULL OR terminal_outcome IN (
            'superseded_pre_call',
            'retriggered',
            'reenqueued',
            'superseded_after_call',
            'outcome_unknown',
            'action_failed',
            'held',
            'superseded_before_lead',
            'superseded_before_apply',
            'repair_reopened',
            'diagnostic_reopened',
            'parked',
            'superseded',
            'passed',
            'merged'
        )),
    -- A terminal phase and a terminal outcome are the same fact stated twice;
    -- they may never disagree.
    CONSTRAINT ci_route_attempts_terminal_agreement_check
        CHECK ((action_phase = 'terminal') = (terminal_outcome IS NOT NULL)),
    -- `calling` and every phase after it must name the owner that got there.
    CONSTRAINT ci_route_attempts_calling_owner_check
        CHECK (action_phase <> 'calling' OR (owner_incarnation_id IS NOT NULL AND calling_at IS NOT NULL)),
    CONSTRAINT ci_route_attempts_charged_pairing_check
        CHECK ((charged_signature_count IS NULL) = (charged_head_count IS NULL)),
    CONSTRAINT ci_route_attempts_charged_nonneg_check
        CHECK (charged_signature_count IS NULL OR charged_signature_count > 0),
    CONSTRAINT ci_route_attempts_head_charge_nonneg_check
        CHECK (charged_head_count IS NULL OR charged_head_count > 0),
    CONSTRAINT ci_route_attempts_resumptions_check
        CHECK (pre_call_resumptions >= 0),
    CONSTRAINT ci_route_attempts_tier2_state_check
        CHECK (tier2_lease_state IS NULL OR tier2_lease_state IN ('open', 'resolved')),
    CONSTRAINT ci_route_attempts_tier2_pairing_check
        CHECK ((tier2_lease_state IS NULL)
               = (tier2_lease_id IS NULL AND tier2_lease_key IS NULL AND tier2_leased_at IS NULL)),
    CONSTRAINT ci_route_attempts_tier2_reason_check
        CHECK (tier2_lease_reason IS NULL OR tier2_lease_reason IN (
            'causal_failure',
            'evidence_unknown',
            'provider_action_failed',
            'outcome_unknown',
            'retry_exhausted'
        )),
    CONSTRAINT ci_route_attempts_reopen_mode_check
        CHECK (reopen_mode IS NULL OR reopen_mode IN ('repair', 'diagnose')),
    CONSTRAINT ci_route_attempts_diagnostic_reason_check
        CHECK (diagnostic_reason IS NULL OR diagnostic_reason IN (
            'evidence_incomplete',
            'provider_action_failed',
            'no_grounded_remedy',
            'no_repository_command'
        )),
    -- A diagnostic reason belongs only to a diagnose-mode reopen.
    CONSTRAINT ci_route_attempts_diagnostic_pairing_check
        CHECK (diagnostic_reason IS NULL OR reopen_mode = 'diagnose')
);

-- The current-evidence Tier-2 hold, scoped to the subject for the same reason
-- the primary key is. PARTIAL so that only OPEN leases are mutually exclusive:
-- once Lead's result has been applied the key is free for a genuinely newer
-- evidence identity on a different row.
CREATE UNIQUE INDEX ci_route_attempts_open_tier2_lease_uniq
    ON ci_route_attempts(subject_kind, subject_id, tier2_lease_key)
    WHERE tier2_lease_state = 'open';

-- Recovery sweeps scan by phase; reporting scans by subject and lane identity.
CREATE INDEX ci_route_attempts_phase_idx
    ON ci_route_attempts(action_phase)
    WHERE action_phase <> 'terminal';
CREATE INDEX ci_route_attempts_lane_identity_idx
    ON ci_route_attempts(subject_kind, subject_id, lane, pr_number, pr_head_sha);
CREATE INDEX ci_route_attempts_task_idx
    ON ci_route_attempts(task_id)
    WHERE task_id IS NOT NULL;
CREATE INDEX ci_route_attempts_owner_idx
    ON ci_route_attempts(owner_incarnation_id)
    WHERE owner_incarnation_id IS NOT NULL;

-- Monotonic charge ledger. Two scopes share one relation because they share
-- one rule: increment only.
--
--   scope='signature' -> key = lane + PR + PR-head SHA + transient fingerprint
--   scope='head'      -> key = PR + PR-head SHA, shared ACROSS both lanes
--
-- The ceilings (2 and 4) live in the repository, not in a CHECK, because
-- "budget exhausted" is a routing decision that sends the evidence to Lead --
-- it is not a corrupt row. A CHECK here would turn a normal, expected
-- exhaustion into a transaction abort.
CREATE TABLE ci_route_budget_counters (
    subject_kind     VARCHAR(32)  NOT NULL,
    subject_id       VARCHAR(36)  NOT NULL,
    scope            VARCHAR(16)  NOT NULL,
    counter_key      VARCHAR(128) NOT NULL,
    charged_count    BIGINT       NOT NULL DEFAULT 0,
    first_charged_at TIMESTAMPTZ  NULL,
    last_charged_at  TIMESTAMPTZ  NULL,

    CONSTRAINT ci_route_budget_counters_pkey
        PRIMARY KEY (subject_kind, subject_id, scope, counter_key),
    CONSTRAINT ci_route_budget_counters_subject_kind_check
        CHECK (subject_kind IN ('task', 'proposal')),
    CONSTRAINT ci_route_budget_counters_scope_check
        CHECK (scope IN ('signature', 'head')),
    CONSTRAINT ci_route_budget_counters_nonneg_check
        CHECK (charged_count >= 0)
);

-- Append-only audit of every `calling`-row owner handoff ATTEMPT, including
-- the ones that legally did nothing. The deferrals are the point: a periodic
-- sweep that correctly left a live owner alone has to be observable, or
-- "we never steal a live owner's row" is an unfalsifiable claim.
CREATE TABLE ci_route_calling_recoveries (
    id                        VARCHAR(36)  NOT NULL,
    subject_kind              VARCHAR(32)  NOT NULL,
    subject_id                VARCHAR(36)  NOT NULL,
    provider_action_key       VARCHAR(128) NOT NULL,
    former_owner_incarnation  VARCHAR(36)  NULL,
    recovering_incarnation    VARCHAR(36)  NOT NULL,
    -- Attested by the caller: it holds the exclusive coordinator advisory
    -- lock for this exact incarnation.
    holds_exclusive_lock      BOOLEAN      NOT NULL,
    -- How the former owner's provider futures were proven gone. Elapsed time
    -- is NOT one of the options, by design.
    quiescence_proof          VARCHAR(32)  NOT NULL,
    recovery_reason           VARCHAR(32)  NOT NULL,
    calling_recovery_timeout_secs BIGINT   NOT NULL,
    observed_calling_at       TIMESTAMPTZ  NULL,
    -- Whether the compare-and-set actually took the row.
    cas_won                   BOOLEAN      NOT NULL,
    resulting_outcome         VARCHAR(32)  NULL,
    recorded_at               TIMESTAMPTZ  NOT NULL DEFAULT now(),

    CONSTRAINT ci_route_calling_recoveries_pkey PRIMARY KEY (id),
    CONSTRAINT ci_route_calling_recoveries_attempt_fkey
        FOREIGN KEY (subject_kind, subject_id, provider_action_key)
        REFERENCES ci_route_attempts(subject_kind, subject_id, provider_action_key)
        ON DELETE CASCADE,
    CONSTRAINT ci_route_calling_recoveries_quiescence_check
        CHECK (quiescence_proof IN ('graceful_drain', 'process_terminated', 'none')),
    CONSTRAINT ci_route_calling_recoveries_reason_check
        CHECK (recovery_reason IN (
            'startup_owner_handoff',
            'live_owner_deferred',
            'not_calling',
            'owner_mismatch',
            'timeout_not_elapsed',
            'lock_not_held',
            'cas_lost'
        )),
    CONSTRAINT ci_route_calling_recoveries_timeout_check
        CHECK (calling_recovery_timeout_secs > 0)
);

CREATE INDEX ci_route_calling_recoveries_attempt_idx
    ON ci_route_calling_recoveries(subject_kind, subject_id, provider_action_key);

-- Provider-action drain proof on the coordinator incarnation lease.
--
-- These are what turn the advisory lock from "someone holds it" into an
-- EXCLUSION PROOF for provider-action futures. `draining_at` is stamped when
-- leadership cancellation closes action admission; `provider_actions_drained_at`
-- is stamped ONLY after the registered provider-action scope is empty, and
-- only then may the lock session be released. Both are nullable and additive:
-- an incarnation registered by an old binary simply has neither, and the
-- calling-recovery predicate treats a missing drain proof as "not proven",
-- never as "drained".
--
-- Anything that reads `coordinator_incarnations` through
-- `CoordinatorIncarnationRepository::get` must project these two columns --
-- including the test-support fixtures that replace the table with a view.
ALTER TABLE coordinator_incarnations ADD COLUMN draining_at VARCHAR(64) NULL;
ALTER TABLE coordinator_incarnations ADD COLUMN provider_actions_drained_at VARCHAR(64) NULL;
