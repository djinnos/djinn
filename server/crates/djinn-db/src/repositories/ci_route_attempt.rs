//! Durable CI evidence-route attempts (proposal `nafu`, wave 1).
//!
//! This is the whole persistence layer for the CI routing contract: the
//! reservation that proves no provider call happened, the single-winner
//! compare-and-set that grants call ownership, the monotonic retry budgets,
//! the owner-scoped finalization, the startup-only owner handoff, and the
//! mutually exclusive Tier-2 (Lead) lease. The coordinator that *uses* it
//! lands in waves 2 and 3; nothing here calls a provider, dispatches a
//! worker, or touches the board.
//!
//! # Why a `reserved` phase exists at all
//!
//! Everything hard about this contract reduces to one question a crashed
//! coordinator cannot answer from memory: *did I already call GitHub?* The
//! answer has to be readable from the repository alone, because there is
//! deliberately no provider read/correlation API in scope — asking the
//! provider "did you get my request?" is exactly the subsystem the proposal
//! refuses to build.
//!
//! So the row is committed **before** the call, in phase `reserved`, and
//! provider mutation is forbidden while it is in that phase. A `reserved`
//! row found after a restart is therefore *proof* that no call happened.
//! That proof is what makes recovery cheap: the row is resumed, not
//! abandoned and not escalated. [`CiRouteAttemptRepository::recover_reserved`]
//! has exactly three outcomes and no fourth:
//!
//! | Situation | Outcome | Cost |
//! | --- | --- | --- |
//! | Evidence identity is obsolete | terminal `superseded_pre_call` | nothing: no charge, no lease, no Lead, no worker |
//! | Identity current, budget remains | resume the **same row** through one CAS | exactly one charge and one provider-call episode, however many recoveries run |
//! | Identity current, budget exhausted | route through retry exhaustion | no charge, at most one Tier-2 lease |
//!
//! The middle row is the one worth stating twice: N concurrent recoveries and
//! N restarts converge on **one** winner and **one** charge. Two mechanisms
//! do that, and it is worth being precise about which does what, because they
//! are easy to confuse:
//!
//! * the **row lock** (`SELECT ... FOR UPDATE` in [`fetch_locked`]) serializes
//!   the recoveries so the losers observe a non-`reserved` phase and return
//!   without charging; and
//! * the **counter lock plus the phase CAS** is what makes a double charge
//!   impossible even if the row lock were absent — a loser that got as far as
//!   incrementing would find `action_phase = 'reserved'` no longer true, fail
//!   the CAS, and roll its increment back with the transaction.
//!
//! `pre_call_resumptions` records how many recoveries ran; it has no influence
//! on the charge, which is the point of recording it.
//!
//! # Why `calling` recovery is so much more restricted
//!
//! A `calling` row proves the opposite: a call episode was authorized, and
//! its result may be in flight at the provider right now. Taking that row
//! from a *live* owner would allow two episodes for one evidence identity.
//! Elapsed wall-clock time cannot distinguish "the owner died" from "the
//! owner is slow", so this module never infers death from time alone. See
//! [`CiRouteAttemptRepository::recover_calling_owner`]: it demands attested
//! exclusive-advisory-lock ownership, an explicit quiescence proof (graceful
//! drain recorded on the former incarnation, or process termination), an
//! owner mismatch, and the 300-second floor — and even then it can lose its
//! compare-and-set to the former owner's own finalizer, which is legal and
//! recorded.
//!
//! That restriction is worth stating as one invariant, because it was
//! previously enforced in some paths and not others:
//!
//! > **A row in `calling` is closed only by its own owner's
//! > [`CiRouteAttemptRepository::finalize_calling`], or by
//! > [`CiRouteAttemptRepository::recover_calling_owner`] under a quiescent
//! > startup handoff. Nothing else may terminalize it.**
//!
//! It is enforced structurally, in [`terminalize_cas_in_tx`] — the single
//! statement every non-owner terminalization funnels through — rather than by
//! each call site remembering. The reason is what a miss costs: closing a live
//! `calling` row makes the owner's own compare-and-set fail, so a real
//! provider result is discarded with nothing reported. The public methods that
//! can be handed such a row ([`CiRouteAttemptRepository::terminalize`],
//! [`CiRouteAttemptRepository::open_tier2_lease`],
//! [`CiRouteAttemptRepository::resolve_tier2_lease`]) additionally check the
//! phase *before* writing, so the outcome each reports stays truthful instead
//! of announcing a supersession the fence silently refused.
//!
//! Every recovery *attempt* writes a row to `ci_route_calling_recoveries`,
//! including the ones that correctly did nothing. A sweep that left a live
//! owner alone has to be observable, or the claim "we never steal a live
//! owner's row" cannot be falsified by a test.
//!
//! # Two write-once outcome fields, not one
//!
//! [`CiRouteAttempt::terminal_outcome`] is the route's own single terminal
//! fact and is never rewritten. A row that terminalized on its provider
//! result (`action_failed`, `outcome_unknown`) may still legally route to Tier
//! 2 **once**; that adjudication lands in
//! [`CiRouteAttempt::tier2_resolution`] instead, because terminalization
//! already happened and is write-once.
//!
//! **Downstream obligation (W2/W4/W5):** any query that counts reopens, parks,
//! or supersessions must union *both* columns. Reading only `terminal_outcome`
//! silently drops every reopen that followed a provider failure — which is
//! precisely the population the evidence-led route exists to serve.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

/// Charged Tier-1 actions allowed per retry-budget key (lane + PR + PR-head
/// SHA + transient fingerprint).
pub const CI_SIGNATURE_BUDGET_LIMIT: i64 = 2;

/// Charged Tier-1 actions allowed per PR head, shared **across both lanes**.
pub const CI_HEAD_BUDGET_LIMIT: i64 = 4;

/// Default age a `reserved` row must reach before a recovery sweep may touch
/// it.
///
/// Unlike [`CI_CALLING_RECOVERY_TIMEOUT_SECS`] the proposal does not fix this
/// number — it says only "the configured reservation timeout". It is a
/// liveness/latency knob rather than a safety one: the safety comes from the
/// compare-and-set, and a too-short value costs at worst a redundant recovery
/// that loses its CAS. Callers may pass their own.
pub const CI_RESERVATION_RECOVERY_TIMEOUT_SECS: i64 = 60;

/// Minimum age of `calling_at` before a startup owner handoff is even
/// considered. Fixed by the proposal.
pub const CI_CALLING_RECOVERY_TIMEOUT_SECS: i64 = 300;

const ROW_COLUMNS: &str = "subject_kind, subject_id, provider_action_key, lane, pr_number, pr_head_sha, run_id, run_head_sha, dequeue_id, \
    task_id, origin_state, class, action, transient_fingerprint, retry_budget_key, head_budget_key, \
    action_phase, terminal_outcome, \
    to_char(reserved_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS reserved_at, \
    to_char(calling_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS calling_at, \
    to_char(terminalized_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS terminalized_at, \
    owner_incarnation_id, pre_call_resumptions, charged_signature_count, charged_head_count, \
    to_char(retry_exhausted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS retry_exhausted_at, \
    tier2_lease_id, tier2_lease_key, tier2_lease_state, tier2_lease_reason, \
    to_char(tier2_leased_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS tier2_leased_at, \
    tier2_resolution, lead_session_id, lead_session_count, reopen_mode, diagnostic_reason, park_justification, \
    lead_rejection, \
    to_char(pr_merged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS pr_merged_at, \
    provider_error::text AS provider_error, superseded_by_evidence::text AS superseded_by_evidence";

/// `WHERE` fragment naming one row by its full subject-scoped primary key.
const BY_PK: &str = "subject_kind = $1 AND subject_id = $2 AND provider_action_key = $3";

// ---------------------------------------------------------------------------
// Vocabularies
// ---------------------------------------------------------------------------

/// A trivial two-way string mapping.
///
/// Every durable vocabulary below is spelled exactly once in Rust and once in
/// migration 193's `CHECK ... IN (...)`. Keeping the two in one place per enum
/// is what stops a new variant from binding as an unchecked string that the
/// CHECK then rejects at 3am instead of at compile time.
macro_rules! durable_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The durable spelling.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Parse a durable spelling, rejecting anything outside the
            /// migration's CHECK vocabulary.
            ///
            /// # Errors
            ///
            /// [`DbError::InvalidData`] for an unknown spelling.
            pub fn parse(value: &str) -> Result<Self> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    other => Err(DbError::InvalidData(format!(
                        concat!("invalid ", stringify!($name), " `{}`"), other
                    ))),
                }
            }
        }
    };
}

durable_enum! {
    /// What a route is remediating.
    ///
    /// A CI evidence identity contains no subject of its own, so the subject
    /// is carried alongside it. Every key on the table is scoped by it.
    CiSubjectKind {
        /// A board task. The only kind wave 1 writes, and the only kind with
        /// database-enforced referential integrity (migration 193 derives
        /// `task_id` from the subject and puts the foreign key on it).
        Task => "task",
        /// Reserved for a proposal-branch PR, which has no owning task and
        /// whose remedy is new work rather than one reopen. Wave 1 has **no
        /// writer** for this and nothing branches on it; the value exists so
        /// that route needs no DDL on a table carrying live routing state.
        Proposal => "proposal",
    }
}

durable_enum! {
    /// Which CI surface produced the evidence.
    CiLane {
        PrHead => "pr_head",
        MergeGroup => "merge_group",
    }
}

durable_enum! {
    /// The deterministic classification of a complete terminal evidence set.
    CiClass {
        Inconclusive => "inconclusive",
        CausalFailure => "causal_failure",
        Unknown => "unknown",
    }
}

durable_enum! {
    /// The route decision recorded for the attempt.
    CiAction {
        RerunRun => "rerun_run",
        Reenqueue => "reenqueue",
        AskLead => "ask_lead",
        Hold => "hold",
        Discard => "discard",
    }
}

impl CiAction {
    /// Whether this action can ever consume a retry-budget slot.
    ///
    /// Exactly the two provider mutations. `charge_and_begin_calling` is the
    /// only writer of either counter and only a route carrying a provider
    /// authorization reaches it, so `ask_lead`, `hold`, and `discard` are
    /// structurally uncharged — which is why [`Self::reserve`]'s ceiling
    /// refusal is scoped to this predicate.
    ///
    /// [`Self::reserve`]: CiRouteAttemptRepository::reserve
    #[must_use]
    pub fn charges_budget(self) -> bool {
        matches!(self, Self::RerunRun | Self::Reenqueue)
    }
}

durable_enum! {
    /// The board state the route was opened from.
    CiOriginState {
        PrDraft => "pr_draft",
        PrReview => "pr_review",
    }
}

durable_enum! {
    /// Non-terminal action phases plus the terminal marker.
    ///
    /// `Reserved` and `Calling` are the two non-terminal phases; `Terminal`
    /// always accompanies a [`CiRouteOutcome`].
    CiActionPhase {
        Reserved => "reserved",
        Calling => "calling",
        Terminal => "terminal",
    }
}

durable_enum! {
    /// The closed terminal vocabulary.
    ///
    /// There is deliberately no `abandoned`: a row proven not to have called
    /// the provider is resumed, superseded, or exhausted.
    CiRouteOutcome {
        SupersededPreCall => "superseded_pre_call",
        Retriggered => "retriggered",
        Reenqueued => "reenqueued",
        SupersededAfterCall => "superseded_after_call",
        OutcomeUnknown => "outcome_unknown",
        ActionFailed => "action_failed",
        Held => "held",
        SupersededBeforeLead => "superseded_before_lead",
        SupersededBeforeApply => "superseded_before_apply",
        RepairReopened => "repair_reopened",
        DiagnosticReopened => "diagnostic_reopened",
        Parked => "parked",
        Superseded => "superseded",
        Passed => "passed",
        Merged => "merged",
    }
}

impl CiRouteOutcome {
    /// The three outcomes that assert something about a provider call.
    ///
    /// They are reachable only through
    /// [`CiRouteAttemptRepository::finalize_calling`], which is fenced to the
    /// exact `calling` owner. Nothing else may claim the provider was called.
    #[must_use]
    pub fn is_provider_finalization(self) -> bool {
        matches!(
            self,
            Self::Retriggered | Self::Reenqueued | Self::ActionFailed
        )
    }
}

durable_enum! {
    /// Why a Tier-2 lease was opened.
    CiTier2Reason {
        CausalFailure => "causal_failure",
        EvidenceUnknown => "evidence_unknown",
        ProviderActionFailed => "provider_action_failed",
        OutcomeUnknown => "outcome_unknown",
        RetryExhausted => "retry_exhausted",
    }
}

durable_enum! {
    /// The two mutually exclusive validated `reopen` payload modes.
    CiReopenMode {
        Repair => "repair",
        Diagnose => "diagnose",
    }
}

durable_enum! {
    /// The closed diagnostic-reason set a `diagnose` reopen must cite.
    CiDiagnosticReason {
        EvidenceIncomplete => "evidence_incomplete",
        ProviderActionFailed => "provider_action_failed",
        NoGroundedRemedy => "no_grounded_remedy",
        NoRepositoryCommand => "no_repository_command",
    }
}

durable_enum! {
    /// Why a delivered Lead result was replaced by the single diagnostic
    /// fallback (proposal `nafu`, wave 5).
    ///
    /// # Why this is durable and not a log line
    ///
    /// The supervisor derives a fallback's [`CiDiagnosticReason`] from the
    /// *route's* Tier-2 reason, so a Lead that timed out on a `causal_failure`
    /// route is recorded with `no_grounded_remedy` — the identical spelling a
    /// Lead that answered and genuinely found no remedy produces. Reporting
    /// therefore conflates "Lead diagnosed" with "Lead never answered" unless
    /// the rejection itself is stored. `None` means the delivered result was
    /// accepted as submitted.
    ///
    /// This is the same closed set the supervisor validates against; it lives
    /// here so the vocabulary is spelled once in Rust and once in migration
    /// 193's CHECK, like every other durable enum in this module.
    CiLeadRejection {
        /// `approve` / `approve_conflict` on non-passing CI evidence.
        ApprovedNonPassingCi => "approved_non_passing_ci",
        /// A reopen naming both plans, or neither.
        ReopenPlanAmbiguous => "reopen_plan_ambiguous",
        /// A repair whose `verification_command` was not copied from
        /// repository/task context or CI evidence.
        VerificationCommandNotRepositoryValid => "verification_command_not_repository_valid",
        /// A reopen whose directive is empty or cites no evidence handle.
        DirectiveNotGrounded => "directive_not_grounded",
        /// A `diagnostic_reason` outside the closed set.
        DiagnosticReasonUnknown => "diagnostic_reason_unknown",
        /// A park on a route the proposal says must reopen.
        ParkUnavailableForRoute => "park_unavailable_for_route",
        /// A `repair` reopen on a **run-absent** route. With no run there is
        /// nothing to re-run and no run-scoped CI evidence to ground a
        /// verification command in, so the only honest adjudication left is a
        /// diagnose. See [`CiRouteAttempt::is_run_absent`].
        RepairUnavailableForRoute => "repair_unavailable_for_route",
        /// A park whose dossier is missing, incomplete, or cites no evidence.
        ParkNotCited => "park_not_cited",
        /// A supersede with no replacement tasks.
        SupersedeWithoutReplacements => "supersede_without_replacements",
        /// A decision string outside the five existing results.
        UnknownDecision => "unknown_decision",
        /// The session finalized through a tool that is not `submit_decision`.
        UnsupportedFinalizeTool => "unsupported_finalize_tool",
        /// The session produced no result at all.
        NoResult => "no_result",
        /// The session timed out. **The reason this enum exists**: without it
        /// this case is indistinguishable from a delivered `no_grounded_remedy`.
        TimedOut => "timed_out",
    }
}

impl CiLeadRejection {
    /// Whether Lead produced no adjudication at all, as opposed to producing
    /// one this contract refused.
    ///
    /// The distinction is the operational one: the first three are a Lead that
    /// never answered (prompt, deadline, or transport), the rest are a Lead
    /// that answered something the contract does not allow.
    #[must_use]
    pub fn is_absent_result(self) -> bool {
        matches!(
            self,
            Self::TimedOut | Self::NoResult | Self::UnsupportedFinalizeTool
        )
    }
}

durable_enum! {
    /// State of the current-evidence Tier-2 hold.
    CiTier2LeaseState {
        Open => "open",
        Resolved => "resolved",
    }
}

durable_enum! {
    /// How a former `calling` owner's provider futures were proven gone.
    ///
    /// One proof and one absence, deliberately. Note what is *not* here:
    /// elapsed time, an expired owner lease, and process death.
    ///
    /// The first two are the same mistake — the 300-second floor is a
    /// *necessary* condition layered on top of the proof, never a substitute
    /// for one. The third looks different and is not, because it has no
    /// admissible witness. The only trace an abrupt death could leave is the
    /// automatic release of the session-scoped coordinator advisory lock, and
    /// Postgres performs that release at backend termination — strictly before
    /// the client process can observe anything. A live process whose lock
    /// connection was dropped by a failover or a restart is therefore
    /// indistinguishable from a dead one until it notices and exits, with its
    /// provider futures running throughout, and closing that interval needs
    /// exactly the elapsed-time inference the proposal forbids. See migration
    /// 196.
    CiQuiescenceProof {
        /// Leadership cancellation closed action admission, joined every
        /// leader-owned provider-action future, and recorded
        /// `provider_actions_drained_at` before releasing the advisory lock.
        ///
        /// The only value that authorises a handoff — and it is re-read from
        /// the former incarnation's own row rather than believed, so a caller
        /// cannot assert it into existence.
        GracefulDrain => "graceful_drain",
        /// No proof. Recorded on deferrals so the deferral is auditable.
        None => "none",
    }
}

durable_enum! {
    /// Why a `calling` recovery attempt did or did not take the row.
    CiCallingRecoveryReason {
        StartupOwnerHandoff => "startup_owner_handoff",
        LiveOwnerDeferred => "live_owner_deferred",
        NotCalling => "not_calling",
        OwnerMismatch => "owner_mismatch",
        TimeoutNotElapsed => "timeout_not_elapsed",
        LockNotHeld => "lock_not_held",
        CasLost => "cas_lost",
    }
}

// ---------------------------------------------------------------------------
// Subject and identity
// ---------------------------------------------------------------------------

/// The thing a route is remediating, and the scope of every key on the table.
///
/// Scoping matters more than it looks. `provider_action_key` and
/// `tier2_lease_key` are caller-computed hashes; if they were globally unique
/// a collision would not merely share a budget, it would make
/// [`CiRouteAttemptRepository::reserve`] answer "already present" for *foreign*
/// evidence, so the colliding route would silently never exist. Scoping by
/// subject makes that class impossible rather than unlikely.
///
/// The cost, stated plainly because it is the flip side of that fix: every
/// per-key limit is **per subject**. The Tier-2 head hold, the signature and
/// head retry budgets, and the pass/merge close all stop at the subject
/// boundary. With one task per PR that boundary never splits a PR head, so
/// nothing observes it. The first non-task subject to share a PR head with a
/// task subject gets a doubled head budget and a second concurrent Lead
/// adjudication for that head, and owns deciding what to do about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRouteSubject {
    pub kind: CiSubjectKind,
    pub id: String,
}

impl CiRouteSubject {
    /// The task subject — the only kind wave 1 writes.
    #[must_use]
    pub fn task(task_id: impl Into<String>) -> Self {
        Self {
            kind: CiSubjectKind::Task,
            id: task_id.into(),
        }
    }
}

/// The immutable evidence identity a route attempt is bound to.
///
/// Stored expanded on the row (not only as the caller's hash) so a recovery
/// sweep can compare it against a freshly observed identity without
/// recomputing the caller's hash function — and so a supersession is
/// explainable by reading the row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiEvidenceIdentity {
    pub lane: CiLane,
    pub pr_number: i64,
    pub pr_head_sha: String,
    /// The provider's run identifier, or `None` when the capture **never named
    /// a run**.
    ///
    /// `None` is an honest absence, not a placeholder. Wave 5 previously wrote
    /// `0` here for lane-level captures that failed closed before runs were
    /// attributed, which collapsed two distinct dequeues of one head onto one
    /// `provider_action_key` — so the second route silently reused the first
    /// route's row. Migration 195 makes the sentinel unrepresentable
    /// (`CHECK (run_id IS NULL OR run_id > 0)`) and
    /// [`CiRouteAttemptRepository::reserve`] refuses it before the CHECK fires
    /// so the message is readable.
    ///
    /// A run-absent identity is **diagnose-only**: see
    /// [`CiRouteAttempt::is_run_absent`].
    pub run_id: Option<i64>,
    pub run_head_sha: String,
    /// Merge-group dequeue event identity. `None` for the PR-head lane.
    pub dequeue_id: Option<String>,
}

impl CiEvidenceIdentity {
    /// Whether a freshly observed identity is the *same* evidence.
    ///
    /// Full equality on every field, deliberately. The lane table treats a
    /// changed PR head, a changed run, a changed merge group, and a changed
    /// dequeue event all as supersession, so there is no field here that may
    /// drift while the route stays authorized.
    #[must_use]
    pub fn matches(&self, observed: &Self) -> bool {
        self == observed
    }
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// One durable route attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiRouteAttempt {
    pub subject: CiRouteSubject,
    pub provider_action_key: String,
    pub identity: CiEvidenceIdentity,
    /// The generated, foreign-key-backed task id. `Some` exactly when the
    /// subject is a task; the database derives it and refuses a direct write.
    pub task_id: Option<String>,
    pub origin_state: CiOriginState,
    pub class: CiClass,
    pub action: CiAction,
    pub transient_fingerprint: String,
    pub retry_budget_key: String,
    pub head_budget_key: String,

    pub action_phase: CiActionPhase,
    pub terminal_outcome: Option<CiRouteOutcome>,
    pub reserved_at: String,
    pub calling_at: Option<String>,
    pub terminalized_at: Option<String>,

    pub owner_incarnation_id: Option<String>,
    pub pre_call_resumptions: i64,
    pub charged_signature_count: Option<i64>,
    pub charged_head_count: Option<i64>,
    pub retry_exhausted_at: Option<String>,

    pub tier2_lease_id: Option<String>,
    pub tier2_lease_key: Option<String>,
    pub tier2_lease_state: Option<CiTier2LeaseState>,
    pub tier2_lease_reason: Option<CiTier2Reason>,
    pub tier2_leased_at: Option<String>,
    pub tier2_resolution: Option<CiRouteOutcome>,
    /// The **first** Lead session bound to this route's lease, retained and
    /// never overwritten. It is the audit handle for the adjudication that
    /// opened; it is *not* a count of how many sessions ran.
    pub lead_session_id: Option<String>,
    /// How many Lead sessions were attached to this route, in total.
    ///
    /// This is the field cost metrics read. `lead_session_id` cannot serve
    /// that purpose: it is one slot, so two sessions on one route used to
    /// report as one and the proposal's "Lead sessions per merged PR" was
    /// structurally incapable of exceeding one per route.
    pub lead_session_count: i64,

    pub reopen_mode: Option<CiReopenMode>,
    pub diagnostic_reason: Option<CiDiagnosticReason>,
    pub park_justification: Option<String>,
    /// Why the delivered Lead result was replaced by the diagnostic fallback,
    /// or `None` when it was accepted as submitted.
    pub lead_rejection: Option<CiLeadRejection>,

    /// When this route's PR was first observed merged, or `None` if it never
    /// was.
    ///
    /// A fact about the **pull request**, deliberately kept out of the
    /// adjudication columns: `tier2_resolution` records how Lead decided one
    /// route's episode and is write-once for that reason, so a merge arriving
    /// after Lead resolved has nowhere to land in it. This is the field the
    /// per-merged-PR cost ratios count distinct PRs from.
    pub pr_merged_at: Option<String>,

    pub provider_error: Option<String>,
    pub superseded_by_evidence: Option<String>,
}

impl CiRouteAttempt {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        fn opt_enum<T>(value: Option<String>, f: fn(&str) -> Result<T>) -> Result<Option<T>> {
            match value {
                Some(v) => f(&v).map(Some),
                None => Ok(None),
            }
        }
        Ok(Self {
            subject: CiRouteSubject {
                kind: CiSubjectKind::parse(row.try_get::<String, _>("subject_kind")?.as_str())?,
                id: row.try_get("subject_id")?,
            },
            provider_action_key: row.try_get("provider_action_key")?,
            identity: CiEvidenceIdentity {
                lane: CiLane::parse(row.try_get::<String, _>("lane")?.as_str())?,
                pr_number: row.try_get("pr_number")?,
                pr_head_sha: row.try_get("pr_head_sha")?,
                run_id: row.try_get::<Option<i64>, _>("run_id")?,
                run_head_sha: row.try_get("run_head_sha")?,
                dequeue_id: row.try_get("dequeue_id")?,
            },
            task_id: row.try_get("task_id")?,
            origin_state: CiOriginState::parse(row.try_get::<String, _>("origin_state")?.as_str())?,
            class: CiClass::parse(row.try_get::<String, _>("class")?.as_str())?,
            action: CiAction::parse(row.try_get::<String, _>("action")?.as_str())?,
            transient_fingerprint: row.try_get("transient_fingerprint")?,
            retry_budget_key: row.try_get("retry_budget_key")?,
            head_budget_key: row.try_get("head_budget_key")?,
            action_phase: CiActionPhase::parse(row.try_get::<String, _>("action_phase")?.as_str())?,
            terminal_outcome: opt_enum(row.try_get("terminal_outcome")?, CiRouteOutcome::parse)?,
            reserved_at: row.try_get("reserved_at")?,
            calling_at: row.try_get("calling_at")?,
            terminalized_at: row.try_get("terminalized_at")?,
            owner_incarnation_id: row.try_get("owner_incarnation_id")?,
            pre_call_resumptions: row.try_get("pre_call_resumptions")?,
            charged_signature_count: row.try_get("charged_signature_count")?,
            charged_head_count: row.try_get("charged_head_count")?,
            retry_exhausted_at: row.try_get("retry_exhausted_at")?,
            tier2_lease_id: row.try_get("tier2_lease_id")?,
            tier2_lease_key: row.try_get("tier2_lease_key")?,
            tier2_lease_state: opt_enum(
                row.try_get("tier2_lease_state")?,
                CiTier2LeaseState::parse,
            )?,
            tier2_lease_reason: opt_enum(row.try_get("tier2_lease_reason")?, CiTier2Reason::parse)?,
            tier2_leased_at: row.try_get("tier2_leased_at")?,
            tier2_resolution: opt_enum(row.try_get("tier2_resolution")?, CiRouteOutcome::parse)?,
            lead_session_id: row.try_get("lead_session_id")?,
            lead_session_count: row.try_get("lead_session_count")?,
            reopen_mode: opt_enum(row.try_get("reopen_mode")?, CiReopenMode::parse)?,
            diagnostic_reason: opt_enum(
                row.try_get("diagnostic_reason")?,
                CiDiagnosticReason::parse,
            )?,
            park_justification: row.try_get("park_justification")?,
            lead_rejection: opt_enum(row.try_get("lead_rejection")?, CiLeadRejection::parse)?,
            pr_merged_at: row.try_get("pr_merged_at")?,
            provider_error: row.try_get("provider_error")?,
            superseded_by_evidence: row.try_get("superseded_by_evidence")?,
        })
    }

    /// Whether the row has reached its single terminal outcome.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.action_phase == CiActionPhase::Terminal
    }

    /// Whether this route's evidence identity **names no run**.
    ///
    /// True exactly when `identity.run_id` is `None`, which happens when the
    /// lane capture failed closed before runs were attributed
    /// (`CheckEnumerationUnavailable`, the four blocking-completeness timestamp
    /// reasons, `AmbiguousMergeGroupCorrelation`).
    ///
    /// **A run-absent route is diagnose-only.** With no run there is nothing to
    /// re-run and no run-scoped CI evidence for a repair's `verification_command`
    /// to be copied from, so a repair reopen on such a route would dispatch a
    /// worker against a plan nothing grounds.
    ///
    /// This predicate is the **database-side half** of that rule:
    /// [`CiRouteAttemptRepository::resolve_tier2_lease`] refuses a
    /// [`CiRouteOutcome::RepairReopened`] resolution on a row where this is
    /// true, which is the last fence before the adjudication becomes durable.
    /// The other half is the agent-side validator, which rejects the delivered
    /// result with [`CiLeadRejection::RepairUnavailableForRoute`] and can
    /// explain itself to Lead. Neither is redundant: the validator produces the
    /// good error message, this one holds even if a future caller reaches the
    /// repository without going through it.
    #[must_use]
    pub fn is_run_absent(&self) -> bool {
        self.identity.run_id.is_none()
    }

    /// Whether the row currently holds an **open** Tier-2 lease.
    #[must_use]
    pub fn holds_open_tier2_lease(&self) -> bool {
        self.tier2_lease_state == Some(CiTier2LeaseState::Open)
    }

    /// Whether this row has already used its one trip to Tier 2.
    ///
    /// True from the moment a lease is opened, and it stays true after that
    /// lease resolves. "Route **once** to Tier 2" is a once-ever property, not
    /// a concurrency property.
    #[must_use]
    pub fn has_routed_to_tier2(&self) -> bool {
        self.tier2_lease_state.is_some()
    }

    /// The Lead adjudication that closed this route, wherever it landed.
    ///
    /// See the module note on the two write-once outcome fields: a row that
    /// terminalized on its provider result keeps that outcome and records the
    /// Tier-2 result separately. Callers counting reopens or parks must use
    /// this rather than `terminal_outcome` alone.
    #[must_use]
    pub fn adjudicated_outcome(&self) -> Option<CiRouteOutcome> {
        self.tier2_resolution.or(self.terminal_outcome)
    }
}

// ---------------------------------------------------------------------------
// Inputs and outcomes
// ---------------------------------------------------------------------------

/// Everything needed to open a reservation.
#[derive(Clone, Debug)]
pub struct CiRouteReservation {
    pub subject: CiRouteSubject,
    /// Caller-computed hash of the immutable evidence identity plus action.
    /// Its uniqueness within the subject is what permits at most one
    /// provider-call episode.
    pub provider_action_key: String,
    pub identity: CiEvidenceIdentity,
    pub origin_state: CiOriginState,
    pub class: CiClass,
    pub action: CiAction,
    pub transient_fingerprint: String,
    /// lane + PR + PR-head SHA + transient fingerprint. Excludes run and
    /// dequeue ids so equivalent later evidence shares one budget.
    pub retry_budget_key: String,
    /// PR + PR-head SHA. Shared across both lanes.
    pub head_budget_key: String,
}

/// Charged slot counts for one budget pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiBudgetCounts {
    pub signature: i64,
    pub head: i64,
}

impl CiBudgetCounts {
    /// Whether either ceiling is already at or beyond its limit.
    #[must_use]
    pub fn is_exhausted(self) -> bool {
        self.signature >= CI_SIGNATURE_BUDGET_LIMIT || self.head >= CI_HEAD_BUDGET_LIMIT
    }
}

/// Result of [`CiRouteAttemptRepository::reserve`].
#[derive(Clone, Debug)]
pub enum CiReserveOutcome {
    /// A fresh reservation was committed. Provider mutation remains forbidden
    /// until the caller wins [`CiRouteAttemptRepository::charge_and_begin_calling`].
    Reserved(Box<CiRouteAttempt>),
    /// This exact evidence identity already has an attempt row *for this
    /// subject*. A duplicate poll or a restart landed here; the existing row
    /// is authoritative.
    ///
    /// The returned row is **not** necessarily under the key that was asked
    /// for. Two run-absent captures of one lane/PR/head/dequeue are one
    /// identity under `NULLS NOT DISTINCT`, so a second irrecoverable reason
    /// collapses onto the first reason's row and gets it back here — which is
    /// the intended reading: one identity earns one route and one
    /// adjudication, not one per reason.
    AlreadyPresent(Box<CiRouteAttempt>),
    /// The budgets were already spent, so no reservation was created and no
    /// Tier-1 action is possible for this evidence.
    BudgetExhausted(CiBudgetCounts),
}

/// Result of the `reserved` -> `calling` compare-and-set.
#[derive(Clone, Debug)]
pub enum CiChargeOutcome {
    /// This caller is the single winner and owns the provider-call episode.
    Charged {
        attempt: Box<CiRouteAttempt>,
        counts: CiBudgetCounts,
    },
    /// The observed identity no longer matches; the row is now terminal
    /// `superseded_pre_call` with no charge and no lease.
    SupersededPreCall(Box<CiRouteAttempt>),
    /// Still current, but the budgets were spent between the reservation and
    /// this call. No charge and no provider call.
    ///
    /// Note the asymmetry with [`CiReservedRecovery::RetryExhausted`]: this is
    /// the *live* path, so it only stamps `retry_exhausted_at` and leaves the
    /// row `reserved`. It does **not** open a Tier-2 lease, because the caller
    /// is still running and owns that decision. The recovery path opens the
    /// lease itself precisely because no caller is left to make it.
    BudgetExhausted {
        attempt: Box<CiRouteAttempt>,
        counts: CiBudgetCounts,
    },
    /// The row had already left `reserved` — another caller won, or it is
    /// already terminal.
    NotReserved(Box<CiRouteAttempt>),
}

/// Result of pre-call recovery. Exactly three productive outcomes, plus the
/// two "nothing to do" shapes.
#[derive(Clone, Debug)]
pub enum CiReservedRecovery {
    /// No row with that key for that subject.
    NotFound,
    /// The row is not a recoverable `reserved` row: either it has advanced
    /// past `reserved`, or its reservation timeout has not elapsed yet.
    NotEligible(Box<CiRouteAttempt>),
    /// Obsolete identity: terminal `superseded_pre_call`, uncharged.
    SupersededPreCall(Box<CiRouteAttempt>),
    /// Current identity with budget: the **same row** advanced to `calling`
    /// under one compare-and-set winner, charging exactly once.
    Resumed {
        attempt: Box<CiRouteAttempt>,
        counts: CiBudgetCounts,
    },
    /// Current identity, budget spent: routed through retry exhaustion.
    RetryExhausted {
        attempt: Box<CiRouteAttempt>,
        counts: CiBudgetCounts,
        /// `Some` when this call opened the lease.
        ///
        /// `None` means no lease could be opened — either this row already
        /// routed to Tier 2, or **another row holds the head lease**. The
        /// second case leaves this row `reserved` with no route out of its own
        /// accord, so W2 must re-drive `recover_reserved` after the holding
        /// lease resolves, or close the row explicitly. It is not
        /// self-healing.
        tier2_lease_id: Option<String>,
    },
}

/// Authority a caller must attest before a `calling` row may be taken.
///
/// Every field is a *proof obligation the database cannot discharge itself*.
/// The repository verifies what it can (owner mismatch, the 300s floor, the
/// former incarnation's recorded `provider_actions_drained_at`) and refuses
/// outright when the caller cannot attest the rest.
#[derive(Clone, Debug)]
pub struct CiCallingRecoveryAuthority {
    /// The recovering coordinator's new immutable incarnation.
    pub recovering_incarnation: String,
    /// The exact former owner this handoff is fenced to.
    pub former_owner_incarnation: String,
    /// The recovering coordinator holds the exclusive coordinator advisory
    /// lock for `recovering_incarnation`. A periodic sweep or a non-leader
    /// runtime passes `false` here and is refused.
    pub holds_exclusive_lock: bool,
    /// How the former owner's provider futures were proven gone.
    pub quiescence_proof: CiQuiescenceProof,
    /// Floor on `calling_at` age. Defaults to
    /// [`CI_CALLING_RECOVERY_TIMEOUT_SECS`].
    pub calling_recovery_timeout_secs: i64,
}

/// Result of a `calling` owner handoff attempt.
#[derive(Clone, Debug)]
pub enum CiCallingRecovery {
    NotFound,
    /// The attempt legally did nothing. An audit row was still written.
    Deferred {
        attempt: Box<CiRouteAttempt>,
        reason: CiCallingRecoveryReason,
    },
    /// The handoff won: the row is terminal, its charge retained, and no
    /// provider query or replay occurred.
    Recovered {
        attempt: Box<CiRouteAttempt>,
        outcome: CiRouteOutcome,
        tier2_lease_id: Option<String>,
    },
}

/// Result of opening a Tier-2 lease.
#[derive(Clone, Debug)]
pub enum CiTier2LeaseOutcome {
    /// This caller opened the unique lease for that current-evidence key.
    Opened {
        attempt: Box<CiRouteAttempt>,
        lease_id: String,
    },
    /// Another row on this subject holds the open lease for that
    /// current-evidence key. Mutually exclusive by construction.
    KeyHeldElsewhere(Box<CiRouteAttempt>),
    /// This row has already used its one trip to Tier 2 — the lease is open,
    /// or it resolved and may not be reopened.
    AlreadyRoutedToTier2(Box<CiRouteAttempt>),
    /// The row is in `calling`: a provider call is in flight and the row
    /// belongs to that owner. Nothing was leased, nothing was closed.
    ///
    /// This is a legal race, not a caller error — a poller can read a row as
    /// `reserved`, decide to escalate, and lose to another poller's charge in
    /// between. The route is adjudicated (or not) after the owner's provider
    /// result lands, or after startup recovery, never by taking it here.
    OwnedByProviderCall(Box<CiRouteAttempt>),
    /// The compare-and-set failed: the identity changed or a newer
    /// passing/merged observation already terminalized the row. The row is
    /// closed as `superseded_before_lead` with no lease and no Lead dispatch.
    SupersededBeforeLead(Box<CiRouteAttempt>),
    NotFound,
}

/// Result of [`CiRouteAttemptRepository::attach_lead_session`].
///
/// The two variants answer "did the fence match?", not "was this the first
/// session?" — that question is answered by `session_count`, which is the
/// post-increment total. `session_count == 1` is a first attach; anything
/// higher is an additional Lead session on the same route, which is a real and
/// reportable cost the previous blind overwrite silently erased.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiLeadSessionAttachment {
    /// The session was counted against this route's open lease.
    Attached {
        /// Total Lead sessions attached to this route, including this one.
        session_count: i64,
    },
    /// No row matched the fence: wrong lease id, or the lease is no longer
    /// open. Nothing was written.
    NotFound,
}

/// A validated Tier-2 resolution, applied atomically with the guard.
#[derive(Clone, Debug)]
pub struct CiTier2Resolution {
    pub outcome: CiRouteOutcome,
    pub reopen_mode: Option<CiReopenMode>,
    pub diagnostic_reason: Option<CiDiagnosticReason>,
    pub park_justification: Option<String>,
    /// Set when this resolution is the **fallback** rather than Lead's own
    /// result. See [`CiLeadRejection`] for why it cannot be inferred from
    /// `diagnostic_reason`.
    pub rejection: Option<CiLeadRejection>,
}

impl CiTier2Resolution {
    /// A validated repair reopen. The verification command itself is
    /// validated by the supervisor in wave 4; what is durable here is that
    /// the reopen was a *repair*, which is what makes a later diagnostic
    /// reason on the same row a contradiction the CHECK rejects.
    #[must_use]
    pub fn repair() -> Self {
        Self {
            outcome: CiRouteOutcome::RepairReopened,
            reopen_mode: Some(CiReopenMode::Repair),
            diagnostic_reason: None,
            park_justification: None,
            rejection: None,
        }
    }

    /// A validated diagnostic reopen citing one closed reason.
    #[must_use]
    pub fn diagnose(reason: CiDiagnosticReason) -> Self {
        Self {
            outcome: CiRouteOutcome::DiagnosticReopened,
            reopen_mode: Some(CiReopenMode::Diagnose),
            diagnostic_reason: Some(reason),
            park_justification: None,
            rejection: None,
        }
    }

    /// A park with a cited infrastructure dead-end.
    #[must_use]
    pub fn park(justification: impl Into<String>) -> Self {
        Self {
            outcome: CiRouteOutcome::Parked,
            reopen_mode: None,
            diagnostic_reason: None,
            park_justification: Some(justification.into()),
            rejection: None,
        }
    }

    /// The plain outcomes that carry no reopen payload.
    #[must_use]
    pub fn plain(outcome: CiRouteOutcome) -> Self {
        Self {
            outcome,
            reopen_mode: None,
            diagnostic_reason: None,
            park_justification: None,
            rejection: None,
        }
    }

    /// Mark this resolution as the fallback that replaced a delivered result.
    ///
    /// Only legal on a diagnostic reopen: the fallback is *always* one
    /// diagnostic reopen, and a repair, park, or supersede is by definition a
    /// result Lead produced and this contract accepted. [`Self::validate`]
    /// enforces that, and so does migration 193's
    /// `ci_route_attempts_rejection_pairing_check`.
    #[must_use]
    pub fn rejected_as(mut self, rejection: CiLeadRejection) -> Self {
        self.rejection = Some(rejection);
        self
    }

    fn validate(&self) -> Result<()> {
        if self.rejection.is_some() && self.reopen_mode != Some(CiReopenMode::Diagnose) {
            return Err(DbError::InvalidData(format!(
                "`{}` cannot carry a Lead rejection: the fallback is always one diagnostic reopen",
                self.outcome.as_str()
            )));
        }
        if self.outcome.is_provider_finalization() {
            return Err(DbError::InvalidData(format!(
                "`{}` asserts a provider call and cannot be a Tier-2 resolution",
                self.outcome.as_str()
            )));
        }
        match self.outcome {
            CiRouteOutcome::RepairReopened => {
                if self.reopen_mode != Some(CiReopenMode::Repair) {
                    return Err(DbError::InvalidData(
                        "repair_reopened requires reopen_mode=repair".to_owned(),
                    ));
                }
                if self.diagnostic_reason.is_some() {
                    return Err(DbError::InvalidData(
                        "repair_reopened forbids a diagnostic reason".to_owned(),
                    ));
                }
            }
            CiRouteOutcome::DiagnosticReopened => {
                if self.reopen_mode != Some(CiReopenMode::Diagnose) {
                    return Err(DbError::InvalidData(
                        "diagnostic_reopened requires reopen_mode=diagnose".to_owned(),
                    ));
                }
                if self.diagnostic_reason.is_none() {
                    return Err(DbError::InvalidData(
                        "diagnostic_reopened requires a closed diagnostic reason".to_owned(),
                    ));
                }
            }
            CiRouteOutcome::Parked => {
                if !self
                    .park_justification
                    .as_deref()
                    .is_some_and(|j| !j.trim().is_empty())
                {
                    return Err(DbError::InvalidData(
                        "parked requires a cited justification".to_owned(),
                    ));
                }
            }
            _ => {
                if self.reopen_mode.is_some() || self.diagnostic_reason.is_some() {
                    return Err(DbError::InvalidData(format!(
                        "{} carries no reopen payload",
                        self.outcome.as_str()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// The repository-checkable half of the rollback quiescence report.
///
/// The in-process half (registered provider-action futures) belongs to the
/// coordinator and is not knowable from here. Wave 5 pairs the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRouteQuiescence {
    pub reserved_rows: i64,
    pub calling_rows: i64,
    pub open_tier2_leases: i64,
    /// Open leases that already have a Lead session attached — a dispatched
    /// adjudication whose result has not been applied.
    pub unapplied_lead_results: i64,
}

impl CiRouteQuiescence {
    /// Whether every repository-visible count is zero.
    #[must_use]
    pub fn is_quiescent(self) -> bool {
        self.reserved_rows == 0
            && self.calling_rows == 0
            && self.open_tier2_leases == 0
            && self.unapplied_lead_results == 0
    }
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

pub struct CiRouteAttemptRepository {
    db: Database,
}

impl CiRouteAttemptRepository {
    #[must_use]
    /// The pool this repository reads and writes.
    ///
    /// Sibling module access only ([`super::ci_route_report`]), so the
    /// reporting reads sit next to the writes they report on without either
    /// side re-deriving a connection.
    pub(crate) fn db(&self) -> &Database {
        &self.db
    }

    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Fetch one attempt by its subject and provider-action key.
    ///
    /// # Errors
    ///
    /// Database failures, or a row carrying a value outside a durable
    /// vocabulary.
    pub async fn get(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
    ) -> Result<Option<CiRouteAttempt>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(&format!(
            "SELECT {ROW_COLUMNS} FROM ci_route_attempts WHERE {BY_PK}"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .fetch_optional(self.db.pool())
        .await?;
        row.as_ref().map(CiRouteAttempt::from_row).transpose()
    }

    /// Read both monotonic counters without charging either.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn budget_counts(
        &self,
        subject: &CiRouteSubject,
        retry_budget_key: &str,
        head_budget_key: &str,
    ) -> Result<CiBudgetCounts> {
        self.db.ensure_initialized().await?;
        let read = |scope: &'static str, key: String| async move {
            let count: Option<i64> = sqlx::query_scalar(
                "SELECT charged_count FROM ci_route_budget_counters \
                 WHERE subject_kind = $1 AND subject_id = $2 AND scope = $3 AND counter_key = $4",
            )
            .bind(subject.kind.as_str())
            .bind(&subject.id)
            .bind(scope)
            .bind(key)
            .fetch_optional(self.db.pool())
            .await?;
            Ok::<i64, DbError>(count.unwrap_or(0))
        };
        Ok(CiBudgetCounts {
            signature: read("signature", retry_budget_key.to_owned()).await?,
            head: read("head", head_budget_key.to_owned()).await?,
        })
    }

    /// Phase 1 of the action protocol: lock and read both budgets, reject
    /// exhausted thresholds, and insert the unique evidence row as `reserved`.
    ///
    /// Committing before the call is the entire point — see the module docs.
    /// Provider mutation is forbidden while the row is `reserved`.
    ///
    /// # The exhaustion refusal is scoped to actions that can charge
    ///
    /// A budget ceiling exists to stop *provider calls*, and only
    /// [`CiAction::RerunRun`] and [`CiAction::Reenqueue`] can ever reach one:
    /// `charge_and_begin_calling` is the sole writer of both counters.
    ///
    /// Refusing every reservation on an exhausted budget therefore made the
    /// proposal's own retry-exhaustion escalation unreachable from a fresh
    /// observation. `apply_budget` in the coordinator's classifier turns a
    /// Tier-1 route whose ceiling is spent into `CiTier2Reason::RetryExhausted`
    /// — an `ask_lead` route that charges nothing and calls nothing — and that
    /// route needs a row, because `open_tier2_lease` operates on one. With a
    /// blanket refusal there was no row, so no lease, so no adjudication: an
    /// exhausted PR head simply stopped retrying and escalated to nobody.
    ///
    /// So the refusal applies to the two charging actions and nothing else. An
    /// `ask_lead`, `hold`, or `discard` reservation is admitted regardless of
    /// the counters and still cannot charge them, because the charge path is a
    /// different method with its own independent ceiling check
    /// (`CiChargeOutcome::BudgetExhausted`). The ceiling is enforced where the
    /// spending happens, not where the bookkeeping starts.
    ///
    /// # The run-id sentinel is refused here, before the CHECK
    ///
    /// `run_id` is either a real provider run or `None`. A `Some(0)` — or any
    /// non-positive value — is the fabricated placeholder migration 195 exists
    /// to abolish, and it is rejected here rather than left to
    /// `ci_route_attempts_run_id_positive_check` so the caller gets a sentence
    /// naming the field instead of SQLSTATE 23514 naming a constraint. The
    /// CHECK is still the enforcement; this is the readable error.
    ///
    /// # Errors
    ///
    /// Database failures, or a non-positive `run_id`.
    pub async fn reserve(&self, input: &CiRouteReservation) -> Result<CiReserveOutcome> {
        self.db.ensure_initialized().await?;
        if let Some(run_id) = input.identity.run_id
            && run_id <= 0
        {
            return Err(DbError::InvalidData(format!(
                "run_id `{run_id}` is not a provider run: an evidence identity that names no run \
                 must use `run_id: None`, never a `0` placeholder"
            )));
        }
        let mut tx = self.db.pool().begin().await?;

        if let Some(existing) =
            fetch_in_tx(&mut tx, &input.subject, &input.provider_action_key).await?
        {
            tx.commit().await?;
            return Ok(CiReserveOutcome::AlreadyPresent(Box::new(existing)));
        }

        let counts = lock_budgets(
            &mut tx,
            &input.subject,
            &input.retry_budget_key,
            &input.head_budget_key,
        )
        .await?;
        if counts.is_exhausted() && input.action.charges_budget() {
            tx.commit().await?;
            return Ok(CiReserveOutcome::BudgetExhausted(counts));
        }

        // `ON CONFLICT DO NOTHING` rather than a bare INSERT. The read above
        // cannot lock a row that does not exist yet, so two concurrent pollers
        // both see "absent"; the budget lock then serializes them and the
        // loser's insert must resolve to `AlreadyPresent` rather than a raw
        // unique violation. The unique key is still what enforces one row per
        // evidence identity — this only decides how the loser is *told*.
        //
        // **No conflict target**, deliberately, so the arbiter is *any* unique
        // index. Two of them can fire here and they are not the same event:
        //
        // * the primary key — a duplicate poll recomputed the same
        //   `provider_action_key`; and
        // * `ci_route_attempts_evidence_identity_uniq` — a *different* key over
        //   the same expanded identity. That is what happens when two distinct
        //   irrecoverable reasons produce two run-absent captures of one
        //   lane/PR/head/dequeue: under `NULLS NOT DISTINCT` they are one
        //   identity, so they collapse onto one row.
        //
        // A named target would have covered only the first and let the second
        // escape as SQLSTATE 23505 — a raw constraint violation out of the
        // method whose entire job is to answer "is this evidence already
        // routed?".
        let inserted = sqlx::query(
            "INSERT INTO ci_route_attempts (subject_kind, subject_id, provider_action_key, lane, pr_number, pr_head_sha, run_id, run_head_sha, dequeue_id, \
             origin_state, class, action, transient_fingerprint, retry_budget_key, head_budget_key, action_phase) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,'reserved') \
             ON CONFLICT DO NOTHING",
        )
        .bind(input.subject.kind.as_str())
        .bind(&input.subject.id)
        .bind(&input.provider_action_key)
        .bind(input.identity.lane.as_str())
        .bind(input.identity.pr_number)
        .bind(&input.identity.pr_head_sha)
        .bind(input.identity.run_id)
        .bind(&input.identity.run_head_sha)
        .bind(input.identity.dequeue_id.as_deref())
        .bind(input.origin_state.as_str())
        .bind(input.class.as_str())
        .bind(input.action.as_str())
        .bind(&input.transient_fingerprint)
        .bind(&input.retry_budget_key)
        .bind(&input.head_budget_key)
        .execute(&mut *tx)
        .await?;

        // The authoritative row is normally this key's own. When the identity
        // index was the arbiter it is a row under a DIFFERENT key, so the
        // fallback looks the collapse up by the expanded identity — otherwise
        // the caller would be told "row disappeared" about a row that is
        // sitting right there.
        let row = match fetch_in_tx(&mut tx, &input.subject, &input.provider_action_key).await? {
            Some(row) => row,
            None => fetch_by_evidence_identity_in_tx(
                &mut tx,
                &input.subject,
                &input.identity,
                input.action,
            )
            .await?
            .ok_or_else(|| DbError::Internal("reserved route row disappeared".to_owned()))?,
        };
        tx.commit().await?;
        Ok(if inserted.rows_affected() == 1 {
            CiReserveOutcome::Reserved(Box::new(row))
        } else {
            CiReserveOutcome::AlreadyPresent(Box::new(row))
        })
    }

    /// Phases 2 and 3: revalidate identity, then atomically compare-and-set
    /// `reserved` -> `calling` while incrementing both monotonic counters.
    ///
    /// **Only the winner may call the provider.** Concurrent callers serialize
    /// on the row lock and every loser sees a non-`reserved` phase.
    ///
    /// The charge is committed in the same transaction as the phase change, so
    /// a crash immediately after this returns leaves a `calling` row that is
    /// already charged — which is exactly the conservative direction: slots
    /// are never released once an action reaches `calling`, regardless of what
    /// the provider answered.
    ///
    /// # Errors
    ///
    /// Database failures, or no such reservation.
    pub async fn charge_and_begin_calling(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        owner_incarnation_id: &str,
        observed_identity: &CiEvidenceIdentity,
    ) -> Result<CiChargeOutcome> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let Some(attempt) = fetch_locked(&mut tx, subject, provider_action_key).await? else {
            tx.commit().await?;
            return Err(DbError::InvalidData(format!(
                "no ci route attempt for `{}`/`{}`/`{provider_action_key}`",
                subject.kind.as_str(),
                subject.id
            )));
        };

        if attempt.action_phase != CiActionPhase::Reserved {
            tx.commit().await?;
            return Ok(CiChargeOutcome::NotReserved(Box::new(attempt)));
        }

        if !attempt.identity.matches(observed_identity) {
            let superseded = terminalize_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                CiRouteOutcome::SupersededPreCall,
                Some(&identity_evidence_json(observed_identity)),
            )
            .await?;
            tx.commit().await?;
            return Ok(CiChargeOutcome::SupersededPreCall(Box::new(superseded)));
        }

        let counts = lock_budgets(
            &mut tx,
            subject,
            &attempt.retry_budget_key,
            &attempt.head_budget_key,
        )
        .await?;
        if counts.is_exhausted() {
            sqlx::query(&format!(
                "UPDATE ci_route_attempts SET retry_exhausted_at = now(), updated_at = now() \
                 WHERE {BY_PK} AND retry_exhausted_at IS NULL"
            ))
            .bind(subject.kind.as_str())
            .bind(&subject.id)
            .bind(provider_action_key)
            .execute(&mut *tx)
            .await?;
            let refreshed = fetch_in_tx(&mut tx, subject, provider_action_key)
                .await?
                .ok_or_else(|| DbError::Internal("route row disappeared".to_owned()))?;
            tx.commit().await?;
            return Ok(CiChargeOutcome::BudgetExhausted {
                attempt: Box::new(refreshed),
                counts,
            });
        }

        let charged = charge_budgets_in_tx(
            &mut tx,
            subject,
            &attempt.retry_budget_key,
            &attempt.head_budget_key,
        )
        .await?;

        // The CAS. `action_phase = 'reserved'` in the WHERE clause is what
        // makes a double charge impossible independently of the row lock: a
        // caller that got as far as incrementing but lost the phase race
        // fails here and rolls its increment back with the transaction.
        let updated = sqlx::query(&format!(
            "UPDATE ci_route_attempts \
             SET action_phase = 'calling', calling_at = now(), owner_incarnation_id = $4, \
                 charged_signature_count = $5, charged_head_count = $6, updated_at = now() \
             WHERE {BY_PK} AND action_phase = 'reserved'"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(owner_incarnation_id)
        .bind(charged.signature)
        .bind(charged.head)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(DbError::Internal(
                "reserved->calling compare-and-set lost under a held row lock".to_owned(),
            ));
        }

        let calling = fetch_in_tx(&mut tx, subject, provider_action_key)
            .await?
            .ok_or_else(|| DbError::Internal("calling route row disappeared".to_owned()))?;
        tx.commit().await?;
        Ok(CiChargeOutcome::Charged {
            attempt: Box::new(calling),
            counts: charged,
        })
    }

    /// Phase 4: the owner records its own provider result.
    ///
    /// **Owner-scoped.** The compare-and-set carries the caller's
    /// `owner_incarnation_id`, so a former owner whose row was legally handed
    /// off writes nothing and observes `false`. That is the whole mechanism by
    /// which a late result from a drained process is rejected rather than
    /// silently overwriting a recovery.
    ///
    /// This is the **only** path to a provider-finalization outcome;
    /// [`Self::terminalize`] refuses them.
    ///
    /// # Errors
    ///
    /// Database failures, or an outcome outside the provider-terminal set.
    pub async fn finalize_calling(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        owner_incarnation_id: &str,
        outcome: CiRouteOutcome,
        provider_error_json: Option<&str>,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        if !outcome.is_provider_finalization() {
            return Err(DbError::InvalidData(format!(
                "`{}` is not a provider finalization outcome",
                outcome.as_str()
            )));
        }
        let updated = sqlx::query(&format!(
            "UPDATE ci_route_attempts \
             SET action_phase = 'terminal', terminal_outcome = $5, terminalized_at = now(), \
                 provider_error = $6::jsonb, updated_at = now() \
             WHERE {BY_PK} AND action_phase = 'calling' AND owner_incarnation_id = $4"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(owner_incarnation_id)
        .bind(outcome.as_str())
        .bind(provider_error_json)
        .execute(self.db.pool())
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Write-once terminalization for every non-provider route close.
    ///
    /// Returns `true` only for the caller that actually wrote the outcome. A
    /// second call — from a duplicate poll, a startup sweep, or a delayed Lead
    /// result — observes `false` and must not treat the row as freshly closed.
    ///
    /// **Two fences, both deliberate.** This sits beside the owner-scoped
    /// [`Self::finalize_calling`], and an unfenced sibling would be a way
    /// around it:
    ///
    /// * a provider-finalization outcome is rejected outright, so nothing but
    ///   the `calling` owner can ever claim the provider was called; and
    /// * a row in `calling` is refused, so a sweep cannot close a route whose
    ///   provider future is still live. Such a row belongs to its owner and
    ///   leaves `calling` only through that owner's finalizer or through
    ///   [`Self::recover_calling_owner`].
    ///
    /// # Errors
    ///
    /// Database failures, a provider-finalization outcome, or a `calling` row.
    pub async fn terminalize(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        outcome: CiRouteOutcome,
        superseded_by_evidence_json: Option<&str>,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        if outcome.is_provider_finalization() {
            return Err(DbError::InvalidData(format!(
                "`{}` may only be written by the calling owner through finalize_calling",
                outcome.as_str()
            )));
        }
        let mut tx = self.db.pool().begin().await?;
        let Some(attempt) = fetch_locked(&mut tx, subject, provider_action_key).await? else {
            tx.commit().await?;
            return Ok(false);
        };
        if attempt.action_phase == CiActionPhase::Calling {
            tx.commit().await?;
            return Err(DbError::InvalidTransition(format!(
                "route `{provider_action_key}` is owned by incarnation `{}` in phase calling; \
                 it can only be closed by that owner or by startup recovery",
                attempt.owner_incarnation_id.as_deref().unwrap_or("<none>")
            )));
        }
        let wrote = terminalize_cas_in_tx(
            &mut tx,
            subject,
            provider_action_key,
            outcome,
            superseded_by_evidence_json,
        )
        .await?;
        tx.commit().await?;
        Ok(wrote)
    }

    /// Pre-call recovery. See the module docs for the three-outcome contract.
    ///
    /// `tier2_lease_key` is the current-evidence lease scope used only in the
    /// exhausted branch; it is passed in because its derivation is the
    /// coordinator's (wave 2's) contract, not the repository's.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn recover_reserved(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        observed_identity: &CiEvidenceIdentity,
        recovering_incarnation_id: &str,
        reservation_timeout_secs: i64,
        tier2_lease_key: &str,
    ) -> Result<CiReservedRecovery> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let Some(attempt) = fetch_locked(&mut tx, subject, provider_action_key).await? else {
            tx.commit().await?;
            return Ok(CiReservedRecovery::NotFound);
        };

        if attempt.action_phase != CiActionPhase::Reserved {
            tx.commit().await?;
            return Ok(CiReservedRecovery::NotEligible(Box::new(attempt)));
        }

        // Eligibility is measured against the DATABASE clock, from the same
        // `now()` that stamped `reserved_at`. Rendering the timestamp and
        // subtracting it in Rust would compare two clocks, and a coordinator
        // whose clock runs behind Postgres would read every reservation as
        // young and never recover one.
        let eligible: bool = sqlx::query_scalar(&format!(
            "SELECT reserved_at <= now() - make_interval(secs => $4::double precision) \
             FROM ci_route_attempts WHERE {BY_PK}"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(reservation_timeout_secs as f64)
        .fetch_one(&mut *tx)
        .await?;
        if !eligible {
            tx.commit().await?;
            return Ok(CiReservedRecovery::NotEligible(Box::new(attempt)));
        }

        if !attempt.identity.matches(observed_identity) {
            let superseded = terminalize_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                CiRouteOutcome::SupersededPreCall,
                Some(&identity_evidence_json(observed_identity)),
            )
            .await?;
            tx.commit().await?;
            return Ok(CiReservedRecovery::SupersededPreCall(Box::new(superseded)));
        }

        let counts = lock_budgets(
            &mut tx,
            subject,
            &attempt.retry_budget_key,
            &attempt.head_budget_key,
        )
        .await?;

        if counts.is_exhausted() {
            sqlx::query(&format!(
                "UPDATE ci_route_attempts \
                 SET retry_exhausted_at = COALESCE(retry_exhausted_at, now()), \
                     pre_call_resumptions = pre_call_resumptions + 1, updated_at = now() \
                 WHERE {BY_PK}"
            ))
            .bind(subject.kind.as_str())
            .bind(&subject.id)
            .bind(provider_action_key)
            .execute(&mut *tx)
            .await?;
            let grant = open_tier2_lease_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                tier2_lease_key,
                CiTier2Reason::RetryExhausted,
            )
            .await?;
            let refreshed = fetch_in_tx(&mut tx, subject, provider_action_key)
                .await?
                .ok_or_else(|| DbError::Internal("route row disappeared".to_owned()))?;
            tx.commit().await?;
            return Ok(CiReservedRecovery::RetryExhausted {
                attempt: Box::new(refreshed),
                counts,
                tier2_lease_id: grant.opened_id(),
            });
        }

        let charged = charge_budgets_in_tx(
            &mut tx,
            subject,
            &attempt.retry_budget_key,
            &attempt.head_budget_key,
        )
        .await?;

        let updated = sqlx::query(&format!(
            "UPDATE ci_route_attempts \
             SET action_phase = 'calling', calling_at = now(), owner_incarnation_id = $4, \
                 charged_signature_count = $5, charged_head_count = $6, \
                 pre_call_resumptions = pre_call_resumptions + 1, updated_at = now() \
             WHERE {BY_PK} AND action_phase = 'reserved'"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(recovering_incarnation_id)
        .bind(charged.signature)
        .bind(charged.head)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(DbError::Internal(
                "reserved->calling resumption lost under a held row lock".to_owned(),
            ));
        }

        let resumed = fetch_in_tx(&mut tx, subject, provider_action_key)
            .await?
            .ok_or_else(|| DbError::Internal("resumed route row disappeared".to_owned()))?;
        tx.commit().await?;
        Ok(CiReservedRecovery::Resumed {
            attempt: Box::new(resumed),
            counts: charged,
        })
    }

    /// Startup-only owner handoff for a `calling` row.
    ///
    /// Every rejection path writes an audit row, so a deferral is observable.
    /// The predicates, in the order they are checked:
    ///
    /// 1. the caller attests exclusive advisory-lock ownership;
    /// 2. the row is still `calling`;
    /// 3. its owner is the exact named former owner, and differs from the
    ///    recovering incarnation;
    /// 4. the caller attests a quiescence proof that is not
    ///    [`CiQuiescenceProof::None`] — and for a graceful drain, the former
    ///    incarnation must actually carry `provider_actions_drained_at`;
    /// 5. `calling_at` is at least `calling_recovery_timeout_secs` old.
    ///
    /// Only then does it compare-and-set.
    ///
    /// `observed_identity` is **not** optional, deliberately. A caller that
    /// cannot observe the current identity must not call this at all: passing
    /// "unknown" would resolve to `superseded_after_call` and silently suppress
    /// the Tier-2 adjudication a possibly-current row is owed.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn recover_calling_owner(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        observed_identity: &CiEvidenceIdentity,
        authority: &CiCallingRecoveryAuthority,
        tier2_lease_key: &str,
    ) -> Result<CiCallingRecovery> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let Some(attempt) = fetch_locked(&mut tx, subject, provider_action_key).await? else {
            tx.commit().await?;
            return Ok(CiCallingRecovery::NotFound);
        };

        // (1-3) lock ownership, phase, and exact owner. A periodic sweep or a
        // non-leader runtime stops here and can never open Tier 2.
        let deferral = if !authority.holds_exclusive_lock {
            Some(CiCallingRecoveryReason::LockNotHeld)
        } else if attempt.action_phase != CiActionPhase::Calling {
            Some(CiCallingRecoveryReason::NotCalling)
        } else if attempt.owner_incarnation_id.as_deref()
            != Some(authority.former_owner_incarnation.as_str())
            || authority.former_owner_incarnation == authority.recovering_incarnation
        {
            Some(CiCallingRecoveryReason::OwnerMismatch)
        } else {
            None
        };

        if let Some(reason) = deferral {
            record_calling_recovery(
                &mut tx,
                subject,
                provider_action_key,
                authority,
                reason,
                false,
                None,
            )
            .await?;
            tx.commit().await?;
            return Ok(CiCallingRecovery::Deferred {
                attempt: Box::new(attempt),
                reason,
            });
        }

        // (4) quiescence. `None` is never enough, and a claimed graceful
        // drain is checked against the former incarnation's own record rather
        // than believed. There is deliberately no third arm: every attestation
        // this repository accepts is re-derived from durable state here, so no
        // caller can win a row by asserting a proof it did not earn.
        let quiescent = match authority.quiescence_proof {
            CiQuiescenceProof::None => false,
            CiQuiescenceProof::GracefulDrain => {
                let drained: Option<Option<String>> = sqlx::query_scalar(
                    "SELECT provider_actions_drained_at FROM coordinator_incarnations WHERE id = $1",
                )
                .bind(&authority.former_owner_incarnation)
                .fetch_optional(&mut *tx)
                .await?;
                matches!(drained, Some(Some(_)))
            }
        };
        if !quiescent {
            record_calling_recovery(
                &mut tx,
                subject,
                provider_action_key,
                authority,
                CiCallingRecoveryReason::LiveOwnerDeferred,
                false,
                None,
            )
            .await?;
            tx.commit().await?;
            return Ok(CiCallingRecovery::Deferred {
                attempt: Box::new(attempt),
                reason: CiCallingRecoveryReason::LiveOwnerDeferred,
            });
        }

        // (5) the 300s floor, again measured by the database clock.
        let elapsed: bool = sqlx::query_scalar(&format!(
            "SELECT calling_at <= now() - make_interval(secs => $4::double precision) \
             FROM ci_route_attempts WHERE {BY_PK}"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(authority.calling_recovery_timeout_secs as f64)
        .fetch_one(&mut *tx)
        .await?;
        if !elapsed {
            record_calling_recovery(
                &mut tx,
                subject,
                provider_action_key,
                authority,
                CiCallingRecoveryReason::TimeoutNotElapsed,
                false,
                None,
            )
            .await?;
            tx.commit().await?;
            return Ok(CiCallingRecovery::Deferred {
                attempt: Box::new(attempt),
                reason: CiCallingRecoveryReason::TimeoutNotElapsed,
            });
        }

        // The external outcome is unknowable and is never queried or replayed.
        // A still-current row becomes `outcome_unknown` and keeps its charge;
        // an obsolete one becomes `superseded_after_call`.
        let still_current = attempt.identity.matches(observed_identity);
        let outcome = if still_current {
            CiRouteOutcome::OutcomeUnknown
        } else {
            CiRouteOutcome::SupersededAfterCall
        };

        let won = sqlx::query(&format!(
            "UPDATE ci_route_attempts \
             SET owner_incarnation_id = $5, action_phase = 'terminal', terminal_outcome = $6, \
                 terminalized_at = now(), superseded_by_evidence = COALESCE($7::jsonb, superseded_by_evidence), \
                 updated_at = now() \
             WHERE {BY_PK} AND action_phase = 'calling' AND owner_incarnation_id = $4"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(&authority.former_owner_incarnation)
        .bind(&authority.recovering_incarnation)
        .bind(outcome.as_str())
        .bind(identity_evidence_json(observed_identity))
        .execute(&mut *tx)
        .await?;

        // Belt and braces. This transaction has held the row lock since
        // `fetch_locked`, so a finalizer racing us blocked before its own
        // UPDATE and this compare-and-set cannot actually lose today — the
        // owner-mismatch and phase deferrals above catch a finalizer that
        // committed *first*. It is kept because the alternative to a lost CAS
        // being observable is a lost CAS being silently treated as a win, and
        // the audit row makes it falsifiable if the locking ever changes.
        if won.rows_affected() != 1 {
            let refreshed = fetch_in_tx(&mut tx, subject, provider_action_key)
                .await?
                .ok_or_else(|| DbError::Internal("route row disappeared".to_owned()))?;
            record_calling_recovery(
                &mut tx,
                subject,
                provider_action_key,
                authority,
                CiCallingRecoveryReason::CasLost,
                false,
                None,
            )
            .await?;
            tx.commit().await?;
            return Ok(CiCallingRecovery::Deferred {
                attempt: Box::new(refreshed),
                reason: CiCallingRecoveryReason::CasLost,
            });
        }

        let lease_id = if still_current {
            open_tier2_lease_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                tier2_lease_key,
                CiTier2Reason::OutcomeUnknown,
            )
            .await?
            .opened_id()
        } else {
            None
        };

        let refreshed = fetch_in_tx(&mut tx, subject, provider_action_key)
            .await?
            .ok_or_else(|| DbError::Internal("route row disappeared".to_owned()))?;
        record_calling_recovery(
            &mut tx,
            subject,
            provider_action_key,
            authority,
            CiCallingRecoveryReason::StartupOwnerHandoff,
            true,
            Some(outcome),
        )
        .await?;
        tx.commit().await?;
        Ok(CiCallingRecovery::Recovered {
            attempt: Box::new(refreshed),
            outcome,
            tier2_lease_id: lease_id,
        })
    }

    /// Open the unique Tier-2 lease for a current-evidence key.
    ///
    /// The compare-and-set is the guard the proposal requires *before* Lead
    /// dispatch: the stored identity must still equal the observed current
    /// identity and the row must not already have been closed by a newer
    /// passing or merged observation. A failed guard closes the row
    /// `superseded_before_lead` and opens nothing.
    ///
    /// A row routes to Tier 2 **at most once, ever**. Both the open lease and
    /// a resolved one refuse a second adjudication; only the partial unique
    /// index is released on resolution, and that release is for a *different*
    /// row carrying genuinely newer evidence.
    ///
    /// `tier2_lease_key` is derived by wave 2 from **(PR number, PR-head
    /// SHA)** — never the lane, the run id, or the dequeue id. A lane-scoped
    /// key would allow two concurrent Lead adjudications for one PR head and
    /// defeat the hold. The subject scoping supplies repository identity, so
    /// the key does not need to.
    ///
    /// **Head scope is a choice among three conflicting spec statements, not a
    /// derivation.** Proposal `nafu` calls this an "evidence-scoped route
    /// lease" (lane transitions table, which would include the run id), "at
    /// most one Lead adjudication for current evidence" (Idempotency, which is
    /// ambiguous), and "a head-level hold" (Risks). Head scope is the most
    /// conservative reading and the only one under which the retry-storm
    /// safeguard exists, so it was chosen — but a wave that reads the
    /// lane-transitions wording literally would widen the hold and reintroduce
    /// concurrent adjudications for one head. Resolve the spec before
    /// changing it.
    ///
    /// The hold is **per subject**, not global: the unique index is
    /// `(subject_kind, subject_id, tier2_lease_key)`, so two different
    /// subjects can hold the identical key open simultaneously. Every subject
    /// is a task today and one PR maps to one task, so no two subjects share a
    /// PR head and the distinction is invisible. It stops being invisible the
    /// moment a non-task subject shares a head — see the `tier2_lease_key`
    /// note in migration 193.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn open_tier2_lease(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        observed_identity: &CiEvidenceIdentity,
        tier2_lease_key: &str,
        reason: CiTier2Reason,
    ) -> Result<CiTier2LeaseOutcome> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let Some(attempt) = fetch_locked(&mut tx, subject, provider_action_key).await? else {
            tx.commit().await?;
            return Ok(CiTier2LeaseOutcome::NotFound);
        };

        // A row whose provider call is in flight belongs to its owner, and
        // this is a benign race rather than a caller bug: W2 can read a row as
        // `reserved`, decide to escalate, and have another poller charge it to
        // `calling` before this call lands.
        //
        // Returning early matters twice over. It stops the supersession branch
        // below from stealing the row — which it used to do, flipping a live
        // `calling` row to terminal `superseded_before_lead` and making the
        // owner's `finalize_calling` a silent no-op. And it keeps the reported
        // outcome honest: with only the structural fence in
        // `terminalize_cas_in_tx`, the write would be refused but this method
        // would still announce `SupersededBeforeLead`.
        if attempt.action_phase == CiActionPhase::Calling {
            tx.commit().await?;
            return Ok(CiTier2LeaseOutcome::OwnedByProviderCall(Box::new(attempt)));
        }

        // A row already terminalized by a newer passing/merged observation is
        // exactly the "newer outcome" half of the guard: the pass/merge path
        // closes route keys, so `is_terminal` covers it. `outcome_unknown` and
        // `action_failed` are the two terminal outcomes that legally still
        // open a lease.
        let terminal_blocks_lease = attempt.is_terminal()
            && !matches!(
                attempt.terminal_outcome,
                Some(CiRouteOutcome::OutcomeUnknown) | Some(CiRouteOutcome::ActionFailed)
            );

        if !attempt.identity.matches(observed_identity) || terminal_blocks_lease {
            let closed = terminalize_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                CiRouteOutcome::SupersededBeforeLead,
                Some(&identity_evidence_json(observed_identity)),
            )
            .await?;
            tx.commit().await?;
            return Ok(CiTier2LeaseOutcome::SupersededBeforeLead(Box::new(closed)));
        }

        let grant = open_tier2_lease_in_tx(
            &mut tx,
            subject,
            provider_action_key,
            tier2_lease_key,
            reason,
        )
        .await?;
        let refreshed = fetch_in_tx(&mut tx, subject, provider_action_key)
            .await?
            .ok_or_else(|| DbError::Internal("route row disappeared".to_owned()))?;
        tx.commit().await?;
        Ok(match grant {
            Tier2Grant::Opened(id) => CiTier2LeaseOutcome::Opened {
                attempt: Box::new(refreshed),
                lease_id: id,
            },
            Tier2Grant::AlreadyRoutedToTier2 => {
                CiTier2LeaseOutcome::AlreadyRoutedToTier2(Box::new(refreshed))
            }
            Tier2Grant::KeyHeldElsewhere => {
                CiTier2LeaseOutcome::KeyHeldElsewhere(Box::new(refreshed))
            }
        })
    }

    /// Bind a dispatched Lead session to an open lease. **Append-only.**
    ///
    /// Fenced to the exact lease id so a stale caller cannot attach a session
    /// to a lease that has already been resolved.
    ///
    /// # The id is retained; the count is incremented
    ///
    /// This used to be a blind `SET lead_session_id = $5`, which made a second
    /// Lead session on one route indistinguishable from the first: the row
    /// still showed one id, and `lead_invocations` — which counted rows with a
    /// non-NULL id — could never exceed one per route. The proposal's whole
    /// cost bound is "Lead sessions per merged PR", so the metric was
    /// structurally incapable of reporting the overrun it exists to detect.
    ///
    /// So, when the fence matches:
    ///
    /// * `lead_session_id` is written **only if it is currently NULL**
    ///   (`COALESCE(lead_session_id, $5)`). The FIRST session id is the audit
    ///   handle for the adjudication that opened and is never overwritten — a
    ///   respawn does not rewrite history.
    /// * `lead_session_count` is incremented **every time**. This is the field
    ///   metrics read.
    ///
    /// The returned [`CiLeadSessionAttachment::Attached::session_count`] is the
    /// post-increment total, so a caller can tell a first attach
    /// (`session_count == 1`) from an additional one without re-reading the row.
    ///
    /// # `lead_session_id` is a row id, not a composed handle
    ///
    /// The column is `VARCHAR(36)`: exactly one UUID. Postgres does not
    /// truncate an over-long value into a `VARCHAR(n)` column, it refuses the
    /// statement with `22001` — which arrives here as `Err`, not as
    /// [`CiLeadSessionAttachment::NotFound`]. A caller that composes a handle
    /// out of a task id and a counter therefore fails **every** attach while
    /// every other effect of its dispatch lands, which is how the coordinator's
    /// Tier-2 dispatch went a whole wave binding nothing. Pass an id.
    ///
    /// # Errors
    ///
    /// Database failures, including a `lead_session_id` longer than the column.
    pub async fn attach_lead_session(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        lease_id: &str,
        lead_session_id: &str,
    ) -> Result<CiLeadSessionAttachment> {
        self.db.ensure_initialized().await?;
        let count: Option<i64> = sqlx::query_scalar(&format!(
            "UPDATE ci_route_attempts \
             SET lead_session_id = COALESCE(lead_session_id, $5), \
                 lead_session_count = lead_session_count + 1, updated_at = now() \
             WHERE {BY_PK} AND tier2_lease_id = $4 AND tier2_lease_state = 'open' \
             RETURNING lead_session_count"
        ))
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .bind(lease_id)
        .bind(lead_session_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(match count {
            Some(session_count) => CiLeadSessionAttachment::Attached { session_count },
            None => CiLeadSessionAttachment::NotFound,
        })
    }

    /// Apply a Lead result under the same atomic identity guard.
    ///
    /// Resolving the lease releases the current-evidence key, and — for a row
    /// that has not already terminalized on its provider outcome — writes the
    /// route's single terminal outcome. A row that *did* terminalize earlier
    /// (`action_failed`, `outcome_unknown`) keeps that outcome and records the
    /// adjudication in `tier2_resolution`, because terminalization is
    /// write-once. Use [`CiRouteAttempt::adjudicated_outcome`] to read it back
    /// without caring which case applied.
    ///
    /// # A run-absent route is diagnose-only
    ///
    /// If the stored row names no run ([`CiRouteAttempt::is_run_absent`]), a
    /// [`CiRouteOutcome::RepairReopened`] resolution is refused with
    /// [`DbError::InvalidData`]. There is no run to re-run and no run-scoped CI
    /// evidence for a repair's `verification_command` to be copied from, so the
    /// repair would dispatch a worker against a plan nothing grounds.
    ///
    /// This is the **database-side half** of the rule and it is deliberately
    /// the last one: the agent-side validator rejects the delivered result with
    /// [`CiLeadRejection::RepairUnavailableForRoute`] and substitutes the
    /// diagnostic fallback, which is where the good explanation belongs. This
    /// fence exists because a guard that lives only in the layer above is a
    /// guard that a future call site can miss. A diagnose resolution on the
    /// same row is accepted unchanged.
    ///
    /// # Errors
    ///
    /// Database failures, a resolution whose payload contradicts its outcome,
    /// or a repair reopen on a run-absent route.
    pub async fn resolve_tier2_lease(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
        lease_id: &str,
        observed_identity: &CiEvidenceIdentity,
        resolution: &CiTier2Resolution,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        resolution.validate()?;
        let mut tx = self.db.pool().begin().await?;

        let Some(attempt) = fetch_locked(&mut tx, subject, provider_action_key).await? else {
            tx.commit().await?;
            return Ok(false);
        };
        if attempt.tier2_lease_id.as_deref() != Some(lease_id)
            || attempt.tier2_lease_state != Some(CiTier2LeaseState::Open)
        {
            tx.commit().await?;
            return Ok(false);
        }

        // Diagnose-only. Checked against the STORED identity rather than the
        // observed one: what makes a repair ungroundable is that *this route*
        // never named a run, and that is a property of the row the Lead session
        // was dispatched about.
        if attempt.is_run_absent() && resolution.outcome == CiRouteOutcome::RepairReopened {
            tx.commit().await?;
            return Err(DbError::InvalidData(format!(
                "route `{provider_action_key}` names no run, so it is diagnose-only: a repair \
                 reopen has nothing to re-run and no run-scoped evidence to ground a \
                 verification command in"
            )));
        }

        // Same fence as `open_tier2_lease`, for the same reason: the
        // supersession branch below terminalizes, and a `calling` row belongs
        // to its owner. Unreachable while `open_tier2_lease` refuses to lease
        // a `calling` row — which is exactly why it is here. This repository
        // does not rely on one guard holding somewhere else.
        if attempt.action_phase == CiActionPhase::Calling {
            tx.commit().await?;
            return Ok(false);
        }

        // The supervisor-side guard, evaluated in the same transaction as the
        // transition it authorizes. A head, lane, or dequeue change since
        // dispatch closes the route without any board mutation.
        if !attempt.identity.matches(observed_identity) {
            resolve_lease_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                lease_id,
                &CiTier2Resolution::plain(CiRouteOutcome::SupersededBeforeApply),
            )
            .await?;
            terminalize_cas_in_tx(
                &mut tx,
                subject,
                provider_action_key,
                CiRouteOutcome::SupersededBeforeApply,
                Some(&identity_evidence_json(observed_identity)),
            )
            .await?;
            tx.commit().await?;
            return Ok(false);
        }

        resolve_lease_in_tx(&mut tx, subject, provider_action_key, lease_id, resolution).await?;
        terminalize_cas_in_tx(
            &mut tx,
            subject,
            provider_action_key,
            resolution.outcome,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Close every `reserved` route for a PR because CI passed or it merged.
    ///
    /// Charges are **not** refunded — a spent slot stays spent. Any open
    /// Tier-2 lease is resolved so its current-evidence key is released and a
    /// delayed Lead result finds nothing to apply to.
    ///
    /// A `calling` row is deliberately left alone: its provider future may be
    /// in flight, and closing it here would steal the row from a live owner
    /// and make its own finalizer a silent no-op. Such a row drains through
    /// that owner or through startup recovery, which is what the rollback
    /// quiescence gate waits for.
    ///
    /// A **merge** additionally stamps [`CiRouteAttempt::pr_merged_at`] on
    /// every row of the PR, whatever its lease state or action phase. That is
    /// the durable "this PR merged" fact and it is what the per-merged-PR cost
    /// ratios divide by; see migration 202 for why it cannot be read off
    /// `tier2_resolution`. The stamp is write-once and touches no adjudication
    /// column, so it neither closes a route nor overwrites a Lead verdict, and
    /// it is not counted in the returned close count.
    ///
    /// Returns the number of routes closed.
    ///
    /// # Errors
    ///
    /// Database failures, or an outcome other than `passed`/`merged`.
    pub async fn close_routes_for_newer_outcome(
        &self,
        subject: &CiRouteSubject,
        pr_number: i64,
        outcome: CiRouteOutcome,
        observed_evidence_json: Option<&str>,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;
        if !matches!(outcome, CiRouteOutcome::Passed | CiRouteOutcome::Merged) {
            return Err(DbError::InvalidData(format!(
                "`{}` is not a newer-outcome close",
                outcome.as_str()
            )));
        }
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            "UPDATE ci_route_attempts \
             SET tier2_lease_state = 'resolved', tier2_resolved_at = now(), \
                 tier2_resolution = COALESCE(tier2_resolution, $4), updated_at = now() \
             WHERE subject_kind = $1 AND subject_id = $2 AND pr_number = $3 \
               AND tier2_lease_state = 'open'",
        )
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(pr_number)
        .bind(outcome.as_str())
        .execute(&mut *tx)
        .await?;
        let closed = sqlx::query(
            "UPDATE ci_route_attempts \
             SET action_phase = 'terminal', terminal_outcome = $4, terminalized_at = now(), \
                 superseded_by_evidence = COALESCE($5::jsonb, superseded_by_evidence), updated_at = now() \
             WHERE subject_kind = $1 AND subject_id = $2 AND pr_number = $3 \
               AND action_phase = 'reserved'",
        )
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(pr_number)
        .bind(outcome.as_str())
        .bind(observed_evidence_json)
        .execute(&mut *tx)
        .await?;
        if outcome == CiRouteOutcome::Merged {
            // No lease-state and no action-phase predicate, on purpose. Both
            // of the statements above are scoped to the routes they may still
            // legally transition; this one records a fact about the PULL
            // REQUEST, which is equally true of a route Lead already
            // adjudicated and of one a live owner is still calling for. The
            // adjudication columns are untouched, so stamping a terminal or
            // `calling` row steals nothing from anybody.
            //
            // `AND pr_merged_at IS NULL` makes the stamp write-once: a PR is
            // polled repeatedly after it merges, and without it every poll
            // would move the timestamp forward and destroy the one thing the
            // column records that a boolean would not.
            sqlx::query(
                "UPDATE ci_route_attempts \
                 SET pr_merged_at = now(), updated_at = now() \
                 WHERE subject_kind = $1 AND subject_id = $2 AND pr_number = $3 \
                   AND pr_merged_at IS NULL",
            )
            .bind(subject.kind.as_str())
            .bind(&subject.id)
            .bind(pr_number)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(closed.rows_affected())
    }

    /// The repository-visible half of the rollback quiescence gate.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn quiescence_counts(&self) -> Result<CiRouteQuiescence> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT \
               COUNT(*) FILTER (WHERE action_phase = 'reserved') AS reserved_rows, \
               COUNT(*) FILTER (WHERE action_phase = 'calling') AS calling_rows, \
               COUNT(*) FILTER (WHERE tier2_lease_state = 'open') AS open_tier2_leases, \
               COUNT(*) FILTER (WHERE tier2_lease_state = 'open' AND lead_session_id IS NOT NULL) AS unapplied_lead_results \
             FROM ci_route_attempts",
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(CiRouteQuiescence {
            reserved_rows: row.try_get("reserved_rows")?,
            calling_rows: row.try_get("calling_rows")?,
            open_tier2_leases: row.try_get("open_tier2_leases")?,
            unapplied_lead_results: row.try_get("unapplied_lead_results")?,
        })
    }

    /// The routes that are still non-terminal, i.e. the routed failed
    /// identities the rollback gate must see advance, pass, or merge.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn non_terminal_identities(
        &self,
    ) -> Result<Vec<(CiRouteSubject, String, CiEvidenceIdentity)>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query(&format!(
            "SELECT {ROW_COLUMNS} FROM ci_route_attempts WHERE action_phase <> 'terminal' \
             ORDER BY reserved_at, subject_kind, subject_id, provider_action_key"
        ))
        .fetch_all(self.db.pool())
        .await?;
        rows.iter()
            .map(|row| {
                CiRouteAttempt::from_row(row)
                    .map(|a| (a.subject, a.provider_action_key, a.identity))
            })
            .collect()
    }

    /// Every recovery attempt recorded against one route, newest last.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn calling_recovery_audit(
        &self,
        subject: &CiRouteSubject,
        provider_action_key: &str,
    ) -> Result<Vec<CiCallingRecoveryRecord>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query(
            "SELECT id, provider_action_key, former_owner_incarnation, recovering_incarnation, \
                    holds_exclusive_lock, quiescence_proof, recovery_reason, \
                    calling_recovery_timeout_secs, cas_won, resulting_outcome \
             FROM ci_route_calling_recoveries \
             WHERE subject_kind = $1 AND subject_id = $2 AND provider_action_key = $3 \
             ORDER BY id",
        )
        .bind(subject.kind.as_str())
        .bind(&subject.id)
        .bind(provider_action_key)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(CiCallingRecoveryRecord {
                    id: row.try_get("id")?,
                    provider_action_key: row.try_get("provider_action_key")?,
                    former_owner_incarnation: row.try_get("former_owner_incarnation")?,
                    recovering_incarnation: row.try_get("recovering_incarnation")?,
                    holds_exclusive_lock: row.try_get("holds_exclusive_lock")?,
                    quiescence_proof: CiQuiescenceProof::parse(
                        row.try_get::<String, _>("quiescence_proof")?.as_str(),
                    )?,
                    recovery_reason: CiCallingRecoveryReason::parse(
                        row.try_get::<String, _>("recovery_reason")?.as_str(),
                    )?,
                    calling_recovery_timeout_secs: row.try_get("calling_recovery_timeout_secs")?,
                    cas_won: row.try_get("cas_won")?,
                    resulting_outcome: match row
                        .try_get::<Option<String>, _>("resulting_outcome")?
                    {
                        Some(v) => Some(CiRouteOutcome::parse(&v)?),
                        None => None,
                    },
                })
            })
            .collect()
    }
}

/// One recorded `calling` recovery attempt, including the legal no-ops.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiCallingRecoveryRecord {
    pub id: String,
    pub provider_action_key: String,
    pub former_owner_incarnation: Option<String>,
    pub recovering_incarnation: String,
    pub holds_exclusive_lock: bool,
    pub quiescence_proof: CiQuiescenceProof,
    pub recovery_reason: CiCallingRecoveryReason,
    pub calling_recovery_timeout_secs: i64,
    pub cas_won: bool,
    pub resulting_outcome: Option<CiRouteOutcome>,
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

async fn fetch_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
) -> Result<Option<CiRouteAttempt>> {
    let row = sqlx::query(&format!(
        "SELECT {ROW_COLUMNS} FROM ci_route_attempts WHERE {BY_PK}"
    ))
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(provider_action_key)
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref().map(CiRouteAttempt::from_row).transpose()
}

/// Find the row that owns an expanded evidence identity, whatever key it was
/// written under.
///
/// `IS NOT DISTINCT FROM` on the two nullable columns rather than `=`, because
/// this query has to reproduce what
/// `ci_route_attempts_evidence_identity_uniq` decided, and that index is
/// `NULLS NOT DISTINCT`: a run-absent, dequeue-less identity must match another
/// run-absent, dequeue-less identity. With plain equality every NULL comparison
/// yields NULL, the `WHERE` drops the row, and the lookup returns nothing for
/// precisely the rows the index collapsed.
async fn fetch_by_evidence_identity_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    identity: &CiEvidenceIdentity,
    action: CiAction,
) -> Result<Option<CiRouteAttempt>> {
    let row = sqlx::query(&format!(
        "SELECT {ROW_COLUMNS} FROM ci_route_attempts \
         WHERE subject_kind = $1 AND subject_id = $2 AND lane = $3 AND pr_number = $4 \
           AND pr_head_sha = $5 AND run_id IS NOT DISTINCT FROM $6 AND run_head_sha = $7 \
           AND dequeue_id IS NOT DISTINCT FROM $8 AND action = $9"
    ))
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(identity.lane.as_str())
    .bind(identity.pr_number)
    .bind(&identity.pr_head_sha)
    .bind(identity.run_id)
    .bind(&identity.run_head_sha)
    .bind(identity.dequeue_id.as_deref())
    .bind(action.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref().map(CiRouteAttempt::from_row).transpose()
}

/// Take the row lock, then re-read through the rendering projection.
///
/// Two statements rather than one because `ROW_COLUMNS` contains `to_char`
/// expressions and Postgres rejects `FOR UPDATE` on a projection it cannot
/// prove is a plain row reference.
async fn fetch_locked(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
) -> Result<Option<CiRouteAttempt>> {
    let locked: Option<(String,)> = sqlx::query_as(&format!(
        "SELECT provider_action_key FROM ci_route_attempts WHERE {BY_PK} FOR UPDATE"
    ))
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(provider_action_key)
    .fetch_optional(&mut **tx)
    .await?;
    if locked.is_none() {
        return Ok(None);
    }
    fetch_in_tx(tx, subject, provider_action_key).await
}

/// Materialize and lock both counter rows, returning their current values.
///
/// The rows are created at zero if absent so the lock has something to hold;
/// creating a counter is not charging it.
async fn lock_budgets(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    retry_budget_key: &str,
    head_budget_key: &str,
) -> Result<CiBudgetCounts> {
    // Deterministic order (signature then head) so two transactions charging
    // the same pair can never deadlock against each other.
    let signature = lock_counter(tx, subject, "signature", retry_budget_key).await?;
    let head = lock_counter(tx, subject, "head", head_budget_key).await?;
    Ok(CiBudgetCounts { signature, head })
}

async fn lock_counter(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    scope: &str,
    counter_key: &str,
) -> Result<i64> {
    sqlx::query(
        "INSERT INTO ci_route_budget_counters (subject_kind, subject_id, scope, counter_key, charged_count) \
         VALUES ($1, $2, $3, $4, 0) \
         ON CONFLICT (subject_kind, subject_id, scope, counter_key) DO NOTHING",
    )
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(scope)
    .bind(counter_key)
    .execute(&mut **tx)
    .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT charged_count FROM ci_route_budget_counters \
         WHERE subject_kind = $1 AND subject_id = $2 AND scope = $3 AND counter_key = $4 FOR UPDATE",
    )
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(scope)
    .bind(counter_key)
    .fetch_one(&mut **tx)
    .await?;
    Ok(count)
}

/// Increment both counters by one and return the new values.
///
/// There is no decrement anywhere in this module. That is the whole of
/// "monotonic and conservative": a slot spent by an action that reached
/// `calling` is never returned, whatever the provider said or whether anyone
/// was still listening.
async fn charge_budgets_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    retry_budget_key: &str,
    head_budget_key: &str,
) -> Result<CiBudgetCounts> {
    let signature = charge_counter(tx, subject, "signature", retry_budget_key).await?;
    let head = charge_counter(tx, subject, "head", head_budget_key).await?;
    Ok(CiBudgetCounts { signature, head })
}

async fn charge_counter(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    scope: &str,
    counter_key: &str,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "UPDATE ci_route_budget_counters \
         SET charged_count = charged_count + 1, \
             first_charged_at = COALESCE(first_charged_at, now()), \
             last_charged_at = now() \
         WHERE subject_kind = $1 AND subject_id = $2 AND scope = $3 AND counter_key = $4 \
         RETURNING charged_count",
    )
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(scope)
    .bind(counter_key)
    .fetch_one(&mut **tx)
    .await?;
    Ok(count)
}

/// The write-once terminal compare-and-set. Returns whether it wrote.
///
/// **`action_phase <> 'calling'` is a structural fence, not a caller
/// courtesy.** A `calling` row's provider future may be in flight; closing it
/// here would steal it, and the owner's own `finalize_calling` — which is
/// compare-and-set against `action_phase = 'calling'` — would then return
/// `false` and silently discard a real provider result.
///
/// The fence lives in this helper rather than at each call site because every
/// non-owner terminalization funnels through here, and the cost of one call
/// site forgetting it is a lost provider outcome that nothing reports. No
/// legitimate caller needs it: `charge_and_begin_calling` and
/// `recover_reserved` only reach terminalization from `reserved`,
/// `close_routes_for_newer_outcome` carries its own `= 'reserved'` predicate,
/// and `recover_calling_owner` — the one path that may legally take a
/// `calling` row — uses its own owner-fenced UPDATE and never comes through
/// here.
///
/// Callers that could be handed a `calling` row must still check the phase
/// themselves *before* calling, so the outcome they report stays truthful: a
/// silent `false` here would otherwise let them announce a supersession that
/// did not happen.
async fn terminalize_cas_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
    outcome: CiRouteOutcome,
    superseded_by_evidence_json: Option<&str>,
) -> Result<bool> {
    let updated = sqlx::query(&format!(
        "UPDATE ci_route_attempts \
         SET action_phase = 'terminal', terminal_outcome = $4, terminalized_at = now(), \
             superseded_by_evidence = COALESCE($5::jsonb, superseded_by_evidence), updated_at = now() \
         WHERE {BY_PK} AND action_phase <> 'terminal' AND action_phase <> 'calling'"
    ))
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(provider_action_key)
    .bind(outcome.as_str())
    .bind(superseded_by_evidence_json)
    .execute(&mut **tx)
    .await?;
    Ok(updated.rows_affected() == 1)
}

async fn terminalize_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
    outcome: CiRouteOutcome,
    superseded_by_evidence_json: Option<&str>,
) -> Result<CiRouteAttempt> {
    terminalize_cas_in_tx(
        tx,
        subject,
        provider_action_key,
        outcome,
        superseded_by_evidence_json,
    )
    .await?;
    fetch_in_tx(tx, subject, provider_action_key)
        .await?
        .ok_or_else(|| DbError::Internal("terminalized route row disappeared".to_owned()))
}

async fn resolve_lease_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
    lease_id: &str,
    resolution: &CiTier2Resolution,
) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE ci_route_attempts \
         SET tier2_lease_state = 'resolved', tier2_resolved_at = now(), tier2_resolution = $5, \
             reopen_mode = $6, diagnostic_reason = $7, park_justification = $8, \
             lead_rejection = $9, updated_at = now() \
         WHERE {BY_PK} AND tier2_lease_id = $4"
    ))
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(provider_action_key)
    .bind(lease_id)
    .bind(resolution.outcome.as_str())
    .bind(resolution.reopen_mode.map(CiReopenMode::as_str))
    .bind(resolution.diagnostic_reason.map(CiDiagnosticReason::as_str))
    .bind(resolution.park_justification.as_deref())
    .bind(resolution.rejection.map(CiLeadRejection::as_str))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Why a Tier-2 lease request did or did not produce a lease.
enum Tier2Grant {
    Opened(String),
    /// This row has already used its one trip to Tier 2 (open or resolved).
    AlreadyRoutedToTier2,
    /// Another row on this subject holds the open lease for that key.
    KeyHeldElsewhere,
}

impl Tier2Grant {
    fn opened_id(self) -> Option<String> {
        match self {
            Self::Opened(id) => Some(id),
            _ => None,
        }
    }
}

/// Open the lease, or explain why not.
///
/// Two distinct refusals, deliberately not collapsed:
///
/// * **once-ever** — `tier2_lease_state IS NULL` in the UPDATE guard. The
///   proposal says failed or unknown provider evidence "may route **once** to
///   Tier 2", and a resolved lease must therefore not be reopenable. A guard
///   of `IS DISTINCT FROM 'open'` would give only *concurrent* uniqueness: an
///   `action_failed` row whose adjudication already landed could be re-leased,
///   re-adjudicated, and would carry a stale `tier2_resolution` while its
///   state flipped back to open.
/// * **mutual exclusion** — another row already holds the key. The partial
///   unique index is the real enforcement; this lookup exists so the loser
///   gets an outcome instead of a constraint violation.
async fn open_tier2_lease_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
    tier2_lease_key: &str,
    reason: CiTier2Reason,
) -> Result<Tier2Grant> {
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT provider_action_key FROM ci_route_attempts \
         WHERE subject_kind = $1 AND subject_id = $2 AND tier2_lease_key = $3 \
           AND tier2_lease_state = 'open' FOR UPDATE",
    )
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(tier2_lease_key)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((holder,)) = existing {
        return Ok(if holder == provider_action_key {
            Tier2Grant::AlreadyRoutedToTier2
        } else {
            Tier2Grant::KeyHeldElsewhere
        });
    }

    let lease_id = uuid::Uuid::now_v7().to_string();
    let updated = sqlx::query(&format!(
        "UPDATE ci_route_attempts \
         SET tier2_lease_id = $4, tier2_lease_key = $5, tier2_lease_state = 'open', \
             tier2_lease_reason = $6, tier2_leased_at = now(), updated_at = now() \
         WHERE {BY_PK} AND tier2_lease_state IS NULL"
    ))
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(provider_action_key)
    .bind(&lease_id)
    .bind(tier2_lease_key)
    .bind(reason.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(if updated.rows_affected() == 1 {
        Tier2Grant::Opened(lease_id)
    } else {
        // Not `KeyHeldElsewhere`: the key lookup above found nothing, so the
        // only way to miss here is this row's own lease having resolved.
        Tier2Grant::AlreadyRoutedToTier2
    })
}

async fn record_calling_recovery(
    tx: &mut Transaction<'_, Postgres>,
    subject: &CiRouteSubject,
    provider_action_key: &str,
    authority: &CiCallingRecoveryAuthority,
    reason: CiCallingRecoveryReason,
    cas_won: bool,
    outcome: Option<CiRouteOutcome>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ci_route_calling_recoveries \
           (id, subject_kind, subject_id, provider_action_key, former_owner_incarnation, \
            recovering_incarnation, holds_exclusive_lock, quiescence_proof, recovery_reason, \
            calling_recovery_timeout_secs, observed_calling_at, cas_won, resulting_outcome) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, \
                 (SELECT calling_at FROM ci_route_attempts \
                   WHERE subject_kind = $2 AND subject_id = $3 AND provider_action_key = $4), \
                 $11, $12)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(subject.kind.as_str())
    .bind(&subject.id)
    .bind(provider_action_key)
    .bind(&authority.former_owner_incarnation)
    .bind(&authority.recovering_incarnation)
    .bind(authority.holds_exclusive_lock)
    .bind(authority.quiescence_proof.as_str())
    .bind(reason.as_str())
    .bind(authority.calling_recovery_timeout_secs)
    .bind(cas_won)
    .bind(outcome.map(CiRouteOutcome::as_str))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Render an observed identity for the `superseded_by_evidence` audit column.
///
/// Recording *what defeated the compare-and-set* is not the same as making
/// that evidence authoritative — the obsolete row still performs no action.
fn identity_evidence_json(identity: &CiEvidenceIdentity) -> String {
    serde_json::json!({
        "observed": {
            "lane": identity.lane.as_str(),
            "pr_number": identity.pr_number,
            "pr_head_sha": identity.pr_head_sha,
            "run_id": identity.run_id,
            "run_head_sha": identity.run_head_sha,
            "dequeue_id": identity.dequeue_id,
        }
    })
    .to_string()
}
