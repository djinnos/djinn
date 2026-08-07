//! The two lane executors (proposal `nafu`, wave 3b).
//!
//! Wave 2 landed a classifier that decides; wave 3 landed the durable schema,
//! the sealed capture, and the provider-action scope. Nothing connected them,
//! so nothing in the tree could take a route from "this evidence is
//! inconclusive" to "GitHub was asked to re-run it". This module is that
//! connection, and it is the only place in the crate that turns a
//! [`CiRouteDecision`] into a provider mutation or a durable row.
//!
//! # Why these are free functions and not `CoordinatorActor` methods
//!
//! Everything the executor needs is passed in: the route repository, the
//! provider seam, the action scope, and a target describing the PR. It holds no
//! task repository, no board handle, no dispatcher, and no session sink.
//!
//! That is not a style preference — it is the assertion. The spec requires
//! every discard fixture to prove *no board mutation and no worker dispatch*,
//! and a method on `CoordinatorActor` could only prove that by inspecting the
//! branch it happened not to take. A free function that was never handed a
//! board cannot mutate one on any branch, and the fixtures still count rows in
//! a real database either side of the call rather than trusting that argument.
//!
//! # The order of the four phases, and why admission comes before the charge
//!
//! 1. `reserve` — commit the unique evidence row as `reserved`, before any
//!    provider contact. Provider mutation is forbidden in this phase.
//! 2. **Admit into the provider-action scope.** A closed scope means leadership
//!    is winding down, and the caller must not call the provider *nor advance
//!    the row to `calling`*. This step is therefore before the charge, not
//!    after it: a charged `calling` row with no future behind it holds both
//!    slots and only startup recovery would ever finalize it.
//! 3. `charge_and_begin_calling` — revalidate identity, compare-and-set
//!    `reserved` → `calling`, increment both monotonic counters. One winner.
//! 4. Call the provider exactly once, then `finalize_calling` under the owner
//!    fence.
//!
//! # What the executor deliberately does not do
//!
//! It does not dispatch Lead and it does not write a `ci_route` directive.
//! Opening the Tier-2 lease is the end of *this* module's half of the
//! contract; it returns the lease id and the route keys in a
//! [`CiTier2Handoff`], and `ci_lane_routing` is what turns that into an
//! arbitration row and a board transition. Keeping the split means the
//! executor still holds no board handle, so "no board mutation on any branch"
//! stays a property of the type signature rather than of the branch coverage.

use djinn_db::{
    CiChargeOutcome, CiOriginState, CiReserveOutcome, CiReservedRecovery, CiRouteAttemptRepository,
    CiRouteReservation, CiTier2LeaseOutcome,
};
use djinn_provider::github_api::{
    CheckRun, GitHubApiError, MergeMethod, RequiredCheckReproduction,
};

use crate::pr_poller::ci_provider::CiRouteProvider;
use crate::types::ProviderActionScope;

use super::gate::CiRoutingGate;
use super::{
    CiAction, CiCompleteEmptyRoute, CiEvidenceIdentity, CiObservation, CiProviderActionKind,
    CiReservedRecoveryRoute, CiRouteOutcome, CiRouteRationale, CiRouteSubject, CiStaleField,
    CiTier2Reason, apply_budget, classify, head_budget_key, may_open_tier2, provider_action_key,
    retry_budget_key, route_reserved_recovery, tier2_lease_key, transient_fingerprint,
};

// ---------------------------------------------------------------------------
// Call target
// ---------------------------------------------------------------------------

/// The GraphQL target `enable_auto_merge` needs. Merge-group lane only.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CiAutoMergeTarget<'a> {
    pub node_id: &'a str,
    pub commit_headline: &'a str,
    pub method: MergeMethod,
}

/// Everything about *where* a route acts, as opposed to *what* it decided.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CiLaneTarget<'a> {
    pub subject: &'a CiRouteSubject,
    pub origin_state: CiOriginState,
    pub owner: &'a str,
    pub repo: &'a str,
    pub incarnation_id: &'a str,
    pub gate: CiRoutingGate,
    /// Present for the merge-group lane; `None` makes the re-enqueue
    /// unexecutable, which is a route-level incompleteness rather than a panic.
    pub auto_merge: Option<CiAutoMergeTarget<'a>>,
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Why a route did nothing without that being an answer about the evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiDeferral {
    /// The provider-action scope refused admission — leadership is winding
    /// down. The row stays `reserved` and is recoverable.
    AdmissionClosed,
    /// Another incarnation owns a live provider call for this evidence. Only
    /// its owner or a quiescent startup handoff may close it.
    ProviderCallInFlight,
    /// The row already reached a terminal outcome.
    AlreadyTerminal,
    /// The gate is `quiescing` and this is a new route.
    GateQuiescing,
    /// The route needed a call target the lane could not supply (a merge-group
    /// re-enqueue with no PR node id). Fails closed: nothing is charged.
    UnexecutableTarget,
    /// The route repository failed. Nothing is claimed either way.
    RepositoryError,
}

/// What one evidence identity's route did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CiLaneOutcome {
    /// The feature is off. The caller must take its legacy path unchanged.
    GateClosed,
    /// A newer passing or merged observation closed this subject's routes.
    ClosedByNewerOutcome(CiRouteRationale),
    /// Hold and re-poll. No row, no lease, no charge — and the caller must not
    /// fall through to the legacy remediation path either, because holding is
    /// the answer.
    Held(CiRouteRationale),
    /// Lane compatibility path for an authoritatively complete empty
    /// enumeration.
    CompleteEmpty(CiCompleteEmptyRoute),
    /// The identity is obsolete. Nothing was spent.
    Discarded(CiStaleField),
    /// Tier 1 charged; the provider accepted. `pr_head` stays `pr_draft`,
    /// `merge_group` stays `pr_review`.
    ProviderAccepted(CiRouteOutcome),
    /// Tier 1 charged; the provider returned an explicit error. The slots stay
    /// charged and the route may escalate once.
    ProviderFailed { lease_opened: bool },
    /// Routed to Lead. `lease_opened` is false when the head already holds an
    /// adjudication, which is the head-level hold doing its job.
    Tier2 {
        reason: CiTier2Reason,
        lease_opened: bool,
        /// Everything the Lead dispatch needs to name this route row, present
        /// exactly when `lease_opened` is true (proposal `nafu`, wave 5).
        ///
        /// Wave 3b opened the lease and returned a bare bool, so nothing
        /// downstream could say *which row* the Lead session it never
        /// dispatched would have adjudicated. These are the keys the
        /// supervisor's guard call needs, and they exist only here.
        handoff: Option<Box<CiTier2Handoff>>,
    },
    /// Nothing happened, and the reason is not an answer about the evidence.
    Deferred(CiDeferral),
}

/// The durable handles that let a Lead session be bound to one route row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CiTier2Handoff {
    pub subject: CiRouteSubject,
    pub provider_action_key: String,
    pub tier2_lease_id: String,
    pub identity: CiEvidenceIdentity,
    pub origin_state: CiOriginState,
    pub reason: CiTier2Reason,
    /// Handles from the evidence bundle a Lead directive must cite. Non-empty
    /// by construction — the supervisor's `is_grounded` fails closed on an
    /// empty list, and `read_arbiter_directive` refuses a block without them.
    pub evidence_references: Vec<String>,
    /// The only corpus a repair's `verification_command` may be drawn from.
    /// May legitimately be empty — an unreproducible check yields no command —
    /// in which case every repair on this route degrades to a diagnosis.
    pub repository_commands: Vec<String>,
}

impl CiLaneOutcome {
    /// Whether the caller must withhold its legacy remediation path.
    ///
    /// This is the single predicate both lane call sites read, so the two lanes
    /// cannot drift on when the legacy `PrCiFailed` reopen is suppressed.
    ///
    /// [`Self::GateClosed`] is the only outcome that hands the evidence back.
    /// Note which side the deferrals sit on: a route deferred because a
    /// provider call is in flight, or because admission closed with a
    /// recoverable `reserved` row behind it, has *already* taken ownership of
    /// this evidence. Letting the legacy path also run would reopen the task
    /// for rework while the queue re-entry it triggered is still live — one
    /// failure spending two remedies.
    pub(crate) fn suppresses_legacy_path(&self) -> bool {
        !matches!(self, Self::GateClosed)
    }

    /// Whether this outcome consumed a Tier-1 charge.
    #[cfg(test)]
    pub(crate) fn charged(&self) -> bool {
        matches!(
            self,
            Self::ProviderAccepted(_) | Self::ProviderFailed { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// Execute one lane's route for one immutable evidence identity.
///
/// `blocking` is the run's blocking check set; it is the input to the transient
/// fingerprint, which is what makes equivalent later evidence share one budget.
pub(crate) async fn execute_route(
    routes: &CiRouteAttemptRepository,
    provider: &dyn CiRouteProvider,
    scope: &ProviderActionScope,
    target: &CiLaneTarget<'_>,
    observation: &CiObservation<'_>,
    blocking: &[&CheckRun],
) -> CiLaneOutcome {
    if !target.gate.owns_routes() {
        return CiLaneOutcome::GateClosed;
    }

    let decision = classify(observation);

    // A newer pass or merge outranks everything, staleness included, so it is
    // answered before the gate's new-route refusal: closing obsolete keys is
    // draining, not admitting.
    if decision.closes_route() {
        let outcome = match decision.rationale() {
            CiRouteRationale::NewerMerge => CiRouteOutcome::Merged,
            _ => CiRouteOutcome::Passed,
        };
        if let Err(error) = routes
            .close_routes_for_newer_outcome(
                target.subject,
                observation.observed_current.pr_number,
                outcome,
                Some(&identity_json(observation.observed_current)),
            )
            .await
        {
            tracing::warn!(%error, "ci route: failed to close routes on a newer outcome");
        }
        return CiLaneOutcome::ClosedByNewerOutcome(decision.rationale());
    }

    // Holds, discards, and the complete-empty compatibility paths never touch
    // the durable layer, so they answer identically under every gate state.
    if !decision.creates_route_row() {
        if let Some(route) = decision.complete_empty_route() {
            return CiLaneOutcome::CompleteEmpty(route);
        }
        return match decision.rationale() {
            CiRouteRationale::Stale(field) => CiLaneOutcome::Discarded(field),
            rationale => CiLaneOutcome::Held(rationale),
        };
    }

    let keys = RouteKeys::derive(target, observation.evidence, blocking, decision.action());

    // `quiescing` admits no new route, but it must still tell the caller
    // whether *this* evidence already has a live one, or the legacy path would
    // double-remedy a failure whose queue re-entry is in flight.
    if !target.gate.admits_new_routes() {
        return match routes.get(target.subject, &keys.action).await {
            Ok(Some(attempt)) if attempt.action_phase == djinn_db::CiActionPhase::Calling => {
                CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
            }
            Ok(Some(attempt)) if !attempt.is_terminal() => {
                CiLaneOutcome::Deferred(CiDeferral::GateQuiescing)
            }
            Ok(_) => CiLaneOutcome::GateClosed,
            Err(error) => {
                tracing::warn!(%error, "ci route: failed to read route under a quiescing gate");
                CiLaneOutcome::Deferred(CiDeferral::RepositoryError)
            }
        };
    }

    let counts = match routes
        .budget_counts(target.subject, &keys.retry_budget, &keys.head_budget)
        .await
    {
        Ok(counts) => counts,
        Err(error) => {
            tracing::warn!(%error, "ci route: failed to read retry budgets");
            return CiLaneOutcome::Deferred(CiDeferral::RepositoryError);
        }
    };
    let decision = apply_budget(decision, counts);

    // `apply_budget` can turn a Tier-1 route into a Tier-2 retry-exhaustion
    // route, and that changes the action — so the action key is re-derived
    // rather than reused. The two are genuinely different routes for the same
    // evidence: one charged and called, one adjudicates because it may not.
    let keys = RouteKeys::derive(target, observation.evidence, blocking, decision.action());

    let reserved = match routes
        .reserve(&CiRouteReservation {
            subject: target.subject.clone(),
            provider_action_key: keys.action.clone(),
            identity: observation.evidence.clone(),
            origin_state: target.origin_state,
            class: decision.class(),
            action: decision.action(),
            transient_fingerprint: keys.fingerprint.clone(),
            retry_budget_key: keys.retry_budget.clone(),
            head_budget_key: keys.head_budget.clone(),
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(%error, "ci route: failed to reserve the route row");
            return CiLaneOutcome::Deferred(CiDeferral::RepositoryError);
        }
    };

    match reserved {
        // Both ceilings spent on a route that still wanted to call. There is no
        // row, so there is nothing to lease — and there does not need to be:
        // whichever earlier route reached the ceiling escalated under the
        // *head-scoped* Tier-2 key, so this head already has its one
        // adjudication.
        CiReserveOutcome::BudgetExhausted(_) => {
            return CiLaneOutcome::Tier2 {
                reason: CiTier2Reason::RetryExhausted,
                lease_opened: false,
                handoff: None,
            };
        }
        CiReserveOutcome::AlreadyPresent(attempt) => {
            if attempt.action_phase == djinn_db::CiActionPhase::Calling {
                return CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight);
            }
            if attempt.is_terminal() {
                return CiLaneOutcome::Deferred(CiDeferral::AlreadyTerminal);
            }
        }
        CiReserveOutcome::Reserved(_) => {}
    }

    let Some(action) = decision.provider_action() else {
        let reason = decision
            .tier2_reason()
            .unwrap_or(CiTier2Reason::EvidenceUnknown);
        return open_tier2(
            routes,
            provider,
            target,
            observation.evidence,
            observation.observed_current,
            &keys,
            reason,
            blocking,
        )
        .await;
    };

    // Phase 2. Admission before the charge — see the module docs. It is also
    // the only way to learn *which* call this route authorizes: `admit` returns
    // the sole carrier of the action kind and run id, so the target check below
    // is necessarily inside the scope.
    let Some(admitted) = action.admit(scope) else {
        return CiLaneOutcome::Deferred(CiDeferral::AdmissionClosed);
    };

    // The re-enqueue has no PR to act on without a node id. Fail closed
    // *before* charging: an unexecutable target must not consume a slot. The
    // guard is released by the early return, so the scope is left empty.
    let auto_merge = match admitted.kind() {
        CiProviderActionKind::EnableAutoMerge => match target.auto_merge {
            Some(t) => Some(t),
            None => return CiLaneOutcome::Deferred(CiDeferral::UnexecutableTarget),
        },
        CiProviderActionKind::RerunFailedJobs => None,
    };

    // Phase 3.
    match routes
        .charge_and_begin_calling(
            target.subject,
            &keys.action,
            target.incarnation_id,
            observation.observed_current,
        )
        .await
    {
        Ok(CiChargeOutcome::Charged { .. }) => {}
        Ok(CiChargeOutcome::SupersededPreCall(attempt)) => {
            return CiLaneOutcome::Discarded(
                super::stale_field(&attempt.identity, observation.observed_current)
                    .unwrap_or(CiStaleField::PrHeadSha),
            );
        }
        Ok(CiChargeOutcome::BudgetExhausted { .. }) => {
            drop(admitted);
            return open_tier2(
                routes,
                provider,
                target,
                observation.evidence,
                observation.observed_current,
                &keys,
                CiTier2Reason::RetryExhausted,
                blocking,
            )
            .await;
        }
        Ok(CiChargeOutcome::NotReserved(attempt)) => {
            return if attempt.action_phase == djinn_db::CiActionPhase::Calling {
                CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
            } else {
                CiLaneOutcome::Deferred(CiDeferral::AlreadyTerminal)
            };
        }
        Err(error) => {
            tracing::warn!(%error, "ci route: failed to charge the route");
            return CiLaneOutcome::Deferred(CiDeferral::RepositoryError);
        }
    }

    // Phase 4. Exactly one call, under the scope guard `admitted` holds.
    let lane = observation.evidence.lane;
    let run_id = u64::try_from(admitted.run_id()).unwrap_or_default();
    let called = match admitted.kind() {
        CiProviderActionKind::RerunFailedJobs => {
            provider
                .rerun_failed_jobs(target.owner, target.repo, run_id)
                .await
        }
        CiProviderActionKind::EnableAutoMerge => {
            let t = auto_merge.unwrap_or(CiAutoMergeTarget {
                node_id: "",
                commit_headline: "",
                method: MergeMethod::Squash,
            });
            provider
                .enable_auto_merge(
                    target.owner,
                    target.repo,
                    u64::try_from(observation.evidence.pr_number).unwrap_or_default(),
                    t.method,
                    t.node_id,
                    t.commit_headline,
                )
                .await
                .map(|_| ())
        }
    };
    // The scope guard is released only after the call has returned a known
    // result, which is what makes leadership's join a genuine quiescence proof.
    drop(admitted);

    let accepted = called.is_ok();
    let error_json = called.as_ref().err().map(provider_error_json);
    let outcome = super::provider_finalization_outcome(lane, accepted);
    match routes
        .finalize_calling(
            target.subject,
            &keys.action,
            target.incarnation_id,
            outcome,
            error_json.as_deref(),
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            // The owner fence rejected this write: another incarnation
            // legally recovered the row while the call was in flight. The
            // recovering incarnation's record is authoritative and this
            // result is deliberately dropped rather than overwriting it.
            tracing::warn!(
                key = %keys.action,
                "ci route: provider finalization lost the owner fence; result discarded"
            );
            return CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight);
        }
        Err(error) => {
            tracing::warn!(%error, "ci route: failed to finalize the provider call");
            return CiLaneOutcome::Deferred(CiDeferral::RepositoryError);
        }
    }

    if accepted {
        return CiLaneOutcome::ProviderAccepted(outcome);
    }

    // An explicit provider error is charged, terminal, and may route to Lead
    // once — only while the identity is still current, which `open_tier2_lease`
    // enforces with its own compare-and-set.
    //
    // The reason is named here, exactly as `CiTier2Reason::RetryExhausted` is
    // named at the budget-exhaustion `open_tier2` call above. There used to be a
    // `let _ = provider_failure_route(decision.class());` on the line before
    // this one: a second, discarded copy of the same verdict that nothing read
    // and no test exercised. It has been deleted rather than threaded through —
    // see the note where it used to live in `super`.
    let escalated = open_tier2(
        routes,
        provider,
        target,
        observation.evidence,
        observation.observed_current,
        &keys,
        CiTier2Reason::ProviderActionFailed,
        blocking,
    )
    .await;
    CiLaneOutcome::ProviderFailed {
        lease_opened: matches!(
            escalated,
            CiLaneOutcome::Tier2 {
                lease_opened: true,
                ..
            }
        ),
    }
}

/// Open the head's Tier-2 lease under the current-identity compare-and-set.
#[allow(clippy::too_many_arguments)]
async fn open_tier2(
    routes: &CiRouteAttemptRepository,
    provider: &dyn CiRouteProvider,
    target: &CiLaneTarget<'_>,
    evidence: &CiEvidenceIdentity,
    observed_current: &CiEvidenceIdentity,
    keys: &RouteKeys,
    reason: CiTier2Reason,
    blocking: &[&CheckRun],
) -> CiLaneOutcome {
    // The caller-side guard `tier2_admission` documents: the repository's
    // supersession branch would happily terminalize a live `calling` row, and
    // the owner's later `finalize_calling` would then be silently discarded.
    match routes.get(target.subject, &keys.action).await {
        Ok(Some(attempt)) if !may_open_tier2(&attempt) => {
            return if attempt.action_phase == djinn_db::CiActionPhase::Calling {
                CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
            } else {
                CiLaneOutcome::Tier2 {
                    reason,
                    lease_opened: false,
                    handoff: None,
                }
            };
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "ci route: failed to read the route before the Tier-2 lease");
            return CiLaneOutcome::Deferred(CiDeferral::RepositoryError);
        }
    }

    // The repair validator's corpus, read once for this route.
    //
    // A repair is invalid without a repository-valid command, so an empty
    // corpus makes every repair degrade to a diagnosis. That degradation is
    // safe but it is not free — a diagnosis costs the same worker session and
    // asks it to re-derive what CI already printed — so the corpus is fetched
    // here rather than left empty.
    let commands = reproduction_commands(provider, target, evidence, blocking).await;

    match routes
        .open_tier2_lease(
            target.subject,
            &keys.action,
            observed_current,
            &keys.tier2_lease,
            reason,
        )
        .await
    {
        Ok(CiTier2LeaseOutcome::Opened { lease_id, .. }) => CiLaneOutcome::Tier2 {
            reason,
            lease_opened: true,
            // The one place these keys exist together. Everything downstream —
            // the `ci_route` directive block, the supervisor's guard call —
            // reads them from here.
            handoff: Some(Box::new(CiTier2Handoff {
                subject: target.subject.clone(),
                provider_action_key: keys.action.clone(),
                tier2_lease_id: lease_id,
                repository_commands: commands,
                identity: evidence.clone(),
                origin_state: target.origin_state,
                reason,
                evidence_references: evidence_references(evidence, blocking),
            })),
        },
        Ok(CiTier2LeaseOutcome::SupersededBeforeLead(attempt)) => CiLaneOutcome::Discarded(
            super::stale_field(&attempt.identity, observed_current).unwrap_or(CiStaleField::RunId),
        ),
        Ok(CiTier2LeaseOutcome::OwnedByProviderCall(_)) => {
            CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
        }
        Ok(
            CiTier2LeaseOutcome::KeyHeldElsewhere(_)
            | CiTier2LeaseOutcome::AlreadyRoutedToTier2(_)
            | CiTier2LeaseOutcome::NotFound,
        ) => CiLaneOutcome::Tier2 {
            reason,
            lease_opened: false,
            handoff: None,
        },
        Err(error) => {
            tracing::warn!(%error, "ci route: failed to open the Tier-2 lease");
            CiLaneOutcome::Deferred(CiDeferral::RepositoryError)
        }
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The commands the failing checks actually executed, for the repair
/// validator's corpus (proposal `nafu`, wave 5).
///
/// # Why this is not "inventing a command from a job name"
///
/// The proposal forbids exactly that, and permits a command "directly exposed
/// as a command by CI evidence". This is the latter, literally: the strings
/// come from `parse_actions_run_commands` over the Actions **job log**, so each
/// one is a line the runner ran. Nothing here derives a command from a check
/// name, a workflow name, or a build tool.
///
/// Bounded on purpose. Only the first [`MAX_REPRODUCTION_CHECKS`] blocking
/// checks are queried: each call is two GitHub reads, this runs only on a route
/// that is already escalating to Lead, and a run with thirty failing checks has
/// long since stopped being one diagnosable failure.
///
/// Failures are silent and non-fatal. An unreproducible check, a rate limit, or
/// a log that no longer exists yields fewer commands — which costs a repair its
/// command and produces a diagnosis, never a wrong one.
async fn reproduction_commands(
    provider: &dyn CiRouteProvider,
    target: &CiLaneTarget<'_>,
    evidence: &CiEvidenceIdentity,
    blocking: &[&CheckRun],
) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    for check in blocking.iter().take(MAX_REPRODUCTION_CHECKS) {
        let context = provider
            .required_check_reproduction_context(
                target.owner,
                target.repo,
                &evidence.run_head_sha,
                &check.name,
            )
            .await;
        let Ok(RequiredCheckReproduction::Reproducible(context)) = context else {
            continue;
        };
        // The failing command, plus the setup commands that ran before it. A
        // repair whose remedy is "run the setup step the workflow runs" is as
        // legitimate as one naming the failing command itself.
        commands.push(context.command);
        commands.extend(context.setup_steps.into_iter().map(|step| step.command));
    }
    commands.retain(|command| !command.trim().is_empty());
    commands.sort();
    commands.dedup();
    commands
}

/// How many blocking checks a Tier-2 route will query for reproduction context.
const MAX_REPRODUCTION_CHECKS: usize = 3;

/// The handles a Lead directive must cite to count as evidence-grounded.
///
/// Deliberately narrow: run id, both head SHAs, the dequeue id when there is
/// one, and the blocking check names. Every one of them is a string that
/// appears verbatim in the evidence bundle Lead is shown, so "cite one of
/// these" is a check Lead can actually satisfy by quoting what it read — as
/// opposed to a similarity score over prose, which is the fabrication the
/// grounding rule exists to stop.
///
/// The list is never empty: the two SHAs are always present, so a route can
/// never produce a block that the supervisor's fail-closed `is_grounded` would
/// reject wholesale — including on a **run-absent** route, where there is no run
/// id to cite. Emitting a placeholder there would hand Lead a reference it could
/// quote to look grounded while citing nothing, which is the opposite of what
/// this list is for.
fn evidence_references(identity: &CiEvidenceIdentity, blocking: &[&CheckRun]) -> Vec<String> {
    let mut out = vec![identity.pr_head_sha.clone(), identity.run_head_sha.clone()];
    if let Some(run_id) = identity.run_id {
        out.push(run_id.to_string());
    }
    if let Some(dequeue) = &identity.dequeue_id {
        out.push(dequeue.clone());
    }
    out.extend(blocking.iter().map(|check| check.name.clone()));
    out.retain(|reference| !reference.trim().is_empty());
    out.sort();
    out.dedup();
    out
}

/// The four derived keys for one route, computed once.
#[derive(Clone, Debug)]
pub(crate) struct RouteKeys {
    pub action: String,
    pub fingerprint: String,
    pub retry_budget: String,
    pub head_budget: String,
    pub tier2_lease: String,
}

impl RouteKeys {
    pub(crate) fn derive(
        target: &CiLaneTarget<'_>,
        identity: &CiEvidenceIdentity,
        blocking: &[&CheckRun],
        action: CiAction,
    ) -> Self {
        let fingerprint = transient_fingerprint(identity.lane, blocking);
        Self {
            action: provider_action_key(target.subject, identity, action),
            retry_budget: retry_budget_key(target.subject, identity, &fingerprint),
            head_budget: head_budget_key(target.subject, identity.pr_number, &identity.pr_head_sha),
            tier2_lease: tier2_lease_key(target.subject, identity.pr_number, &identity.pr_head_sha),
            fingerprint,
        }
    }
}

fn identity_json(identity: &CiEvidenceIdentity) -> String {
    serde_json::to_string(identity).unwrap_or_else(|_| "{}".to_owned())
}

/// The bounded provider error envelope stored on the route row.
fn provider_error_json(error: &GitHubApiError) -> String {
    const MAX_BODY: usize = 2_000;
    let body: String = error.body.chars().take(MAX_BODY).collect();
    serde_json::json!({
        "method": error.method,
        "path": error.path,
        "status": error.status.map(|s| s.as_u16()),
        "body": body,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// The reserved-recovery sweep
// ---------------------------------------------------------------------------

/// How often the recurring reservation sweep runs.
///
/// Wave 3 left recovery startup-only, and one of its own doc comments explains
/// why that is not enough: a reservation whose head lease is already taken
/// stamps `retry_exhausted_at`, opens no lease, and stays in phase `reserved`.
/// Nothing in the repository re-drives it and the peer's lease resolving does
/// not wake it, so under a startup-only sweep that row sits `reserved` until
/// the process restarts or the PR head moves.
pub(crate) const RESERVED_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// What one sweep did, counted rather than described.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CiSweepReport {
    pub examined: usize,
    pub resumed: usize,
    pub superseded: usize,
    pub escalated: usize,
    pub held_by_head_lease: usize,
    pub inert: usize,
    /// Rows the sweep refused to touch because it could not prove the identity
    /// is still current. See [`sweep_reserved_routes`].
    pub unverifiable: usize,
}

/// The current PR head for a subject, as the coordinator durably knows it.
///
/// The sweep needs a current-identity witness and has no provider client of its
/// own. Re-polling GitHub per stranded row would make a background sweep an
/// unbounded API fan-out, and passing the *stored* identity as if it were the
/// observed one would be strictly wrong: `recover_reserved` would then resume
/// an obsolete row and call the provider against a head that has moved, which
/// is the exact mutation the pre-call guard exists to prevent.
///
/// So the witness is the poller's own durable CI snapshot, which is the sole
/// writer of the observed head for a task/PR pair. A row whose head the witness
/// cannot supply is counted `unverifiable` and left alone for the polling path,
/// which does hold a live observation.
#[async_trait::async_trait]
pub(crate) trait CiCurrentHeadWitness: Send + Sync {
    async fn current_pr_head(&self, subject: &CiRouteSubject, pr_number: i64) -> Option<String>;
}

#[async_trait::async_trait]
impl CiCurrentHeadWitness for djinn_db::TaskRepository {
    async fn current_pr_head(&self, subject: &CiRouteSubject, pr_number: i64) -> Option<String> {
        if subject.kind != djinn_db::CiSubjectKind::Task {
            return None;
        }
        self.get_ci_snapshot_for_task_pr(&subject.id, pr_number)
            .await
            .ok()
            .flatten()
            .map(|snapshot| snapshot.head_sha)
    }
}

/// Recover every *stranded* `reserved` row once.
///
/// # The sweep deliberately never resumes a route
///
/// `recover_reserved` has three outcomes and only one of them is safe to reach
/// from here. `Resumed` compare-and-sets the row into `calling` and charges
/// both counters — it hands the caller one provider-call episode and expects it
/// to be performed. This sweep holds no provider client (a background sweep
/// that called GitHub would be a second, unobserved provider path, and
/// resolving an installation client per stranded row would make it an unbounded
/// API fan-out), so a `Resumed` row here would sit `calling`, charged, with no
/// future behind it — strictly worse than the `reserved` row it started as.
///
/// It does not need to. A `reserved` row whose identity is still current and
/// whose budget still has room is exactly the row the *polling* path resumes on
/// its next tick: `reserve` answers `AlreadyPresent`, and the executor charges
/// and calls from there with a live client. So the sweep reads both facts first
/// and skips those rows entirely.
///
/// That leaves precisely the rows no poller will revisit:
///
/// * **obsolete identity** — the head moved, so nothing polls this evidence
///   again; closed terminal `superseded_pre_call`, uncharged;
/// * **current identity, budget spent** — cannot be charged, so the poller's
///   resume path would only re-derive the same refusal; escalated to the head's
///   Tier-2 lease, or counted `held_by_head_lease` when the head already holds
///   one. The second is the case wave 3 flagged as not self-healing: nothing in
///   the repository re-drives it and a peer's lease resolving does not wake it.
///
/// Both branches are non-resuming *by construction of `recover_reserved`*,
/// which returns `Resumed` only for a current identity with budget remaining.
/// The read-then-act window cannot invert either predicate: budgets are
/// monotonic and a PR head only moves forward.
///
/// Idempotent and safe to run concurrently with the pollers: every state change
/// goes through the repository's compare-and-set.
pub(crate) async fn sweep_reserved_routes(
    routes: &CiRouteAttemptRepository,
    witness: &dyn CiCurrentHeadWitness,
    incarnation_id: &str,
    gate: CiRoutingGate,
) -> CiSweepReport {
    let mut report = CiSweepReport::default();
    if !gate.owns_routes() {
        return report;
    }

    let rows = match routes.non_terminal_identities().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "ci route sweep: failed to enumerate non-terminal routes");
            return report;
        }
    };

    for (subject, action_key, identity) in rows {
        report.examined += 1;

        let Some(head) = witness.current_pr_head(&subject, identity.pr_number).await else {
            report.unverifiable += 1;
            continue;
        };
        // The witness proves the head, and only the head. Every other component
        // of the identity is immutable for this row, so substituting the
        // observed head into the stored identity produces exactly the
        // observation the pre-call guard needs: identical when the head has not
        // moved, divergent on `PrHeadSha` when it has.
        let observed = CiEvidenceIdentity {
            pr_head_sha: head,
            ..identity.clone()
        };
        let lease_key = tier2_lease_key(&subject, observed.pr_number, &observed.pr_head_sha);

        // The two reads that keep the sweep out of the resume branch. Skipping
        // here is not a weaker guarantee: it is the polling path's row.
        let Ok(Some(attempt)) = routes.get(&subject, &action_key).await else {
            report.inert += 1;
            continue;
        };
        if attempt.action_phase != djinn_db::CiActionPhase::Reserved {
            report.inert += 1;
            continue;
        }
        if attempt.identity.matches(&observed) {
            let budget_left = match routes
                .budget_counts(
                    &subject,
                    &attempt.retry_budget_key,
                    &attempt.head_budget_key,
                )
                .await
            {
                Ok(counts) => !counts.is_exhausted(),
                Err(error) => {
                    tracing::warn!(%error, key = %action_key, "ci route sweep: budget read failed");
                    continue;
                }
            };
            if budget_left {
                report.inert += 1;
                continue;
            }
        }

        let recovery = match routes
            .recover_reserved(
                &subject,
                &action_key,
                &observed,
                incarnation_id,
                djinn_db::CI_RESERVATION_RECOVERY_TIMEOUT_SECS,
                &lease_key,
            )
            .await
        {
            Ok(recovery) => recovery,
            Err(error) => {
                tracing::warn!(%error, key = %action_key, "ci route sweep: recovery failed");
                continue;
            }
        };

        match route_reserved_recovery(&recovery) {
            CiReservedRecoveryRoute::Inert => report.inert += 1,
            CiReservedRecoveryRoute::Superseded => report.superseded += 1,
            CiReservedRecoveryRoute::Tier2Exhausted => report.escalated += 1,
            CiReservedRecoveryRoute::HeldByHeadLease => report.held_by_head_lease += 1,
            CiReservedRecoveryRoute::ResumeProviderCall => {
                // Unreachable given the two reads above, and loud rather than
                // silent if it ever becomes reachable: a resumed row is
                // `calling` and charged with no future behind it, and only
                // startup recovery would ever close it.
                report.resumed += 1;
                if let CiReservedRecovery::Resumed { attempt, .. } = &recovery {
                    tracing::error!(
                        key = %attempt.provider_action_key,
                        "ci route sweep: resumed a route it cannot call — the row is now \
                         charged and `calling` with no provider future behind it"
                    );
                }
            }
        }
    }

    report
}

// ---------------------------------------------------------------------------
// Startup owner handoff for `calling` rows
// ---------------------------------------------------------------------------

/// What a startup handoff pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CiHandoffReport {
    pub examined: usize,
    /// Recovered and still current: charged, `outcome_unknown`, and eligible
    /// for at most one Tier-2 lease.
    pub outcome_unknown: usize,
    /// Recovered and obsolete: `superseded_after_call`, with no provider query,
    /// no replay, and no board mutation.
    pub superseded_after_call: usize,
    /// Left alone. A live owner, a missing quiescence proof, a timeout not yet
    /// elapsed, or a lost compare-and-set are all this.
    pub deferred: usize,
    pub unverifiable: usize,
}

/// The liveness proof the handoff needs about the *former* owner.
///
/// Deliberately a separate trait from the route repository: the authority is
/// the coordinator incarnation ledger plus the advisory lock, and neither is a
/// CI concept. It exists so the fixtures can present a terminated owner, a
/// gracefully drained owner, and a live one without a second process.
#[async_trait::async_trait]
pub(crate) trait CiOwnerLiveness: Send + Sync {
    /// The quiescence proof available for `incarnation_id`, or
    /// [`djinn_db::CiQuiescenceProof::None`] when the former owner may still be
    /// running.
    async fn quiescence_proof(&self, incarnation_id: &str) -> djinn_db::CiQuiescenceProof;
}

/// Hand off every `calling` row whose former owner is provably quiescent.
///
/// Startup-only by contract — elapsed time is never evidence that an owner is
/// dead, so this may only run on a new incarnation that has just won the
/// advisory lock. The repository re-checks every predicate and writes an audit
/// row for each refusal, so a deferral here is observable rather than silent.
///
/// After a legal handoff the external outcome is genuinely unknowable, and the
/// repository resolves that rather than guessing: a still-current row becomes
/// [`djinn_db::CiRouteOutcome::OutcomeUnknown`] and retains its charge — that
/// transition is durable and owned entirely by `recover_calling_owner`, with no
/// classifier mirror — while an obsolete one becomes
/// `superseded_after_call`, which is [`super::supersession_outcome`] of
/// [`super::CiRouteStage::AfterCall`]. Nothing here queries the provider,
/// replays the call, or touches the board.
pub(crate) async fn recover_calling_owners_at_startup(
    routes: &CiRouteAttemptRepository,
    witness: &dyn CiCurrentHeadWitness,
    liveness: &dyn CiOwnerLiveness,
    recovering_incarnation: &str,
    holds_exclusive_lock: bool,
    gate: CiRoutingGate,
) -> CiHandoffReport {
    let mut report = CiHandoffReport::default();
    if !gate.owns_routes() {
        return report;
    }

    let rows = match routes.non_terminal_identities().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "ci route handoff: failed to enumerate non-terminal routes");
            return report;
        }
    };

    for (subject, action_key, identity) in rows {
        let Ok(Some(attempt)) = routes.get(&subject, &action_key).await else {
            continue;
        };
        if attempt.action_phase != djinn_db::CiActionPhase::Calling {
            continue;
        }
        let Some(former) = attempt.owner_incarnation_id.clone() else {
            continue;
        };
        if former == recovering_incarnation {
            continue;
        }
        report.examined += 1;

        // `recover_calling_owner` refuses an unobservable identity outright,
        // and it is right to: passing "unknown" would resolve to
        // `superseded_after_call` and silently suppress the Tier-2 adjudication
        // a possibly-current row is owed.
        let Some(head) = witness.current_pr_head(&subject, identity.pr_number).await else {
            report.unverifiable += 1;
            continue;
        };
        let observed = CiEvidenceIdentity {
            pr_head_sha: head,
            ..identity.clone()
        };

        let authority = djinn_db::CiCallingRecoveryAuthority {
            recovering_incarnation: recovering_incarnation.to_owned(),
            former_owner_incarnation: former.clone(),
            holds_exclusive_lock,
            quiescence_proof: liveness.quiescence_proof(&former).await,
            calling_recovery_timeout_secs: djinn_db::CI_CALLING_RECOVERY_TIMEOUT_SECS,
        };

        let lease_key = tier2_lease_key(&subject, observed.pr_number, &observed.pr_head_sha);
        match routes
            .recover_calling_owner(&subject, &action_key, &observed, &authority, &lease_key)
            .await
        {
            Ok(djinn_db::CiCallingRecovery::Recovered { outcome, .. }) => {
                // The obsolete arm's outcome is read from the classifier's
                // stage table rather than restated, so the repository and the
                // routing contract cannot drift on what "recovered but stale"
                // is called.
                if outcome == super::supersession_outcome(super::CiRouteStage::AfterCall) {
                    report.superseded_after_call += 1;
                } else {
                    report.outcome_unknown += 1;
                }
            }
            Ok(djinn_db::CiCallingRecovery::Deferred { reason, .. }) => {
                report.deferred += 1;
                tracing::info!(
                    key = %action_key,
                    reason = reason.as_str(),
                    "ci route handoff: deferred"
                );
            }
            Ok(djinn_db::CiCallingRecovery::NotFound) => {}
            Err(error) => {
                tracing::warn!(%error, key = %action_key, "ci route handoff: recovery failed");
            }
        }
    }

    report
}

/// The coordinator-incarnation-backed liveness witness.
///
/// One proof, and only one: `provider_actions_drained_at`. The former
/// incarnation closed action admission, joined every provider future and
/// finalizer, and only then stamped its own row. That stamp is the sole entry
/// in the ledger that is a *fact* about where those futures went, rather than
/// an inference from a clock.
///
/// # Why an expired renewal lease is not `ProcessTerminated`
///
/// It reads like process-death evidence and is not. Renewal is scoped to the
/// coordinator's cancellation token, so it stops the instant leadership is
/// cancelled — long before the process is gone. A leader whose drain overran
/// its budget therefore looks *identical* to a dead one once any expiry window
/// passes, while a `rerun_failed_jobs` future of its own is still in flight.
/// Reading that as termination hands a charged `calling` row to a new
/// incarnation, so the same provider action runs twice, the old owner's fenced
/// `finalize_calling` is discarded, and a real episode is adjudicated
/// `outcome_unknown`. Elapsed time is never by itself evidence that a `calling`
/// owner is dead.
///
/// # What "no proof" costs
///
/// The row stays `calling` and the repository defers — writing a
/// `LiveOwnerDeferred` audit row on every pass, so a stuck row is observable
/// rather than silent. In the ordinary case the former owner's own fenced
/// finalizer resolves it; in the crash case it waits for an incarnation that
/// can show a drain stamp. A row that waits is strictly cheaper than a second
/// provider call against evidence that is already charged.
pub(crate) struct CiIncarnationLiveness {
    pub incarnations: djinn_db::CoordinatorIncarnationRepository,
}

#[async_trait::async_trait]
impl CiOwnerLiveness for CiIncarnationLiveness {
    async fn quiescence_proof(&self, incarnation_id: &str) -> djinn_db::CiQuiescenceProof {
        match self.incarnations.get(incarnation_id).await {
            Ok(Some(row)) if row.provider_actions_drained_at.is_some() => {
                djinn_db::CiQuiescenceProof::GracefulDrain
            }
            // A live owner, an owner whose renewal lease lapsed, an incarnation
            // the ledger never recorded, and a failed read are one answer:
            // none of them says where the former owner's provider future went.
            _ => djinn_db::CiQuiescenceProof::None,
        }
    }
}

#[cfg(test)]
mod tests;
