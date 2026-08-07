//! The pure CI route classifier and route-decision contract (proposal `nafu`,
//! wave 2).
//!
//! Nothing in this module calls a provider, opens a lease, touches the board,
//! or dispatches a worker. It decides, from evidence the poller already
//! captured, which of five routes a terminal CI observation takes — and it
//! derives the four durable keys that make those routes idempotent across
//! polls, restarts, lanes, and subjects. Wave 3 wires the two lane
//! executors to it; wave 4 wires the Lead adjudication result.
//!
//! # The classification is closed, and Tier 1 is borrowed
//!
//! Tier 1 — "retry without spending a worker or a Lead session" — is exactly
//! [`ci_triage::is_inconclusive`]: a non-empty blocking set in which no check
//! carries causal evidence. This module deliberately defines no second
//! transience predicate. "Runner infrastructure" and "queue eviction" are not
//! machine classes here, and nothing branches on a migration framework, a
//! build tool, a job name, a repository, or an artifact type.
//!
//! Everything else in the table fails closed. One causal check vetoes
//! automation for the whole run. An empty blocking set on a run that
//! *concluded failure* is absence, not proof of transience. Incomplete
//! evidence is not evidence.
//!
//! The subject appears in the keys as a *namespace*, never as a branch: no
//! route decision differs because of which project or repository produced the
//! evidence. Keying and branching are different things, and only the second is
//! the deny list the proposal rules out.
//!
//! # Why the keys are computed here and not in `djinn-db`
//!
//! The repository stores the keys; it cannot derive them, because their scope
//! is a coordinator fact. Three of the four are aggregate keys whose whole job
//! is to make *different* evidence share one budget, so only the caller knows
//! which fields to leave out.
//!
//! The scope that matters most is the one the proposal's `CiEvidenceId` does
//! not name. A PR number is unique only within one repository, and this
//! coordinator serves every project, so a globally-scoped key would let
//! `pr_head/#41/abc…` in one project collide with `pr_head/#41/abc…` in
//! another — on the provider-action key (suppressing a legitimate call as a
//! "duplicate"), on both budget counters, and on the Tier-2 lease. Every key
//! here therefore begins with the durable [`CiRouteSubject`], and every field
//! inside is length-prefixed before hashing so that no two distinct field
//! vectors can render to the same pre-image.
//!
//! # The four keys
//!
//! | Key | Scope | Invariant it serves |
//! | --- | --- | --- |
//! | [`provider_action_key`] | subject + full evidence identity + action | at most one provider-call episode per evidence |
//! | [`retry_budget_key`] | subject + lane + PR + PR-head SHA + transient fingerprint | two charged actions per equivalent failure signature |
//! | [`head_budget_key`] | subject + PR + PR-head SHA, **no lane** | four charged actions per subject's PR head, across both lanes |
//! | [`tier2_lease_key`] | subject + PR + PR-head SHA, **no lane** | at most one Lead adjudication in flight per subject's PR head |
//!
//! [`tier2_lease_key`] excluding the lane is load-bearing and is the reason it
//! is not simply the retry-budget key. Scoped per lane, a PR head whose
//! `pr_head` route and `merge_group` route both went unknown could hold two
//! concurrent Lead adjudications for the same head — which is precisely the
//! retry storm the proposal's head-level hold exists to prevent. It also
//! excludes the run id and the dequeue id for the same reason: a second
//! failing run on an unchanged head must join the existing hold, not open a
//! second one.
//!
//! # The head-level hold is per SUBJECT, not per PR head
//!
//! State the limit plainly, because the proposal states the safeguard at head
//! level and the implementation cannot reach that. The lease's unique index is
//! `(subject_kind, subject_id, tier2_lease_key)` and the budget counters' key
//! is `(subject_kind, subject_id, scope, counter_key)`. Excluding the lane from
//! the lease key therefore buys the cross-lane hold *within one subject* — a
//! task's `pr_head` and `merge_group` routes genuinely contend for one lease —
//! but two different subjects observing the same PR head each get their own
//! lease and their own head counter.
//!
//! Today that is unreachable: one PR head belongs to one task, so subject and
//! head are in bijection. It stops being unreachable the moment a `proposal`
//! subject can share a PR head with a task subject, at which point the head
//! ceiling doubles to eight charged actions and two Lead adjudications may run
//! for one head. Nothing here can fix that — the scope is the index's, not the
//! key's — so nothing here is written to depend on a global per-head hold.

use std::collections::BTreeSet;

use djinn_provider::github_api::{CheckRun, CheckSetCompleteness, CheckSetIncompleteReason};
use sha2::{Digest, Sha256};

use crate::types::{ProviderActionGuard, ProviderActionScope};

use super::ci_triage::{self, CheckEvidence};

pub(crate) use djinn_db::{
    CiAction, CiActionPhase, CiBudgetCounts, CiClass, CiEvidenceIdentity, CiLane,
    CiReservedRecovery, CiRouteAttempt, CiRouteOutcome, CiRouteSubject, CiTier2Reason,
};
// Read only by `budget_ceilings`, which exists so a test can assert the numbers
// the routing contract enforces are the numbers the repository charges against.
#[cfg(test)]
pub(crate) use djinn_db::{CI_HEAD_BUDGET_LIMIT, CI_SIGNATURE_BUDGET_LIMIT};

// ---------------------------------------------------------------------------
// Subject scope
// ---------------------------------------------------------------------------
//
// The scope is `djinn_db::CiRouteSubject` — `(kind, id)`, where a task subject
// carries the task's id. It is consumed rather than redefined, and it is also
// the physical scope of the primary key, the budget-counter primary key, and
// the partial unique index behind the Tier-2 lease.
//
// That structural scope is what discharges the cross-project collision
// obligation: the proposal's `CiEvidenceId` has no repository field and a PR
// number is unique only within one repository, so a globally-scoped key would
// let one project's evidence answer `AlreadyPresent` for another's — a route
// that silently never exists.
//
// An earlier draft of this module folded the GitHub `owner/repo` pair into
// every key as a second scope. That is deliberately NOT done, and the reason
// is worth recording: `owner/repo` is *mutable*. A repository rename mid-flight
// would change every derived key, orphan an in-flight `reserved` or `calling`
// row, and authorize a second provider-call episode for evidence that already
// had one — the precise failure the action key exists to prevent. A subject id
// is immutable for the life of the route, so it is the correct scope and the
// mutable name is not folded in anywhere.

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------

/// Key-schema version. Bumping it invalidates every derived key at once,
/// which is the only safe way to change a key's field set: a silent change
/// would make in-flight `reserved` and `calling` rows unfindable and permit a
/// second provider-call episode for evidence that already had one.
const KEY_SCHEMA: &str = "nafu1";

/// Accumulates length-prefixed fields into an unambiguous hash pre-image.
///
/// `field("ab").field("c")` and `field("a").field("bc")` must not produce the
/// same bytes, or two distinct field vectors would share one key. Length
/// prefixing is what makes that impossible; a separator character alone is
/// not, because separators occur in refs, SHAs, and fingerprints.
struct KeyPreimage {
    buf: String,
}

impl KeyPreimage {
    fn new(tag: &str) -> Self {
        let mut me = Self { buf: String::new() };
        me.push(KEY_SCHEMA);
        me.push(tag);
        me
    }

    fn push(&mut self, field: &str) {
        self.buf.push_str(&field.len().to_string());
        self.buf.push(':');
        self.buf.push_str(field);
        self.buf.push('|');
    }

    fn field(mut self, field: &str) -> Self {
        self.push(field);
        self
    }

    fn num(self, value: i64) -> Self {
        self.field(&value.to_string())
    }

    /// `None` and `Some("")` must not collide: an absent dequeue id is a
    /// different fact from an empty one.
    fn opt(self, value: Option<&str>) -> Self {
        match value {
            Some(v) => self.field("some").field(v),
            None => self.field("none"),
        }
    }

    /// `tag:<64 hex>` — 70 characters, comfortably inside the durable
    /// `VARCHAR(128)` columns while leaving the tag readable in a `psql` dump.
    fn finish(self, tag: &str) -> String {
        let digest = Sha256::digest(self.buf.as_bytes());
        format!("{tag}:{}", hex::encode(digest))
    }
}

/// Every key begins with the subject, so a key read out of context — in a log
/// line, a metrics label, a `psql` dump — is still self-scoping even though
/// the columns beside it already carry the scope.
fn subject_preimage(tag: &str, subject: &CiRouteSubject) -> KeyPreimage {
    KeyPreimage::new(tag)
        .field(subject.kind.as_str())
        .field(&subject.id)
}

/// Per-evidence provider-action identity.
///
/// Includes every field of the immutable evidence identity plus the action, so
/// a distinct later run gets a distinct key and may consume the next budget
/// slot, while a duplicate poll or a restart lands on the existing row.
pub(crate) fn provider_action_key(
    subject: &CiRouteSubject,
    identity: &CiEvidenceIdentity,
    action: CiAction,
) -> String {
    let tag = "pa";
    subject_preimage(tag, subject)
        .field(identity.lane.as_str())
        .num(identity.pr_number)
        .field(&identity.pr_head_sha)
        .num(identity.run_id)
        .field(&identity.run_head_sha)
        .opt(identity.dequeue_id.as_deref())
        .field(action.as_str())
        .finish(tag)
}

/// Aggregate signature budget identity: lane + PR + PR-head SHA + transient
/// fingerprint.
///
/// Excludes the run id and the dequeue id so that equivalent later evidence —
/// a second run on the same head failing the same way — shares one budget
/// instead of starting a fresh one.
pub(crate) fn retry_budget_key(
    subject: &CiRouteSubject,
    identity: &CiEvidenceIdentity,
    transient_fingerprint: &str,
) -> String {
    let tag = "sig";
    subject_preimage(tag, subject)
        .field(identity.lane.as_str())
        .num(identity.pr_number)
        .field(&identity.pr_head_sha)
        .field(transient_fingerprint)
        .finish(tag)
}

/// Aggregate head budget identity: PR + PR-head SHA, shared across both lanes.
pub(crate) fn head_budget_key(
    subject: &CiRouteSubject,
    pr_number: i64,
    pr_head_sha: &str,
) -> String {
    let tag = "head";
    subject_preimage(tag, subject)
        .num(pr_number)
        .field(pr_head_sha)
        .finish(tag)
}

/// The current-evidence Tier-2 hold: PR + PR-head SHA, **no lane, no run id,
/// no dequeue id**.
///
/// Head scope, not lane scope. See the module docs: lane scope would permit
/// two concurrent Lead adjudications for one PR head — one from the `pr_head`
/// route and one from the `merge_group` route — and the proposal's retry-storm
/// safeguard is explicitly *"a head-level hold while current provider failure
/// or unknown outcome is adjudicated"*.
pub(crate) fn tier2_lease_key(
    subject: &CiRouteSubject,
    pr_number: i64,
    pr_head_sha: &str,
) -> String {
    let tag = "t2";
    subject_preimage(tag, subject)
        .num(pr_number)
        .field(pr_head_sha)
        .finish(tag)
}

/// The transient fingerprint: a stable hash of the sorted, normalized tuples
/// `(check name, conclusion, ci_triage evidence class)` plus the lane.
///
/// The evidence class comes from [`ci_triage::check_evidence`] rather than
/// being recomputed here — the same ranking that decides Tier 1 decides what
/// "the same kind of failure" means, so the budget cannot drift from the
/// classifier.
///
/// Sorted and deduplicated, so check ordering from the provider cannot split
/// one signature into two budgets.
pub(crate) fn transient_fingerprint(lane: CiLane, blocking: &[&CheckRun]) -> String {
    let tuples: BTreeSet<String> = blocking
        .iter()
        .map(|cr| {
            format!(
                "{}\u{1f}{}\u{1f}{}",
                cr.name.trim().to_lowercase(),
                cr.conclusion.as_deref().unwrap_or("none"),
                evidence_class_name(ci_triage::check_evidence(cr)),
            )
        })
        .collect();

    let tag = "fp";
    let mut pre = KeyPreimage::new(tag).field(lane.as_str());
    for tuple in &tuples {
        pre.push(tuple);
    }
    pre.finish(tag)
}

fn evidence_class_name(evidence: CheckEvidence) -> &'static str {
    match evidence {
        CheckEvidence::Causal => "causal",
        CheckEvidence::RanThenCancelled => "ran_then_cancelled",
        CheckEvidence::NeverExecuted => "never_executed",
    }
}

// ---------------------------------------------------------------------------
// Captured evidence
// ---------------------------------------------------------------------------

/// Why a snapshot is not yet classifiable at all.
///
/// Distinct from [`CiIncompleteReason`]: a pending run will resolve itself on
/// a later poll, so the route holds and spends nothing. Incomplete evidence
/// on a *terminal* run will not resolve itself, so it fails closed to Lead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiPendingReason {
    /// The workflow run has not reached a terminal status.
    RunNonTerminal,
    /// At least one required/blocking check is still `queued` or `in_progress`.
    RequiredCheckNonTerminal,
}

/// The closed set of ways a *terminal* run's evidence can be incomplete or
/// unknown. Every one of these fails closed: no provider mutation, and Lead
/// adjudicates only if the identity is still current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiIncompleteReason {
    /// A blocking check has no `started_at`, so its execution cannot be
    /// verified.
    MissingStartTimestamp,
    /// A blocking check has no `completed_at`.
    MissingCompletionTimestamp,
    /// Both timestamps exist but are not mutually comparable (mixed offset
    /// forms or differing widths), so the interval is meaningless.
    MalformedExecutionInterval,
    /// A blocking check reports a *hard failure* conclusion (`failure` /
    /// `timed_out`) together with a non-positive execution interval. The
    /// conclusion says the lane judged its own work; the interval says it
    /// never ran. Contradictory evidence cannot authorize an automatic
    /// retry — this is the "false transient" the proposal names as risk #1.
    NonPositiveExecutionInterval,
    /// A page of the check-run enumeration returned a non-success status, so
    /// the collected list is a prefix — possibly the empty prefix, when page 1
    /// is the one that failed.
    EnumerationPageFailed,
    /// The enumeration walk hit its page ceiling, so the check list could not
    /// be enumerated in full.
    CheckEnumerationUnavailable,
    /// The enumeration returned fewer runs than the provider's own
    /// `total_count`, so the blocking set may be missing a causal member.
    PartialPagination,
    /// The check API returned an error for a ref whose immutable run identity
    /// is already known — a *complete-but-unusable* case, not an enumeration
    /// that can be re-polled into completeness.
    CheckApiError,
    /// A log/annotation read that completeness depended on failed, again after
    /// an immutable run is known.
    LogApiError,
    /// More than one terminal merge-group run correlates to this PR.
    AmbiguousMergeGroupCorrelation,
    /// No terminal merge-group run correlates to this PR.
    MergeGroupCorrelationUnavailable,
    /// A blocking check could not be attributed to any Actions run, so no
    /// per-run evidence set can be proven complete — and `rerun_failed_jobs`
    /// has no run to act on.
    RunAttributionUnavailable,
}

impl CiIncompleteReason {
    /// Whether the *enumeration itself* failed, as opposed to the enumeration
    /// succeeding and its contents being unusable.
    ///
    /// This predicate is the difference between two rows of the proposal's
    /// table that both look like "we don't know":
    ///
    /// * **Enumeration failure → hold, with no route row.** The set we hold is
    ///   not an immutable CI evidence bundle at all; a later poll can turn it
    ///   into one for free. Spending a route row, a Tier-2 lease, and a Lead
    ///   session on it would mean paying for adjudication of evidence that does
    ///   not exist yet — and, because incomplete polls repeat, paying again on
    ///   every poll until the provider recovers.
    /// * **Complete but unusable → Tier 2 after the current-identity guard.**
    ///   Here the evidence *is* an immutable bundle with a constructible
    ///   run/lane identity; it is simply one no automatic action can read. That
    ///   has a deduplicated adjudication and will not fix itself.
    ///
    /// Note which side the two API-error reasons sit on. `CheckApiError` and
    /// `LogApiError` are Tier 2 because the proposal scopes them to "after an
    /// immutable run is known" — the run identity survives the error, so the
    /// route can be keyed. A page failure during enumeration has no run yet.
    ///
    /// # `CheckEnumerationUnavailable` is on the *other* side, and that is a fix
    ///
    /// It used to sit here with the other two, and that made it a permanent
    /// wedge. A hold's whole premise is that "a later poll can turn this into an
    /// evidence bundle for free" — true of a failed page (a provider incident)
    /// and of a short read (the provider disagreeing with itself), and false of
    /// pagination truncation. Truncation means the PR genuinely has more check
    /// runs than `MAX_PAGES * PER_PAGE`, which is a property of the PR, not of
    /// the moment: every subsequent enumeration returns the identical verdict,
    /// so the route holds on every poll forever, with no route row, no
    /// adjudication, and nothing on the board to say why the task stopped.
    ///
    /// It is instead complete-but-unusable current evidence: a real enumeration
    /// whose contents no automatic action can read. That is the guarded Tier-2
    /// row of the proposal's table, and it is bounded — the head-scoped lease
    /// makes it exactly one Lead adjudication per PR head, not one per poll.
    pub(crate) fn is_enumeration_failure(self) -> bool {
        matches!(self, Self::EnumerationPageFailed | Self::PartialPagination)
    }

    /// Whether the reason means **no run was ever named** — as distinct from
    /// an enumeration that failed, and from several runs being named at once.
    ///
    /// This is the second hold predicate, and it is deliberately *not* folded
    /// into [`Self::is_enumeration_failure`]: neither of these is an
    /// enumeration failure (the enumeration finished perfectly), so putting
    /// them there would make that predicate's name a lie and would corrupt the
    /// report, which distinguishes "the provider did not finish telling us"
    /// from "the provider told us and there was nothing to point at".
    ///
    /// # Why they must hold rather than take the guarded Tier-2 route
    ///
    /// AC5 admits to Tier 2 "the closed complete-but-unusable cases **with a
    /// constructible immutable run/lane identity**". These two have neither:
    ///
    /// * `MergeGroupCorrelationUnavailable` — zero terminal merge-group runs
    ///   correlate to this PR. Nothing was named, so there is no run identity,
    ///   and a later poll can produce one for free once the queue run appears.
    /// * `RunAttributionUnavailable` — a blocking check belongs to no Actions
    ///   run we can name, so there is no run identity *and* `rerun_failed_jobs`
    ///   has nothing to act on.
    ///
    /// Routing them to Tier 2 anyway is what forced the lane executor to
    /// fabricate `run_id: 0` / `dequeue_id: None` for the route row's
    /// provider-action key — collapsing two distinct observations of the same
    /// PR head onto one key, so the second resolved `AlreadyPresent` and never
    /// got a route at all. AC9 and AC14 forbid that synthetic identity by name,
    /// and the fix is to stop creating the row, not to invent the fields.
    ///
    /// # Why `AmbiguousMergeGroupCorrelation` is deliberately NOT here
    ///
    /// The proposal body puts it on the Tier-2 side explicitly — "missing or
    /// malformed execution timestamps, check/log API error after immutable run
    /// identification, or ambiguous merge-group correlation follows the guarded
    /// Tier-2 route". Ambiguity is *several* runs named, not none, and its
    /// lane identity (lane, PR number, PR head SHA, **and the real dequeue id**)
    /// is constructible without fabricating anything that distinguishes one
    /// dequeue from another. Its `run_id` stays `0` because no single run
    /// exists to name — that residual is enumerated explicitly in
    /// `ci_routing::tests`, not hidden.
    pub(crate) fn no_run_was_named(self) -> bool {
        matches!(
            self,
            Self::MergeGroupCorrelationUnavailable | Self::RunAttributionUnavailable
        )
    }
}

/// What the poller managed to capture for one evidence identity.
///
/// # Why this is a newtype and not a plain enum
///
/// The dangerous variant is the complete one. `is_inconclusive` is true of a
/// blocking set in which no member carries causal evidence — and a check that
/// is still `in_progress` carries no causal evidence *yet*. So a caller that
/// hand-builds "complete" from a set of running checks gets `NeverExecuted` for
/// each, `is_inconclusive` returns true, and the classifier hands back a
/// Tier-1 provider authorization for a run that has not finished. Nothing in
/// the type system stopped that while the variants were public.
///
/// They are private now. [`CiCapture::prove_complete`] is the only door to a
/// complete capture, it takes the provider's own enumeration verdict as an
/// argument, and it downgrades to `NonTerminal` or `Incomplete` on its own
/// authority. "Asserts no Tier 1 on a non-terminal run" is therefore a property
/// of the constructor rather than a claim about a call site.
///
/// # The three shapes of "nothing blocking"
///
/// | Shape | Capture | Route |
/// | --- | --- | --- |
/// | run concluded green | [`CiCapture::passing`] | existing passing path |
/// | enumeration provably complete, blocking set empty | `CompleteEmpty` | lane no-CI compatibility path |
/// | enumeration could not be proven complete | `Incomplete` | hold and re-poll |
///
/// Rows two and three are byte-identical at the collected-runs level — a failed
/// first page returns zero runs with `total_count: 0`. Only the provider's
/// verdict separates them, which is why it is an argument here rather than
/// something re-derived from the list.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CiCapture<'a>(Capture<'a>);

#[derive(Clone, Copy, Debug)]
enum Capture<'a> {
    /// Do not classify yet.
    NonTerminal(CiPendingReason),
    /// The run concluded successfully with the complete required set green.
    Passing,
    /// The PR/task already merged.
    Merged,
    /// The enumeration is provably complete and the blocking set is empty.
    CompleteEmpty,
    /// A terminal failing run whose complete, non-empty blocking set is
    /// `blocking`.
    FailedComplete { blocking: &'a [&'a CheckRun] },
    /// A terminal failing run whose evidence set could not be completed.
    Incomplete(CiIncompleteReason),
}

impl<'a> CiCapture<'a> {
    /// Nothing terminal to classify yet.
    pub(crate) fn non_terminal(reason: CiPendingReason) -> Self {
        Self(Capture::NonTerminal(reason))
    }

    /// The run concluded green.
    pub(crate) fn passing() -> Self {
        Self(Capture::Passing)
    }

    /// The PR/task already merged.
    pub(crate) fn merged() -> Self {
        Self(Capture::Merged)
    }

    /// Terminal, but the evidence could not be completed. Fails closed.
    pub(crate) fn incomplete(reason: CiIncompleteReason) -> Self {
        Self(Capture::Incomplete(reason))
    }

    /// The incompleteness reason, when this capture is incomplete.
    pub(crate) fn incomplete_reason(&self) -> Option<CiIncompleteReason> {
        match self.0 {
            Capture::Incomplete(reason) => Some(reason),
            _ => None,
        }
    }

    /// Whether this capture is the authoritatively complete *empty* one.
    #[cfg(test)]
    pub(crate) fn is_complete_empty(&self) -> bool {
        matches!(self.0, Capture::CompleteEmpty)
    }

    /// The **only** constructor that can yield a complete capture.
    ///
    /// It discharges four obligations in a fixed order, and the order is the
    /// contract:
    ///
    /// 1. **The enumeration is provably complete.** Taken from the provider's
    ///    own verdict, never inferred from the collected list — the list cannot
    ///    tell a failed first page from a repository with no CI.
    /// 2. **Every blocking member is terminal.** This is the seal's whole
    ///    point: a running check is not causal *yet*, and treating "not causal
    ///    yet" as "inconclusive" is a provider retry on a live run.
    /// 3. **An empty blocking set is complete-empty**, not a failure with no
    ///    explanation. It leaves remediation routing entirely.
    /// 4. **The execution evidence is usable** — timestamps present, mutually
    ///    comparable, and not contradicting a hard-failure conclusion.
    ///
    /// (3) sits before (4) deliberately: an empty set trivially satisfies (4),
    /// and running the timestamp checks on it would only obscure which row of
    /// the proposal's table it took.
    pub(crate) fn prove_complete(
        enumeration: CheckSetCompleteness,
        blocking: &'a [&'a CheckRun],
    ) -> Self {
        if let Some(reason) = enumeration.incomplete_reason() {
            return Self(Capture::Incomplete(enumeration_incomplete_reason(reason)));
        }
        if blocking.iter().any(|cr| cr.status != "completed") {
            return Self(Capture::NonTerminal(
                CiPendingReason::RequiredCheckNonTerminal,
            ));
        }
        if blocking.is_empty() {
            return Self(Capture::CompleteEmpty);
        }
        if let Some(reason) = blocking_evidence_completeness(blocking) {
            return Self(Capture::Incomplete(reason));
        }
        Self(Capture::FailedComplete { blocking })
    }
}

/// Map the provider's enumeration verdict onto the classifier's closed reason
/// set.
///
/// The three provider reasons are kept distinct here rather than folded into
/// one `PartialPagination`, because they answer different operational
/// questions: a failed page is a provider incident, a truncated walk is a PR
/// that outgrew the ceiling, and a short read is the provider disagreeing with
/// itself.
///
/// They do **not** route identically, and the split is two-way:
///
/// * `PageFetchFailed` → `EnumerationPageFailed` and `ShortRead` →
///   `PartialPagination` are enumeration failures
///   ([`CiIncompleteReason::is_enumeration_failure`]), so they **hold** and
///   write no route row. A later poll can turn either into a real evidence
///   bundle for free.
/// * `MaxPagesTruncated` → `CheckEnumerationUnavailable` is deliberately *not*
///   an enumeration failure, so it does **not** hold: truncation is a property
///   of the PR rather than of the moment, so every subsequent enumeration
///   returns the identical verdict and a hold is a permanent wedge with nothing
///   on the board to explain it. See the rationale on
///   [`CiIncompleteReason::is_enumeration_failure`] — and note it is the one
///   route row still keyed on a placeholder `run_id`, which is a known
///   deviation from the proposal's table under separate review.
fn enumeration_incomplete_reason(reason: CheckSetIncompleteReason) -> CiIncompleteReason {
    match reason {
        CheckSetIncompleteReason::PageFetchFailed => CiIncompleteReason::EnumerationPageFailed,
        CheckSetIncompleteReason::MaxPagesTruncated => {
            CiIncompleteReason::CheckEnumerationUnavailable
        }
        CheckSetIncompleteReason::ShortRead => CiIncompleteReason::PartialPagination,
    }
}

/// Whether the blocking set's execution evidence is complete enough to route
/// automatically.
///
/// Checked in a fixed order so the answer does not depend on how the provider
/// ordered the check list:
///
/// 1. a missing `started_at`, then
/// 2. a missing `completed_at`, then
/// 3. two timestamps that exist but are not mutually comparable, then
/// 4. a **hard-failure conclusion with a non-positive interval**.
///
/// (4) deserves its own note. [`ci_triage::executed`] deliberately treats a
/// non-positive interval as proof of non-execution, which is what makes a
/// `needs:`-sealed aggregator sort last and what makes a run of them
/// *inconclusive*. That tolerance is correct for ranking and is left exactly as
/// it is. But a lane that reports `failure`/`timed_out` — a verdict about its
/// own work — while also reporting that it never ran is contradicting itself,
/// and routing that to an automatic retry is the proposal's "false transient"
/// risk. So the contradiction, and only the contradiction, fails closed to
/// Lead. A plain `cancelled` lane with a non-positive interval stays exactly
/// what `ci_triage` says it is: never executed, and Tier-1 eligible.
pub(crate) fn blocking_evidence_completeness(blocking: &[&CheckRun]) -> Option<CiIncompleteReason> {
    if blocking.iter().any(|cr| cr.started_at.is_none()) {
        return Some(CiIncompleteReason::MissingStartTimestamp);
    }
    if blocking.iter().any(|cr| cr.completed_at.is_none()) {
        return Some(CiIncompleteReason::MissingCompletionTimestamp);
    }
    if blocking
        .iter()
        .any(|cr| ci_triage::completed_after_start(cr).is_none())
    {
        return Some(CiIncompleteReason::MalformedExecutionInterval);
    }
    if blocking.iter().any(|cr| {
        super::ci_helpers::is_hard_failure_conclusion(cr.conclusion.as_deref())
            && ci_triage::completed_after_start(cr) == Some(false)
    }) {
        return Some(CiIncompleteReason::NonPositiveExecutionInterval);
    }
    None
}

/// Which component of the immutable identity diverged, reported in declaration
/// order so a single diverged field is named deterministically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiStaleField {
    Lane,
    PrNumber,
    PrHeadSha,
    RunId,
    RunHeadSha,
    DequeueId,
}

/// The first identity component on which stored and observed disagree, or
/// `None` when the evidence is still current.
///
/// [`CiEvidenceIdentity::matches`] already answers the yes/no question; this
/// answers *which field*, which is what the durable supersession record and
/// the stale-suppression report need.
pub(crate) fn stale_field(
    stored: &CiEvidenceIdentity,
    observed: &CiEvidenceIdentity,
) -> Option<CiStaleField> {
    if stored.lane != observed.lane {
        return Some(CiStaleField::Lane);
    }
    if stored.pr_number != observed.pr_number {
        return Some(CiStaleField::PrNumber);
    }
    if stored.pr_head_sha != observed.pr_head_sha {
        return Some(CiStaleField::PrHeadSha);
    }
    if stored.run_id != observed.run_id {
        return Some(CiStaleField::RunId);
    }
    if stored.run_head_sha != observed.run_head_sha {
        return Some(CiStaleField::RunHeadSha);
    }
    if stored.dequeue_id != observed.dequeue_id {
        return Some(CiStaleField::DequeueId);
    }
    None
}

/// One classification input: the evidence being classified, the identity the
/// poller observes as current right now, and the capture.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CiObservation<'a> {
    pub evidence: &'a CiEvidenceIdentity,
    pub observed_current: &'a CiEvidenceIdentity,
    pub capture: CiCapture<'a>,
}

// ---------------------------------------------------------------------------
// Route decision
// ---------------------------------------------------------------------------

/// A provider mutation this route is authorized to perform.
///
/// Deliberately unconstructible outside this module: [`CiRouteDecision`] is
/// the only source of one, and only the Tier-1 branch produces it. A wave-3
/// lane executor that cannot obtain this value cannot call GitHub, which is
/// what makes "asserts no provider mutation" a structural property of the
/// classifier rather than a claim about a log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CiProviderAction {
    kind: CiProviderActionKind,
    /// The Actions run id for `rerun_failed_jobs`. Carried for both kinds so
    /// the merge-group re-enqueue can name the run it is re-entering for.
    run_id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiProviderActionKind {
    /// `rerun_failed_jobs(owner, repo, run_id)` — the PR-head lane.
    RerunFailedJobs,
    /// `enable_auto_merge(...)` — the merge-group lane's queue re-entry.
    EnableAutoMerge,
}

impl CiProviderAction {
    /// Admit this action into the leader's provider-action scope.
    ///
    /// `None` means admission is closed — leadership is winding down — and the
    /// caller must not call the provider **or** advance its route row to
    /// `calling`. A charged `calling` row with no future behind it is worse
    /// than no row at all: it holds its slots and nothing but startup recovery
    /// will ever finalize it.
    ///
    /// The returned value is the only thing that names a *call target*, and it
    /// owns the scope guard for as long as it lives. That is what makes "no
    /// provider call escapes the tracked scope" a property of the type rather
    /// than a convention every lane executor has to remember: a call site that
    /// skipped the scope would have nothing to pass to the provider.
    ///
    /// # What this actually guarantees, and what it does not
    ///
    /// The accessor route is closed. `CiProviderAction` used to expose `kind()`
    /// and `run_id()` as `pub(crate)`, so a sibling module could read the call
    /// target straight off the *unadmitted* value; both now live on
    /// [`AdmittedCiProviderAction`] alone, and `action.run_id()` no longer
    /// compiles. That removes the accidental path — the one a reader would
    /// reach for — and it is worth having.
    ///
    /// **It is not a proof, and this comment previously claimed it was.** The
    /// executor is handed a `CiEvidenceIdentity` whose `run_id` and `lane` are
    /// public fields on the `djinn-db` type, and `CiRouteDecision::provider_action`
    /// is `pub(crate)`. A sibling module that reads the authorization bit and
    /// then calls `provider.rerun_failed_jobs(owner, repo, identity.run_id as
    /// u64)` with no scope and no guard compiles cleanly today. Closing that
    /// would mean making the identity's fields private across `djinn-db`, which
    /// is a wave-1 change with a large blast radius and is not attempted here.
    ///
    /// So: "no provider call escapes the tracked scope" is enforced by this
    /// type for the obvious route, and by review for the rest. The fixtures
    /// assert the *observable* property instead — a closed scope performs no
    /// provider mutation, counted on the seam — which does not depend on the
    /// stronger claim.
    pub(crate) fn admit(&self, scope: &ProviderActionScope) -> Option<AdmittedCiProviderAction> {
        scope.admit().map(|guard| AdmittedCiProviderAction {
            kind: self.kind,
            run_id: self.run_id,
            _guard: guard,
        })
    }
}

/// A provider action that holds a live scope guard.
///
/// Deliberately not `Clone` and with no public constructor: it exists only
/// between [`CiProviderAction::admit`] and the end of the provider call, and
/// dropping it is what leaves the scope.
#[derive(Debug)]
pub(crate) struct AdmittedCiProviderAction {
    kind: CiProviderActionKind,
    run_id: i64,
    _guard: ProviderActionGuard,
}

impl AdmittedCiProviderAction {
    pub(crate) fn kind(&self) -> CiProviderActionKind {
        self.kind
    }

    /// The Actions run id to call the provider with.
    pub(crate) fn run_id(&self) -> i64 {
        self.run_id
    }
}

/// Why a route decided what it decided. Durable reporting reads this; the
/// classifier's tests assert on it rather than on prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiRouteRationale {
    /// Nothing terminal to classify yet.
    Pending(CiPendingReason),
    /// The identity is obsolete. Named field for the supersession record.
    Stale(CiStaleField),
    /// Every blocking check was cancelled or never executed.
    Inconclusive,
    /// At least one blocking check carries causal evidence.
    CausalFailure,
    /// The enumeration was provably complete and nothing blocking was in it.
    /// Lane-specific compatibility behaviour; creates no remediation state.
    EmptyBlockingSet,
    /// Terminal run, incomplete evidence.
    IncompleteEvidence(CiIncompleteReason),
    /// Tier 1 was the classification, but a budget ceiling is spent.
    BudgetExhausted,
    // There is deliberately no `ProviderActionFailed` rationale either. A failed
    // provider call is a durable fact — `finalize_calling` writes
    // `CiRouteOutcome::ActionFailed` and the executor then opens the Tier-2
    // lease with `CiTier2Reason::ProviderActionFailed` — and nothing classifies
    // it. Its decision-shaped mirror was constructible only by
    // `provider_failure_route`, whose result was discarded; see the note where
    // that function used to live.
    // There is deliberately no `ProviderOutcomeUnknown` rationale. A `calling`
    // row that was legally handed off *is* a real route state, but it is a
    // durable one: `recover_calling_owner` writes
    // `CiRouteOutcome::OutcomeUnknown` and opens the lease in one transaction,
    // and nothing classifies it. A decision-shaped mirror of that fact existed
    // here, was constructible only by a function nothing called, and carried a
    // comment claiming "wave 4 consumes it" that was never true — so it is
    // deleted rather than kept as an unenforced second source of truth.
    /// A newer passing observation closes the route.
    NewerPass,
    /// A merged PR closes the route.
    NewerMerge,
}

/// The route decision for one observation.
///
/// Constructible only by this module's classifiers. The accessors below are
/// the complete set of authorizations a decision carries; there is no other
/// door.
/// The lane-specific behaviour an authoritatively complete *empty* enumeration
/// takes.
///
/// The two lanes genuinely differ in this repository and the proposal (rev 49)
/// is explicit that they must not be collapsed:
///
/// * the draft lane already maps a no-CI PR to `PrDraftCiAction::Proceed` once
///   the minimum-age guard has elapsed, and
/// * the review lane already persists `Passing` for the current head, because a
///   `pr_review` PR has *already* cleared that guard and would otherwise wedge
///   forever on a repository with no CI (`Unknown` maps to `Hold` at the gate).
///
/// Neither creates a route row, a Tier-2 lease, a Lead or worker session, a
/// provider remediation action, or a Tier-1 charge. This value exists so a lane
/// executor is told which existing path to take rather than inventing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiCompleteEmptyRoute {
    /// `pr_draft`: existing `PrDraftCiAction::Proceed`, after the minimum-age
    /// guard.
    PrHeadProceed,
    /// `pr_review`: persist current-head `Passing`, let the existing merge gate
    /// progress.
    MergeGroupRecordPassing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CiRouteDecision {
    class: CiClass,
    action: CiAction,
    rationale: CiRouteRationale,
    provider_action: Option<CiProviderAction>,
    tier2_reason: Option<CiTier2Reason>,
    complete_empty: Option<CiCompleteEmptyRoute>,
    creates_route_row: bool,
    closes_route: bool,
}

impl CiRouteDecision {
    pub(crate) fn class(&self) -> CiClass {
        self.class
    }

    pub(crate) fn action(&self) -> CiAction {
        self.action
    }

    pub(crate) fn rationale(&self) -> CiRouteRationale {
        self.rationale
    }

    /// The **only** authorization to mutate provider state. `None` for every
    /// hold, discard, Tier-2, passing, and merged route.
    pub(crate) fn provider_action(&self) -> Option<&CiProviderAction> {
        self.provider_action.as_ref()
    }

    /// The **only** authorization to open a Tier-2 lease and dispatch Lead.
    pub(crate) fn tier2_reason(&self) -> Option<CiTier2Reason> {
        self.tier2_reason
    }

    #[cfg(test)]
    pub(crate) fn opens_tier2_lease(&self) -> bool {
        self.tier2_reason.is_some()
    }

    /// The lane compatibility path for an authoritatively complete empty
    /// enumeration. `None` for every other route.
    pub(crate) fn complete_empty_route(&self) -> Option<CiCompleteEmptyRoute> {
        self.complete_empty
    }

    /// Whether this decision may write a `ci_route_attempts` row at all.
    ///
    /// False for holds, discards, complete-empty, passing, and merged. It is
    /// the single predicate the "no route row" half of every negative-space
    /// assertion reads, so those assertions cannot drift apart from the
    /// executor.
    pub(crate) fn creates_route_row(&self) -> bool {
        self.creates_route_row
    }

    /// Whether this decision consumes a Tier-1 retry charge. Only a decision
    /// carrying a provider authorization can.
    #[cfg(test)]
    pub(crate) fn consumes_tier1_charge(&self) -> bool {
        self.provider_action.is_some()
    }

    /// True for the two routes that mean "an existing route row, if any, is
    /// closed by a newer authoritative outcome".
    pub(crate) fn closes_route(&self) -> bool {
        self.closes_route
    }

    /// A classification decision never transitions the board and never
    /// dispatches a worker. Board transitions in this contract come from
    /// exactly one place — a resolved Tier-2 lease carrying a validated Lead
    /// result (wave 4) — and this accessor exists so that invariant is
    /// asserted by tests rather than assumed.
    #[cfg(test)]
    pub(crate) fn authorizes_board_transition(&self) -> bool {
        false
    }

    /// See [`Self::authorizes_board_transition`].
    #[cfg(test)]
    pub(crate) fn authorizes_worker_dispatch(&self) -> bool {
        false
    }

    fn hold(class: CiClass, reason: CiPendingReason) -> Self {
        Self {
            class,
            action: CiAction::Hold,
            rationale: CiRouteRationale::Pending(reason),
            provider_action: None,
            tier2_reason: None,
            complete_empty: None,
            // A hold is the proposal's "no route row" answer, verbatim:
            // incomplete or non-terminal evidence is not an immutable CI
            // evidence bundle and cannot enter remediation. The row would be
            // the synthetic identity the scope explicitly rules out.
            creates_route_row: false,
            closes_route: false,
        }
    }

    /// The hold an *enumeration failure* takes.
    ///
    /// Structurally identical to [`Self::hold`] — no row, no lease, no charge,
    /// re-poll — but it carries the incompleteness reason rather than a pending
    /// reason, because "the provider did not finish telling us" is a different
    /// fact from "the run has not finished", and the stale/held suppression
    /// report distinguishes them.
    fn hold_incomplete(reason: CiIncompleteReason) -> Self {
        Self {
            class: CiClass::Unknown,
            action: CiAction::Hold,
            rationale: CiRouteRationale::IncompleteEvidence(reason),
            provider_action: None,
            tier2_reason: None,
            complete_empty: None,
            creates_route_row: false,
            closes_route: false,
        }
    }

    fn discard(class: CiClass, field: CiStaleField) -> Self {
        Self {
            class,
            action: CiAction::Discard,
            rationale: CiRouteRationale::Stale(field),
            provider_action: None,
            tier2_reason: None,
            complete_empty: None,
            creates_route_row: false,
            closes_route: false,
        }
    }

    fn tier1(lane: CiLane, run_id: i64) -> Self {
        let (action, kind) = match lane {
            CiLane::PrHead => (CiAction::RerunRun, CiProviderActionKind::RerunFailedJobs),
            CiLane::MergeGroup => (CiAction::Reenqueue, CiProviderActionKind::EnableAutoMerge),
        };
        Self {
            class: CiClass::Inconclusive,
            action,
            rationale: CiRouteRationale::Inconclusive,
            provider_action: Some(CiProviderAction { kind, run_id }),
            tier2_reason: None,
            complete_empty: None,
            creates_route_row: true,
            closes_route: false,
        }
    }

    fn tier2(class: CiClass, rationale: CiRouteRationale, reason: CiTier2Reason) -> Self {
        Self {
            class,
            action: CiAction::AskLead,
            rationale,
            provider_action: None,
            tier2_reason: Some(reason),
            complete_empty: None,
            creates_route_row: true,
            closes_route: false,
        }
    }

    /// The lane compatibility route for an authoritatively complete empty
    /// enumeration.
    ///
    /// `action` is `Discard` because there is nothing to re-poll for and
    /// nothing to remediate: the evidence was complete and it said "this
    /// repository has no blocking CI here". It is *not* `Hold`, which would
    /// claim a later poll could change the answer. Nothing durable is written
    /// either way — `creates_route_row` is false — so the durable vocabulary
    /// never sees this value.
    fn complete_empty(lane: CiLane) -> Self {
        Self {
            class: CiClass::Unknown,
            action: CiAction::Discard,
            rationale: CiRouteRationale::EmptyBlockingSet,
            provider_action: None,
            tier2_reason: None,
            complete_empty: Some(match lane {
                CiLane::PrHead => CiCompleteEmptyRoute::PrHeadProceed,
                CiLane::MergeGroup => CiCompleteEmptyRoute::MergeGroupRecordPassing,
            }),
            creates_route_row: false,
            closes_route: false,
        }
    }

    fn closed(rationale: CiRouteRationale) -> Self {
        Self {
            class: CiClass::Unknown,
            action: CiAction::Discard,
            rationale,
            provider_action: None,
            tier2_reason: None,
            complete_empty: None,
            creates_route_row: false,
            closes_route: true,
        }
    }
}

// ---------------------------------------------------------------------------
// The classifier
// ---------------------------------------------------------------------------

/// Classify one observation. Pure: no I/O, no clock, no randomness.
///
/// The proposal's table, implemented exhaustively:
///
/// | Complete run evidence | Route |
/// | --- | --- |
/// | at least one causal blocking check | Tier 2 after the current-identity guard |
/// | every blocking check cancelled/never executed (`is_inconclusive`) | Tier 1, subject to budget |
/// | empty blocking set on a completed run | Tier 2 after the guard |
/// | run or any required check non-terminal | Hold |
/// | failed enumeration page, or collected count below `total_count` | Hold, no route row |
/// | no merge-group run correlated, or a blocking check attributable to no run | Hold, no route row |
/// | missing/malformed timestamps, check or log API error, ambiguous merge-group correlation, `MAX_PAGES` truncation | Tier 2 after the guard |
/// | stale run head, PR head, merge-group, or dequeue identity | Discard |
/// | passing complete set | existing passing path |
/// | merged task/PR | existing merged path |
///
/// A newer passing or merged observation is authoritative over everything,
/// including staleness, so it is answered first: it suppresses older pending
/// adjudications rather than being suppressed by them.
///
/// Staleness is evaluated *after* the class is computed, so an obsolete row is
/// still recorded with the class its evidence actually carried. That keeps the
/// stale-suppression report able to say what was thrown away.
pub(crate) fn classify(observation: &CiObservation<'_>) -> CiRouteDecision {
    match observation.capture.0 {
        Capture::Merged => return CiRouteDecision::closed(CiRouteRationale::NewerMerge),
        Capture::Passing => return CiRouteDecision::closed(CiRouteRationale::NewerPass),
        _ => {}
    }

    let (class, complete) = match observation.capture.0 {
        Capture::NonTerminal(_) | Capture::CompleteEmpty => (CiClass::Unknown, None),
        Capture::Incomplete(reason) => (
            CiClass::Unknown,
            Some(CiRouteRationale::IncompleteEvidence(reason)),
        ),
        Capture::FailedComplete { blocking } => {
            // `prove_complete` guarantees this set is non-empty and terminal,
            // so `is_inconclusive`'s "no member is causal" genuinely means
            // "nothing reached a verdict about the code" rather than "nothing
            // has reached a verdict *yet*".
            if ci_triage::is_inconclusive(blocking) {
                (CiClass::Inconclusive, Some(CiRouteRationale::Inconclusive))
            } else {
                (
                    CiClass::CausalFailure,
                    Some(CiRouteRationale::CausalFailure),
                )
            }
        }
        Capture::Merged | Capture::Passing => unreachable!("handled above"),
    };

    // The current-identity guard. Obsolete evidence cannot authorize a
    // provider call, a Tier-2 lease, a board mutation, or a worker — so it is
    // discarded before any of those can be produced, whatever it classified as.
    //
    // Complete-empty is guarded too. It creates no remediation state, but it
    // does drive a lane through its no-CI fast path to *green*, and doing that
    // on the strength of a superseded head is exactly the stale mutation the
    // proposal forbids.
    if let Some(field) = stale_field(observation.evidence, observation.observed_current) {
        return CiRouteDecision::discard(class, field);
    }

    match (observation.capture.0, complete) {
        (Capture::NonTerminal(reason), _) => CiRouteDecision::hold(class, reason),
        (Capture::CompleteEmpty, _) => CiRouteDecision::complete_empty(observation.evidence.lane),
        // Two independent reasons to hold, kept as two predicates because they
        // are two different facts and the report separates them:
        //
        // * the enumeration failed — this is not an evidence bundle yet, and a
        //   later poll can turn it into one for free;
        // * no run was ever named — there is no run/lane identity to key a
        //   route row on, and creating one would mean fabricating the fields
        //   that distinguish one observation from the next.
        //
        // Either way: no route row, no lease, no session, and re-poll.
        (Capture::Incomplete(reason), _)
            if reason.is_enumeration_failure() || reason.no_run_was_named() =>
        {
            CiRouteDecision::hold_incomplete(reason)
        }
        (_, Some(CiRouteRationale::Inconclusive)) => {
            CiRouteDecision::tier1(observation.evidence.lane, observation.evidence.run_id)
        }
        (_, Some(rationale @ CiRouteRationale::CausalFailure)) => {
            CiRouteDecision::tier2(class, rationale, CiTier2Reason::CausalFailure)
        }
        (_, Some(rationale)) => {
            CiRouteDecision::tier2(class, rationale, CiTier2Reason::EvidenceUnknown)
        }
        (_, None) => unreachable!("NonTerminal and CompleteEmpty are handled above"),
    }
}

/// Apply the monotonic budgets to a classified decision.
///
/// Only a Tier-1 decision can be changed: it becomes a Tier-2 retry-exhaustion
/// route when either ceiling is spent. Every other route is budget-independent
/// — a causal failure does not become cheaper because retries remain, and a
/// stale route must not acquire a lease because they do.
///
/// The thresholds themselves are [`CI_SIGNATURE_BUDGET_LIMIT`] (2 per
/// signature) and [`CI_HEAD_BUDGET_LIMIT`] (4 per PR head across both lanes),
/// consumed from `djinn-db` rather than restated here.
/// The two monotonic ceilings, read from `djinn-db` rather than restated here.
///
/// Returned as a pair so a test can assert the numbers the routing contract
/// actually enforces are the numbers the repository charges against — a local
/// copy of `2` and `4` would drift the first time either is tuned.
#[cfg(test)]
pub(crate) fn budget_ceilings() -> (i64, i64) {
    (CI_SIGNATURE_BUDGET_LIMIT, CI_HEAD_BUDGET_LIMIT)
}

pub(crate) fn apply_budget(decision: CiRouteDecision, counts: CiBudgetCounts) -> CiRouteDecision {
    if decision.provider_action.is_none() || !counts.is_exhausted() {
        return decision;
    }
    CiRouteDecision::tier2(
        decision.class,
        CiRouteRationale::BudgetExhausted,
        CiTier2Reason::RetryExhausted,
    )
}

// There is deliberately no `provider_failure_route` constructor, and no
// `CiRouteRationale::ProviderActionFailed` for it to carry.
//
// It existed, it was never called for its value — the executor's only call site
// was `let _ = provider_failure_route(decision.class());` on the line above a
// hard-coded `CiTier2Reason::ProviderActionFailed` — and no test exercised it.
// So it was a second source of truth that nothing could hold to the first.
//
// Consuming it instead of deleting it was tried and is not available: the reason
// lives behind `CiRouteDecision::tier2_reason() -> Option<_>`, whose `None` is
// unreachable for a Tier-2 constructor, and this workspace denies
// `clippy::expect_used`. The honest choices were a panic site, a dead fallback
// branch that would swallow exactly the drift the threading was meant to catch,
// or deletion. `open_tier2` now names `CiTier2Reason::ProviderActionFailed`
// directly, which is how its sibling call site already names
// `CiTier2Reason::RetryExhausted` — one convention, one literal, no shadow copy.

// ---------------------------------------------------------------------------
// Route stages and supersession
// ---------------------------------------------------------------------------

/// Where in the route a supersession was detected. Each stage has exactly one
/// terminal outcome, and they are distinguished because the proposal reports
/// obsolete routes suppressed before the provider call, before Lead dispatch,
/// and before supervisor application as separate counts.
///
/// Only `AfterCall` has a coordinator consumer today — the startup owner
/// handoff. The other three name supersessions the *supervisor* detects (before
/// Lead dispatch, and atomically with applying a Lead result), which is wave 4's
/// half of the contract, and the wave-5 report counts all four separately.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiRouteStage {
    /// Reserved, no provider call has happened.
    PreCall,
    /// The provider call episode was authorized; its result is unknowable.
    AfterCall,
    /// Between the route decision and the Tier-2 lease.
    BeforeLead,
    /// Between Lead dispatch and the supervisor applying its result.
    BeforeApply,
}

pub(crate) fn supersession_outcome(stage: CiRouteStage) -> CiRouteOutcome {
    match stage {
        CiRouteStage::PreCall => CiRouteOutcome::SupersededPreCall,
        CiRouteStage::AfterCall => CiRouteOutcome::SupersededAfterCall,
        CiRouteStage::BeforeLead => CiRouteOutcome::SupersededBeforeLead,
        CiRouteStage::BeforeApply => CiRouteOutcome::SupersededBeforeApply,
    }
}

// ---------------------------------------------------------------------------
// Provider finalization
// ---------------------------------------------------------------------------

/// The outcome a provider-call episode finalizes with.
///
/// # This must be written through `finalize_calling`, never `terminalize`
///
/// `finalize_calling` fences the write to the owning incarnation, which is the
/// mechanism by which a late result from a drained process is rejected instead
/// of silently overwriting a recovery. `terminalize` now refuses these three
/// outcomes and refuses a `calling` row outright, so the fenced path is the
/// only way to claim the provider was called — but that refusal is an *error*
/// at run time, not a compile-time one, so the mapping below is deliberately
/// closed to exactly the set `finalize_calling` accepts.
pub(crate) fn provider_finalization_outcome(lane: CiLane, accepted: bool) -> CiRouteOutcome {
    match (lane, accepted) {
        (_, false) => CiRouteOutcome::ActionFailed,
        (CiLane::PrHead, true) => CiRouteOutcome::Retriggered,
        (CiLane::MergeGroup, true) => CiRouteOutcome::Reenqueued,
    }
}

// ---------------------------------------------------------------------------
// Recovery routing
// ---------------------------------------------------------------------------

/// What the coordinator does with the result of pre-call recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiReservedRecoveryRoute {
    /// No row, or the row is not a recoverable reservation. Nothing to do.
    Inert,
    /// Terminal `superseded_pre_call`. Uncharged, no lease, no Lead, no worker.
    Superseded,
    /// This caller won the compare-and-set and owns one provider-call episode.
    ResumeProviderCall,
    /// Budget spent and this caller opened the head's Tier-2 lease.
    Tier2Exhausted,
    /// Budget spent, and the PR head's Tier-2 lease is **already held by
    /// another row**.
    ///
    /// `recover_reserved` stamps `retry_exhausted_at`, opens no lease, and
    /// leaves the row in phase `reserved`. That row is *not* resumable — its
    /// budget is spent, so a further recovery re-enters this same branch — and
    /// it must not be treated as one, or the coordinator would call the
    /// provider on a row that was never charged. Nor is it stranded: it stays
    /// non-terminal, so the recurring reservation sweep keeps finding it, and
    /// it converges the moment the head advances (it closes
    /// `superseded_pre_call`).
    ///
    /// It is **not** self-healing on its own: nothing in the repository
    /// re-drives it, and the peer's lease resolving does not wake it. The
    /// reservation sweep that owns this route must therefore be periodic, not
    /// startup-only, or the row sits `reserved` until its head moves.
    HeldByHeadLease,
}

/// Route the outcome of [`djinn_db::CiRouteAttemptRepository::recover_reserved`].
///
/// The `RetryExhausted { tier2_lease_id: None }` case is the one worth naming:
/// it is neither a resumption nor a completed escalation, and collapsing it
/// into either is a real defect. See [`CiReservedRecoveryRoute::HeldByHeadLease`].
pub(crate) fn route_reserved_recovery(recovery: &CiReservedRecovery) -> CiReservedRecoveryRoute {
    match recovery {
        CiReservedRecovery::NotFound | CiReservedRecovery::NotEligible(_) => {
            CiReservedRecoveryRoute::Inert
        }
        CiReservedRecovery::SupersededPreCall(_) => CiReservedRecoveryRoute::Superseded,
        CiReservedRecovery::Resumed { .. } => CiReservedRecoveryRoute::ResumeProviderCall,
        CiReservedRecovery::RetryExhausted { tier2_lease_id, .. } => match tier2_lease_id {
            Some(_) => CiReservedRecoveryRoute::Tier2Exhausted,
            None => CiReservedRecoveryRoute::HeldByHeadLease,
        },
    }
}

/// Whether a route may be handed to `open_tier2_lease` at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiTier2Admission {
    /// The lease may be opened.
    Admit,
    /// This row has already used its one trip to Tier 2.
    AlreadyAdjudicated,
    /// The row is in phase `calling`. Only its owner may close it.
    ProviderCallInFlight,
}

/// Whether a route may still open a Tier-2 lease.
///
/// Two independent refusals, and the second is the one worth reading.
///
/// **Once-ever.** "May route once to Tier 2" is a once-ever property, not a
/// concurrency one: a route whose lease has already resolved must not be
/// re-adjudicated when a later sweep revisits it, or the row's
/// `tier2_resolution` flips back to open carrying a stale Lead result.
/// `has_routed_to_tier2` stays true after the lease resolves, which is what
/// makes that a durable guard rather than a snapshot of the moment.
///
/// **Never against a live `calling` row.** This one is a caller obligation
/// because the repository does not hold it. `open_tier2_lease`'s supersession
/// branch terminalizes through a compare-and-set guarded only on
/// `action_phase <> 'terminal'`, and `calling` satisfies that. So handing it a
/// `calling` row together with a superseding observed identity flips the row
/// to `terminal / superseded_before_lead` while the owner's provider future is
/// still in flight — and the owner's subsequent `finalize_calling`, which is
/// fenced on `action_phase = 'calling'`, then returns `false` and its provider
/// result is **silently discarded**. A charged episode that really did call
/// GitHub would end up recorded as a supersession that never called it.
///
/// The correct route for a `calling` row whose evidence went stale is the
/// owner-fenced one: let the owner finalize, or take the row through
/// `recover_calling_owner`, which requires quiescence proof and exclusive lock
/// ownership precisely so that a live owner is never overtaken. This guard is
/// kept even if the repository later adds its own fence: it is the one place
/// the repository trusts its caller, and defence in depth costs one comparison.
pub(crate) fn tier2_admission(attempt: &CiRouteAttempt) -> CiTier2Admission {
    if attempt.action_phase == CiActionPhase::Calling {
        return CiTier2Admission::ProviderCallInFlight;
    }
    if attempt.has_routed_to_tier2() {
        return CiTier2Admission::AlreadyAdjudicated;
    }
    CiTier2Admission::Admit
}

/// Convenience predicate over [`tier2_admission`].
pub(crate) fn may_open_tier2(attempt: &CiRouteAttempt) -> bool {
    tier2_admission(attempt) == CiTier2Admission::Admit
}

// ---------------------------------------------------------------------------
// Reading terminal results back
// ---------------------------------------------------------------------------

/// Whether this route ended in a worker reopen, of either mode.
///
/// Both predicates read `CiRouteAttempt::adjudicated_outcome`, never
/// `terminal_outcome`, and the difference is not cosmetic. `terminal_outcome`
/// is write-once and is claimed by whichever close happened *first*: for a
/// route whose provider call errored that is `action_failed`, committed before
/// Lead ever ran, and the Lead result then lands on `tier2_resolution`. A
/// report that reads `terminal_outcome` alone counts such a route as a provider
/// failure and drops the reopen entirely — which is every reopen that followed
/// a provider failure or an unknown outcome.
// Read back by the wave-5 reporting surface (repair vs diagnostic reopens, parks
// with cited cause). No coordinator path reads a resolved Tier-2 lease yet,
// because nothing dispatches Lead yet.
#[allow(dead_code)]
pub(crate) fn is_reopen(attempt: &CiRouteAttempt) -> bool {
    matches!(
        attempt.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened) | Some(CiRouteOutcome::DiagnosticReopened)
    )
}

/// Whether this route ended in a park. See [`is_reopen`] on why this reads the
/// adjudicated outcome rather than the terminal one.
#[allow(dead_code)]
pub(crate) fn is_park(attempt: &CiRouteAttempt) -> bool {
    attempt.adjudicated_outcome() == Some(CiRouteOutcome::Parked)
}

pub(crate) mod executor;
pub(crate) mod gate;
// Wave 5: the producer of the `ci_route` directive block and the only path
// that turns an opened Tier-2 lease into a Lead session.
pub(crate) mod tier2_dispatch;

#[cfg(test)]
mod tests;
