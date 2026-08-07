//! The two lane call sites (proposal `nafu`, wave 3b).
//!
//! Thin by design. Everything decidable lives in [`super::ci_routing`] and
//! everything durable lives in [`super::ci_routing::executor`]; this module
//! only assembles the arguments from what the poller already holds and reports
//! back one bit — whether the route layer took ownership of this evidence, in
//! which case the caller must withhold its legacy remediation path.
//!
//! Keeping it here rather than inline in `pr_watcher.rs` / `pr_commands.rs` is
//! what keeps the wave's edits to those two shared modules to a handful of
//! lines each, which is the coordination rule the proposal's work-package table
//! sets for `pr_poller`.

use djinn_db::{
    CiEvidenceIdentity, CiHoldIdentity, CiLane, CiOriginState, CiRouteAttemptRepository,
};
use djinn_provider::github_api::{CheckRun, DequeueEvent, MergeMethod, WorkflowRun};

use super::ci_hold::{CiHoldDisposition, CiHoldPoll};
use super::ci_provider::CiRouteProvider;
use super::ci_routing::executor::{
    CiAutoMergeTarget, CiIncarnationLiveness, CiLaneOutcome, CiLaneTarget, CiTier2Handoff,
    execute_route, recover_calling_owners_at_startup, sweep_reserved_routes,
};
use super::ci_routing::gate::CiRoutingGate;
use super::ci_routing::tier2_dispatch::CiTier2Dispatch;
use super::ci_routing::{
    CiCapture, CiCompleteEmptyRoute, CiIncompleteReason, CiObservation, CiRouteSubject,
};
use super::ci_snapshot::evidence::{
    CiLaneEvidence, CiRunEvidence, capture_merge_group_evidence, capture_pr_head_evidence,
    correlate_merge_group_run, dequeue_identity,
};
use crate::actor::CoordinatorActor;

/// What a lane call site must do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiLaneDisposition {
    /// The route layer took ownership. Do not run the legacy path.
    Routed,
    /// The route layer declined. Run the legacy path unchanged.
    Legacy,
    /// The enumeration was provably complete and contained nothing blocking.
    ///
    /// # Why this is a third answer and not `Routed`
    ///
    /// It was `Routed`, and that was a gate-on wedge. `CiCompleteEmptyRoute` was
    /// produced by the classifier and read by nobody: `fold` only consulted
    /// `suppresses_legacy_path`, which is true of everything except
    /// `GateClosed`. So a merge-group dequeue whose correlated run yielded an
    /// empty failing-check set returned `CompleteEmpty(MergeGroupRecordPassing)`
    /// → `is_routed()` → `handle_queue_failure` returned early — and nothing
    /// recorded `Passing`, nothing re-enqueued, nothing reopened. The task sat
    /// in `pr_review` indefinitely, which is the opposite of the proposal's
    /// "records current-head `Passing` and allows the existing review merge
    /// gate".
    ///
    /// It is not `Legacy` either: the legacy path for both call sites is
    /// *remediation*, and complete-empty is the compatibility path to green.
    /// The lane's `Passing` snapshot is written before this is returned; the
    /// caller's job is only to skip remediation and let its existing gate run.
    CompleteEmpty(CiCompleteEmptyRoute),
}

impl CiLaneDisposition {
    /// Whether the caller must withhold its legacy *remediation* path.
    ///
    /// True for `CompleteEmpty` as well as `Routed`: an authoritatively empty
    /// enumeration is a verdict of green, and reopening a task for rework on
    /// the strength of it would be the same wrong answer the completeness
    /// verdict exists to prevent.
    pub(crate) fn is_routed(self) -> bool {
        matches!(self, Self::Routed | Self::CompleteEmpty(_))
    }

    /// The lane compatibility path, when there is one.
    pub(crate) fn complete_empty(self) -> Option<CiCompleteEmptyRoute> {
        match self {
            Self::CompleteEmpty(route) => Some(route),
            _ => None,
        }
    }
}

/// Fold a lane's per-run outcomes into one disposition.
///
/// A lane can produce several evidence identities — a PR head with two failing
/// workflow runs is two routes — and the caller needs one answer. Ownership is
/// sticky: if *any* route took ownership, the legacy path must stay withheld,
/// or the one run the route layer declined would reopen the task while the
/// other's re-run is in flight.
///
/// Complete-empty is answered before ownership because it is a *lane-level*
/// capture: it can only arise when the lane produced no per-run evidence at
/// all, so it is never in a vector alongside a route that took ownership.
fn fold(outcomes: &[CiLaneOutcome]) -> CiLaneDisposition {
    if let Some(route) = outcomes.iter().find_map(|outcome| match outcome {
        CiLaneOutcome::CompleteEmpty(route) => Some(*route),
        _ => None,
    }) {
        return CiLaneDisposition::CompleteEmpty(route);
    }
    if outcomes.iter().any(CiLaneOutcome::suppresses_legacy_path) {
        CiLaneDisposition::Routed
    } else {
        CiLaneDisposition::Legacy
    }
}

/// Whether a lane's capture is one the hold ledger should *count* rather than
/// route.
///
/// Exactly the recoverable incompleteness. An irrecoverable one is not waiting
/// for anything — it takes its own diagnose-only route — and letting a streak
/// keep running underneath it would let one head escalate a second time on top
/// of an adjudication it already has.
/// Rewrite an evidence identity into its **run-absent** form when the capture's
/// reason has no run to name.
///
/// # Why the rewrite happens here and not in the executor
///
/// The classifier decides that an irrecoverable reason routes under the
/// run-absent identity, but it is handed the identity rather than owning it —
/// and the executor keys the durable row on whatever it was handed. Applying
/// the rewrite in the executor would key the ROW run-absent while
/// `observed_current` still named the run, so the very next poll's
/// current-identity guard would see a `RunId` divergence and supersede a route
/// that nothing had actually superseded.
///
/// So both halves of the observation are rewritten together, before the guard
/// ever runs. That is also what makes the collapse real on the **merge-group**
/// lane: `capture_merge_group_evidence` reaches the four timestamp reasons
/// *after* `correlate_merge_group_run` named a run, so without this the merge
/// lane would key them on that run while the PR-head lane keyed the identical
/// reasons run-absent — two rows, two leases, and two Lead sessions for one
/// head, which is exactly what revision 58's `NULLS NOT DISTINCT` collapse
/// exists to prevent.
///
/// `run_head_sha` is normalised to the observed PR head for the same reason:
/// the collapse is over the whole identity, and a run-absent row that kept a
/// per-run head SHA would still be distinct from its sibling.
fn run_absent_if_required(
    identity: CiEvidenceIdentity,
    capture: &CiCapture<'_>,
    observed_head_sha: &str,
) -> CiEvidenceIdentity {
    match capture.incomplete_reason() {
        Some(reason) if reason.is_run_absent() => CiEvidenceIdentity {
            run_id: None,
            run_head_sha: observed_head_sha.to_owned(),
            ..identity
        },
        _ => identity,
    }
}

fn evidence_recoverable_hold_reason(evidence: &CiLaneEvidence<'_>) -> Option<CiIncompleteReason> {
    match evidence {
        CiLaneEvidence::Incomplete(reason) if reason.is_recoverable() => Some(*reason),
        _ => None,
    }
}

impl CoordinatorActor {
    pub(crate) fn ci_routes(&self) -> CiRouteAttemptRepository {
        CiRouteAttemptRepository::new(self.db.clone())
    }

    /// Run the apply transaction and translate its verdict into a lane answer.
    ///
    /// `None` means "this result is authoritative — go and route it". `Some`
    /// means the ledger absorbed it, and in every one of those cases the lane
    /// must ALSO withhold its legacy remediation path: a superseded, held, or
    /// escalated observation that fell through to `handle_ci_failure` would
    /// reopen the task on evidence the contract says produces no worker.
    async fn settle_hold(
        &self,
        poll: &CiHoldPoll,
        observed_current: &CiHoldIdentity,
        evidence: &CiLaneEvidence<'_>,
        origin_state: CiOriginState,
        task_id: &str,
    ) -> Option<CiLaneDisposition> {
        let recoverable = evidence_recoverable_hold_reason(evidence);
        match self
            .apply_ci_hold_poll(
                poll,
                observed_current,
                recoverable.is_none(),
                origin_state,
                // The reason an escalating streak reports is the one it has been
                // holding on. A streak that reaches the bound is by definition
                // mid-hold, so this is `Some`; `PartialPagination` is the
                // conservative stand-in for the unreachable case and never
                // reaches the durable row on any path a hold can take.
                recoverable.unwrap_or(CiIncompleteReason::PartialPagination),
                task_id,
            )
            .await
        {
            CiHoldDisposition::Proceed => None,
            CiHoldDisposition::Absorbed(_) => Some(CiLaneDisposition::Routed),
        }
    }

    /// Apply this poll's result to the hold ledger and, if it is authoritative,
    /// drive and settle the lane.
    ///
    /// Every lane exit goes through here rather than calling `drive_lane`
    /// directly. That is deliberate: the merge-group lane has four exits
    /// (correlation failure, check-API failure, annotation failure, and the
    /// ordinary capture) and an exit that skipped the apply step would advance
    /// no high-watermark — so a delayed poll on that path could still overwrite
    /// a newer one, and the ordering contract would hold on three paths out of
    /// four.
    #[allow(clippy::too_many_arguments)]
    async fn apply_and_drive(
        &self,
        poll: &CiHoldPoll,
        hold_identity: &CiHoldIdentity,
        target: &CiLaneTarget<'_>,
        evidence: &CiLaneEvidence<'_>,
        pr_number: i64,
        pr_head_sha: &str,
        provider: &dyn CiRouteProvider,
        known: Option<CiEvidenceIdentity>,
        task_id: &str,
        pull_number: u64,
    ) -> CiLaneDisposition {
        if let Some(absorbed) = self
            .settle_hold(poll, hold_identity, evidence, target.origin_state, task_id)
            .await
        {
            return absorbed;
        }
        let outcomes = self
            .drive_lane(target, evidence, pr_number, pr_head_sha, provider, known)
            .await;
        self.settle(&outcomes, task_id, pull_number, pr_head_sha)
            .await
    }

    /// The gate, read once per call site so a mid-poll environment change
    /// cannot split one lane's decisions across two postures.
    ///
    /// The lane methods take the resolved gate as a *parameter* rather than
    /// reading it themselves. That is what makes them testable: a fixture that
    /// had to mutate `DJINN_CI_EVIDENCE_ROUTING` to exercise the enabled path
    /// would be mutating process-global state that every other test in the
    /// binary can observe. The environment is read here, at the call site, and
    /// nowhere else.
    ///
    /// The one exception is `CoordinatorActor::test_ci_routing_gate`, which
    /// is `#[cfg(test)]` and `None` everywhere else. It exists so the AC12
    /// wiring fixtures can drive the *production* startup path and the
    /// *production* tick — both of which read the gate through this method —
    /// without touching the process environment. See that field for why the
    /// environment is not usable there.
    pub(crate) fn ci_routing_gate(&self) -> CiRoutingGate {
        #[cfg(test)]
        if let Some(gate) = self.test_ci_routing_gate {
            return gate;
        }
        CiRoutingGate::from_env()
    }

    /// Record the lane compatibility verdict for an authoritatively complete
    /// *empty* enumeration, and say which existing path the caller should take.
    ///
    /// Both lanes persist `Passing` for the current head, and the reason is the
    /// same in each: the review merge gate maps `Unknown` to `Hold`, so a
    /// repository with no blocking CI wedges forever unless something writes the
    /// verdict down. Neither creates a route row, a Tier-2 lease, a Lead or
    /// worker session, a provider action, or a Tier-1 charge — the snapshot is
    /// the whole effect.
    async fn record_complete_empty(
        &self,
        task_id: &str,
        pull_number: u64,
        head_sha: &str,
        route: CiCompleteEmptyRoute,
    ) -> CiLaneDisposition {
        self.persist_ci_snapshot(
            task_id,
            pull_number,
            head_sha,
            djinn_core::models::CiStatus::Passing,
            Vec::new(),
            None,
            0,
            None,
        )
        .await;
        tracing::info!(
            task_id,
            pr = pull_number,
            ?route,
            "ci route: authoritatively complete empty enumeration — recorded current-head Passing"
        );
        CiLaneDisposition::CompleteEmpty(route)
    }

    /// Fold a lane's outcomes and *execute* the compatibility path if that is
    /// what they came to.
    ///
    /// Every `fold` call site goes through this. That is the point: the wedge
    /// this replaces was a decision produced by the classifier and executed by
    /// nobody, and a second `fold` call site that forgot to execute would
    /// reintroduce it.
    async fn settle(
        &self,
        outcomes: &[CiLaneOutcome],
        task_id: &str,
        pull_number: u64,
        head_sha: &str,
    ) -> CiLaneDisposition {
        // `nafu` wave 5: an opened Tier-2 lease has to become a Lead session,
        // or the route layer takes ownership of the evidence (suppressing the
        // legacy path) and then does nothing with it — the task sits in
        // `pr_draft`/`pr_review` with no remedy at all. Wave 3b returned a bare
        // `lease_opened: true` and stopped here.
        //
        // At most one dispatch per settle: the head-scoped lease admits one
        // adjudication per PR head across both lanes, so a lane that produced
        // two Tier-2 routes has exactly one of them holding the lease and the
        // other reporting `lease_opened: false`.
        self.dispatch_opened_tier2_leases(outcomes, task_id).await;
        match fold(outcomes) {
            CiLaneDisposition::CompleteEmpty(route) => {
                self.record_complete_empty(task_id, pull_number, head_sha, route)
                    .await
            }
            other => other,
        }
    }

    /// Dispatch Lead for every route that actually took the Tier-2 lease.
    ///
    /// Reads the handoff, not the boolean: `lease_opened: true` without a
    /// handoff would be a route with nothing to bind a session to, and the
    /// executor only produces the two together.
    async fn dispatch_opened_tier2_leases(&self, outcomes: &[CiLaneOutcome], task_id: &str) {
        let handoffs: Vec<&CiTier2Handoff> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                CiLaneOutcome::Tier2 {
                    handoff: Some(handoff),
                    ..
                } => Some(handoff.as_ref()),
                _ => None,
            })
            .collect();
        if handoffs.is_empty() {
            return;
        }
        let task = match self.task_repo().get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, task_id, "ci route: could not load the task to dispatch Lead");
                return;
            }
        };
        for handoff in handoffs {
            let dispatch = self
                .dispatch_ci_tier2_lead(&task, handoff, task.pr_url.as_deref())
                .await;
            // One dispatch per hold cycle. A second handoff in the same settle
            // finds the row unconsumed and reports `AlreadyInFlight`, which is
            // the head-level hold answering correctly rather than a failure.
            if dispatch == CiTier2Dispatch::Dispatched {
                break;
            }
        }
    }

    /// Route the PR-head lane's terminal evidence.
    ///
    /// Replaces `retrigger_inconclusive_run`'s fan-out when the gate is on. The
    /// two differ in exactly the way the proposal requires: the legacy helper
    /// derives its run ids from `parse_actions_run_id` over *every* failing
    /// check and re-runs all of them under an in-memory dedupe set, while this
    /// path derives one immutable [`CiEvidenceIdentity`] per Actions run via
    /// `check_run_actions_run_id` and gives each its own durable action key.
    ///
    /// [`CiEvidenceIdentity`]: djinn_db::CiEvidenceIdentity
    /// # Why this re-enumerates rather than reusing the poller's check set
    ///
    /// Revision 58 requires the poll sequence to be reserved **before** the
    /// provider enumeration whose result it orders. The caller's
    /// `CheckRunsResponse` was fetched by `get_pull_request` several steps
    /// earlier, before this lane knew the head SHA the hold identity is keyed
    /// on — so ordering against it would order by *response arrival*, which is
    /// the exact authority the contract replaces.
    ///
    /// So the lane takes its own enumeration, under its own reserved sequence,
    /// through the `list_check_runs_for_ref` seam the merge-group lane already
    /// uses. It costs one extra read per poll of a *failing* PR with the gate
    /// on — this method is reached only from the `Inconclusive` and `Failing`
    /// branches — and it is the difference between a fixture that witnesses the
    /// ordering and one that performs it.
    ///
    /// `blocking_filter` replaces the pre-filtered slice for the same reason:
    /// the slice was computed from the stale response. The two call sites use
    /// different predicates, and both are non-capturing, so a plain `fn`
    /// pointer keeps the seam honest without a closure allocation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn route_pr_head_ci_evidence(
        &self,
        provider: &dyn CiRouteProvider,
        task_id: &str,
        task_short_id: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
        pr_head_sha: &str,
        blocking_filter: fn(&CheckRun) -> bool,
        gate: CiRoutingGate,
    ) -> CiLaneDisposition {
        if !gate.owns_routes() {
            return CiLaneDisposition::Legacy;
        }

        let subject = CiRouteSubject::task(task_id);
        let incarnation = self.coordinator_incarnation_id.clone();
        let pr_number = i64::try_from(pull_number).unwrap_or_default();

        let hold_identity = Self::ci_hold_identity(
            &subject,
            owner,
            repo,
            pr_number,
            pr_head_sha,
            CiLane::PrHead,
            None,
        );

        // ── Transaction 1: reserve, then let go of the database ───────────
        let Some(poll) = self.reserve_ci_hold_poll(hold_identity.clone()).await else {
            return CiLaneDisposition::Legacy;
        };

        // ── The provider call, with no transaction open across it ─────────
        let checks = match provider
            .list_check_runs_for_ref(owner, repo, pr_head_sha)
            .await
        {
            Ok(checks) => checks,
            Err(error) => {
                // The enumeration itself failed. That is a recoverable
                // incompleteness by definition — a later poll can succeed — so
                // it goes through the ledger like any other, which is what
                // bounds it. Synthesising the provider's own verdict here
                // rather than short-circuiting is what keeps "a failed request
                // counts toward the bound" true.
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    %error,
                    "ci route: PR-head check enumeration failed; counting one incomplete poll"
                );
                return self
                    .settle_hold(
                        &poll,
                        &hold_identity,
                        &CiLaneEvidence::Incomplete(CiIncompleteReason::EnumerationPageFailed),
                        CiOriginState::PrDraft,
                        task_id,
                    )
                    .await
                    .unwrap_or(CiLaneDisposition::Routed);
            }
        };

        let blocking: Vec<&CheckRun> = checks
            .check_runs
            .iter()
            .filter(|cr| blocking_filter(cr))
            .collect();
        let evidence = capture_pr_head_evidence(pr_number, pr_head_sha, &checks, &blocking);

        let target = CiLaneTarget {
            subject: &subject,
            origin_state: CiOriginState::PrDraft,
            owner,
            repo,
            incarnation_id: &incarnation,
            gate,
            auto_merge: None,
        };

        // ── Transaction 2: apply, under the sequence reserved in step 1 ───
        let disposition = self
            .apply_and_drive(
                &poll,
                &hold_identity,
                &target,
                &evidence,
                pr_number,
                pr_head_sha,
                provider,
                None,
                task_id,
                pull_number,
            )
            .await;
        tracing::debug!(
            task_id = %task_short_id,
            pr = pull_number,
            gate = gate.as_str(),
            sequence = poll.sequence(),
            ?disposition,
            "ci route: pr_head lane"
        );
        disposition
    }

    /// Route the merge-group lane's terminal evidence for one dequeue.
    ///
    /// This is also the producer of the two incompleteness reasons wave 3 left
    /// without one. Both are scoped to "after an immutable run is known", which
    /// is why they can only be produced here — after `correlate_merge_group_run`
    /// has named exactly one terminal run:
    ///
    /// * a failing `list_check_runs_for_ref` on the correlated run's head is
    ///   [`CiIncompleteReason::CheckApiError`];
    /// * a failing annotation read on the primary blocking check is
    ///   [`CiIncompleteReason::LogApiError`].
    ///
    /// Neither is an enumeration failure — the run identity survives both — so
    /// both take the guarded Tier-2 route rather than holding, keyed on that
    /// run identity.
    ///
    /// `CheckApiError` is now reached only by a genuine transport or
    /// JSON-decode failure. A non-success *page status* — including page 1 —
    /// is an enumeration verdict rather than an error, so it arrives as
    /// `Ok(Incomplete(PageFetchFailed))` and holds, exactly as it does on the
    /// PR-head lane. That symmetry is the point: one fact, one route.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn route_merge_group_ci_evidence(
        &self,
        provider: &dyn CiRouteProvider,
        task_id: &str,
        task_short_id: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
        pr_head_sha: &str,
        pr_node_id: &str,
        merge_method: MergeMethod,
        runs: &[WorkflowRun],
        dequeue: Option<&DequeueEvent>,
        gate: CiRoutingGate,
    ) -> CiLaneDisposition {
        if !gate.owns_routes() {
            return CiLaneDisposition::Legacy;
        }

        let subject = CiRouteSubject::task(task_id);
        let incarnation = self.coordinator_incarnation_id.clone();
        let pr_number = i64::try_from(pull_number).unwrap_or_default();
        // The re-enqueue is a queue re-entry, not a merge authored here, so the
        // headline is the generic PR reference rather than the PR title the
        // undraft path passes. GitHub overwrites it with the queue's own merge
        // commit message when the group lands.
        let commit_headline = format!("Merge pull request #{pull_number}");
        let auto_merge = CiAutoMergeTarget {
            node_id: pr_node_id,
            commit_headline: &commit_headline,
            method: merge_method,
        };
        let target = CiLaneTarget {
            subject: &subject,
            origin_state: CiOriginState::PrReview,
            owner,
            repo,
            incarnation_id: &incarnation,
            gate,
            auto_merge: Some(auto_merge),
        };

        // A dequeue this poll cannot name is a route that could never prove on a
        // later poll that it is looking at the same eviction.
        let Some(dequeue_id) = dequeue.and_then(dequeue_identity) else {
            tracing::debug!(
                task_id = %task_short_id,
                pr = pull_number,
                "ci route: merge-group dequeue has no identity; leaving the lane to the legacy path"
            );
            return CiLaneDisposition::Legacy;
        };

        // The dequeue is part of the hold identity, so the reservation waits
        // until it is named — and it still precedes every provider read this
        // lane makes: correlation runs over the workflow runs the caller
        // already holds, and the two reads below (`list_check_runs_for_ref`
        // and `get_check_run_annotations`) both come after this point.
        let hold_identity = Self::ci_hold_identity(
            &subject,
            owner,
            repo,
            pr_number,
            pr_head_sha,
            CiLane::MergeGroup,
            Some(&dequeue_id),
        );
        let Some(poll) = self.reserve_ci_hold_poll(hold_identity.clone()).await else {
            return CiLaneDisposition::Legacy;
        };

        let run = match correlate_merge_group_run(pull_number, runs) {
            Ok(run) => run.clone(),
            Err(err) => {
                // No *single* run was named — but the lane identity is real,
                // and that is what the route row is keyed on. Lane, PR number,
                // PR head SHA and the dequeue id were all resolved above from
                // what the provider actually said; only the run fields are
                // absent, because naming one is precisely what failed.
                //
                // Passing this instead of `None` is what fixes the collision
                // the audit named: with a fabricated `dequeue_id: None` the
                // provider-action key collapsed to subject + lane + PR + PR
                // head, so two merge-group dequeues of the same head that both
                // failed correlation shared one key — the second resolved
                // `AlreadyPresent` and never got a route at all, and
                // `stale_field` could never supersede on a newer dequeue.
                //
                // Of the two error arms only `AmbiguousMergeGroupCorrelation`
                // still reaches a route row (the proposal puts it on the
                // guarded Tier-2 side explicitly); `NotTerminal` and
                // `MergeGroupCorrelationUnavailable` hold and write nothing, so
                // for them this identity is never persisted.
                let lane_identity = CiEvidenceIdentity {
                    lane: CiLane::MergeGroup,
                    pr_number,
                    pr_head_sha: pr_head_sha.to_owned(),
                    // No single run was named, and revision 58 gives that
                    // fact its own encoding rather than a sentinel: `run_id`
                    // is NULL, under `CHECK (run_id IS NULL OR run_id > 0)`,
                    // matched `NULLS NOT DISTINCT`. The `run_id: 0` placeholder
                    // that stood here collapsed two distinct dequeues of one
                    // head onto a single key; absence now collapses only with
                    // absence, which is the intended behaviour — every
                    // irrecoverable reason on this lane identity shares one
                    // diagnose-only route rather than opening a second lease.
                    run_id: None,
                    run_head_sha: pr_head_sha.to_owned(),
                    dequeue_id: Some(dequeue_id.clone()),
                };
                let evidence: CiLaneEvidence<'_> = err.into();
                return self
                    .apply_and_drive(
                        &poll,
                        &hold_identity,
                        &target,
                        &evidence,
                        pr_number,
                        pr_head_sha,
                        provider,
                        Some(lane_identity),
                        task_id,
                        pull_number,
                    )
                    .await;
            }
        };

        // From here on the run identity is known and immutable, which is the
        // exact precondition the two API-error reasons are scoped to. Keying
        // their route rows on it — rather than on the synthetic per-head
        // identity — is what stops two different failures of one merge-group run
        // sharing a single provider-action key.
        let known = CiEvidenceIdentity {
            lane: CiLane::MergeGroup,
            pr_number,
            pr_head_sha: pr_head_sha.to_owned(),
            run_id: Some(i64::try_from(run.id).unwrap_or(i64::MAX)),
            run_head_sha: run.head_sha.clone(),
            dequeue_id: Some(dequeue_id.clone()),
        };

        // `CheckApiError`: the run identity is known and immutable, and the
        // check API still refused. Complete-but-unusable, not re-pollable.
        let checks = match provider
            .list_check_runs_for_ref(owner, repo, &run.head_sha)
            .await
        {
            Ok(checks) => checks,
            Err(error) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    run_id = run.id,
                    %error,
                    "ci route: check API failed after the merge-group run was identified"
                );
                let evidence = CiLaneEvidence::Incomplete(CiIncompleteReason::CheckApiError);
                return self
                    .apply_and_drive(
                        &poll,
                        &hold_identity,
                        &target,
                        &evidence,
                        pr_number,
                        pr_head_sha,
                        provider,
                        Some(known),
                        task_id,
                        pull_number,
                    )
                    .await;
            }
        };

        let failed: Vec<&CheckRun> = checks
            .check_runs
            .iter()
            .filter(|cr| super::is_failing_conclusion(cr.conclusion.as_deref()))
            .collect();

        // `LogApiError`: the annotation read the evidence bundle depends on
        // failed, again after an immutable run is known.
        if let Some(primary) = failed.first()
            && provider
                .get_check_run_annotations(owner, repo, primary.id)
                .await
                .is_err()
        {
            tracing::warn!(
                task_id = %task_short_id,
                run_id = run.id,
                check = %primary.name,
                "ci route: annotation read failed after the merge-group run was identified"
            );
            let evidence = CiLaneEvidence::Incomplete(CiIncompleteReason::LogApiError);
            return self
                .apply_and_drive(
                    &poll,
                    &hold_identity,
                    &target,
                    &evidence,
                    pr_number,
                    pr_head_sha,
                    provider,
                    Some(known),
                    task_id,
                    pull_number,
                )
                .await;
        }

        let evidence = capture_merge_group_evidence(
            pr_number,
            pr_head_sha,
            &run,
            &dequeue_id,
            &checks,
            &failed,
        );
        // `known` is the identity of the run this capture was taken from, and
        // it is passed rather than dropped. `capture_merge_group_evidence` can
        // return a *lane-level* verdict — an enumeration reason, a
        // non-terminal blocking check, complete-empty, or an unusable-timestamp
        // reason — and every one of those used to be keyed on a fabricated
        // `run_id: 0` / `dequeue_id: None` even though the correlated run was
        // right here in scope. The `Runs` case ignores this argument: each run
        // carries its own identity.
        //
        // `known` is moved by the `CheckApiError` and `LogApiError` branches
        // above, but both of those `return`, so this use is on a path where it
        // was never moved.
        let disposition = self
            .apply_and_drive(
                &poll,
                &hold_identity,
                &target,
                &evidence,
                pr_number,
                pr_head_sha,
                provider,
                Some(known),
                task_id,
                pull_number,
            )
            .await;
        tracing::debug!(
            task_id = %task_short_id,
            pr = pull_number,
            run_id = run.id,
            gate = gate.as_str(),
            sequence = poll.sequence(),
            ?disposition,
            "ci route: merge_group lane"
        );
        disposition
    }

    /// Drive every route one lane's evidence produced.
    ///
    /// `observed_current` is rebuilt per route from the *live* PR head this
    /// poll observed, so a head that moved between capture and execution is a
    /// pre-call discard rather than a provider call against stale code.
    async fn drive_lane(
        &self,
        target: &CiLaneTarget<'_>,
        evidence: &CiLaneEvidence<'_>,
        pr_number: i64,
        observed_head_sha: &str,
        provider: &dyn CiRouteProvider,
        // The identity to key a lane-level capture on. The merge-group lane
        // always supplies one — the correlated run when there is one, the real
        // lane identity (with the real dequeue id) when correlation failed —
        // and the PR-head lane supplies `None`, because it has nothing more
        // specific than the lane and the head until it fans out per run.
        known: Option<CiEvidenceIdentity>,
    ) -> Vec<CiLaneOutcome> {
        let routes = self.ci_routes();
        let scope = self.provider_action_scope.clone();

        if let Some(capture) = evidence.lane_capture() {
            // No per-run evidence: hold, incomplete, or complete-empty.
            //
            // This comment used to assert that "none of these branches creates
            // a route row", and that was simply false: several incompleteness
            // reasons classify to Tier 2 and DO write a row — keyed on whatever
            // identity arrives here. Fabricating that identity is what let two
            // distinct observations of one PR head collapse onto a single
            // provider-action key.
            //
            // What is true now: the recoverable reasons hold and write nothing,
            // the merge-group lane always passes a real `known`, the fallback
            // below is reached only by the PR-head lane, and `run_absent`
            // rewrites either of them when the reason has no run to name.
            let identity = known.unwrap_or(CiEvidenceIdentity {
                lane: lane_of(target.origin_state),
                pr_number,
                pr_head_sha: observed_head_sha.to_owned(),
                // This is a lane-level verdict and no run has been named,
                // so the run field is genuinely ABSENT. `run_id: None` is the
                // run-absent evidence identity revision 58 requires; there is
                // no in-band sentinel and the database refuses one
                // (`CHECK (run_id IS NULL OR run_id > 0)`).
                //
                // On the PR-head lane `run_head_sha` *is* the PR head, so that
                // field is real — `capture_pr_head_evidence` sets it
                // identically for genuine runs — which is what lets the
                // `NULLS NOT DISTINCT` unique index collapse every
                // irrecoverable reason on one head onto exactly one row.
                run_id: None,
                run_head_sha: observed_head_sha.to_owned(),
                dequeue_id: None,
            });
            let identity = run_absent_if_required(identity, &capture, observed_head_sha);
            let observation = CiObservation {
                evidence: &identity,
                observed_current: &identity,
                capture,
            };
            return vec![execute_route(&routes, provider, &scope, target, &observation, &[]).await];
        }

        let CiLaneEvidence::Runs(runs) = evidence else {
            return Vec::new();
        };

        let mut outcomes = Vec::with_capacity(runs.len());
        for run in runs {
            outcomes.push(
                self.drive_one(&routes, &scope, target, run, observed_head_sha, provider)
                    .await,
            );
        }
        outcomes
    }

    async fn drive_one(
        &self,
        routes: &CiRouteAttemptRepository,
        scope: &crate::types::ProviderActionScope,
        target: &CiLaneTarget<'_>,
        run: &CiRunEvidence<'_>,
        observed_head_sha: &str,
        provider: &dyn CiRouteProvider,
    ) -> CiLaneOutcome {
        let capture = CiCapture::prove_complete(run.enumeration, &run.blocking);
        let evidence = run_absent_if_required(run.identity.clone(), &capture, observed_head_sha);
        let observed_current = CiEvidenceIdentity {
            pr_head_sha: observed_head_sha.to_owned(),
            ..evidence.clone()
        };
        let observation = CiObservation {
            evidence: &evidence,
            observed_current: &observed_current,
            capture,
        };
        execute_route(routes, provider, scope, target, &observation, &run.blocking).await
    }

    /// Close this subject's routes on a newer passing or merged observation.
    ///
    /// The proposal is explicit that a pass or a merge is authoritative over
    /// everything, staleness included: it "suppresses older pending
    /// adjudications rather than being suppressed by them", and it closes keys
    /// **without refunding charges**. Without this, a PR that went green after a
    /// Tier-1 re-run would leave its route rows non-terminal forever, holding
    /// the head's Tier-2 lease against the next head that needs it.
    ///
    /// Cheap when the feature is off (one gate read, no query) and cheap when it
    /// is on and there is nothing to close (one `UPDATE ... WHERE` matching
    /// zero rows).
    pub(crate) async fn close_ci_routes_on_success(
        &self,
        task_id: &str,
        pull_number: u64,
        merged: bool,
    ) {
        let gate = self.ci_routing_gate();
        if !gate.owns_routes() {
            return;
        }
        let subject = CiRouteSubject::task(task_id);
        let pr_number = i64::try_from(pull_number).unwrap_or_default();
        // Route the decision through the classifier rather than hard-coding the
        // outcome, so "a newer pass outranks a stale route" stays one rule.
        let identity = CiEvidenceIdentity {
            lane: CiLane::PrHead,
            pr_number,
            pr_head_sha: String::new(),
            run_id: None,
            run_head_sha: String::new(),
            dequeue_id: None,
        };
        let capture = if merged {
            CiCapture::merged()
        } else {
            CiCapture::passing()
        };
        let decision = super::ci_routing::classify(&CiObservation {
            evidence: &identity,
            observed_current: &identity,
            capture,
        });
        if !decision.closes_route() {
            return;
        }
        let outcome = if merged {
            djinn_db::CiRouteOutcome::Merged
        } else {
            djinn_db::CiRouteOutcome::Passed
        };
        match self
            .ci_routes()
            .close_routes_for_newer_outcome(&subject, pr_number, outcome, None)
            .await
        {
            Ok(0) => {}
            Ok(closed) => tracing::info!(
                task_id,
                pr = pull_number,
                closed,
                outcome = outcome.as_str(),
                "ci route: closed routes on a newer authoritative outcome"
            ),
            Err(error) => {
                tracing::warn!(%error, task_id, "ci route: failed to close routes on success")
            }
        }
    }

    /// The startup owner handoff for `calling` rows left behind by a former
    /// incarnation.
    ///
    /// Runs once, from `CoordinatorActor` startup, after this incarnation has
    /// registered its lease. It is startup-only because elapsed time is never
    /// evidence that a `calling` owner is dead. The one proof this witness can
    /// read is the former owner's own `provider_actions_drained_at` stamp — a
    /// fact about where that process's provider futures went, not about how
    /// long a row has sat. Without it the row is deferred, not taken.
    pub(crate) async fn recover_ci_calling_owners_at_startup(&self) {
        let gate = self.ci_routing_gate();
        if !gate.owns_routes() {
            return;
        }
        let liveness = CiIncarnationLiveness {
            incarnations: djinn_db::CoordinatorIncarnationRepository::new(self.db.clone()),
        };
        let report = recover_calling_owners_at_startup(
            &self.ci_routes(),
            &self.task_repo(),
            &liveness,
            &self.coordinator_incarnation_id,
            // The coordinator actor only runs behind `run_with_leadership`, so
            // reaching here at all is the advisory-lock attestation.
            true,
            gate,
        )
        .await;
        if report.examined > 0 {
            tracing::info!(
                examined = report.examined,
                outcome_unknown = report.outcome_unknown,
                superseded_after_call = report.superseded_after_call,
                deferred = report.deferred,
                unverifiable = report.unverifiable,
                "ci route: startup owner handoff complete"
            );
        }
    }

    /// One pass of the recurring reserved-route sweep.
    ///
    /// Called from the coordinator's periodic tick rather than only at startup;
    /// see [`sweep_reserved_routes`] for why a startup-only sweep strands the
    /// head-lease-blocked rows forever.
    pub(crate) async fn sweep_ci_routes(&self) {
        let gate = self.ci_routing_gate();
        if !gate.owns_routes() {
            return;
        }
        let routes = self.ci_routes();
        let witness = self.task_repo();
        let report =
            sweep_reserved_routes(&routes, &witness, &self.coordinator_incarnation_id, gate).await;
        if report.examined > 0 {
            tracing::info!(
                examined = report.examined,
                superseded = report.superseded,
                escalated = report.escalated,
                held_by_head_lease = report.held_by_head_lease,
                unverifiable = report.unverifiable,
                "ci route sweep: pass complete"
            );
        }

        // `nafu` wave 5. Reporting is emitted from the sweep rather than from an
        // operator-invoked tool, so the numbers exist without anyone having
        // thought to ask for them — which is the difference between a report and
        // a query someone could write.
        self.emit_ci_route_report().await;

        // While draining, take the rollback quiescence report on every pass.
        // The proposal blocks a binary rollback on "one repository-checkable
        // report" reading zero across six counts, and an operator watching a
        // drain needs to see it converge, not to poll for it by hand. It is
        // recorded whether or not it is clean: a blocked attempt is exactly what
        // has to be auditable afterwards.
        if gate == CiRoutingGate::Quiescing
            && let Err(error) = self.record_ci_rollback_quiescence_report().await
        {
            tracing::warn!(%error, "ci route: rollback quiescence report failed");
        }
    }
}

fn lane_of(origin: CiOriginState) -> CiLane {
    match origin {
        CiOriginState::PrDraft => CiLane::PrHead,
        CiOriginState::PrReview => CiLane::MergeGroup,
    }
}
