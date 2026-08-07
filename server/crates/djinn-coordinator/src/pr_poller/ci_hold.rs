//! The bounded-hold ordering contract for incomplete CI evidence (proposal
//! `nafu`, revision 58).
//!
//! # What this module exists to prevent
//!
//! Two pollers can observe the same lane identity at once, and a provider
//! response can arrive in a different order from the requests that produced it.
//! Without an authority order, the *later-arriving* result wins — so a delayed
//! incomplete poll can land after a complete one, resurrect a streak the
//! complete poll cleared, and eventually escalate a head whose CI is green.
//!
//! Revision 58 fixes the authority order at **sequence reservation time**, not
//! at response-arrival time, and the shape of that is exactly three steps:
//!
//! 1. a **short** transaction that revalidates the current identity and assigns
//!    this logical poll one monotonically increasing sequence;
//! 2. the provider enumeration, with **no** transaction open across it;
//! 3. a second **short** transaction that applies the result only if its
//!    sequence exceeds the retained `last_applied_poll_sequence`.
//!
//! The middle step is why the two transactions cannot be one. A transaction
//! spanning a GitHub round trip holds a row lock — and a pool connection — for
//! the length of a network call that has its own retry ladder, which is how one
//! slow PR starves the coordinator's pool. It is also why the ordering has to
//! be a *durable sequence* rather than "whoever holds the lock": the lock is
//! released between the two steps by construction.
//!
//! # The logical poll, and why it is a UUID
//!
//! A "logical poll" is one attempt to observe one lane identity. It is
//! identified by a UUID minted at entry and carried through both transactions,
//! so a transaction retry — a serialization failure, a pool hiccup, a process
//! restart mid-poll — reads back the sequence it was already assigned rather
//! than reserving a second one. Without that, a retry would look like a newer
//! poll and would be able to supersede its own earlier self.
//!
//! # What a hold is allowed to do
//!
//! Nothing. A recoverable incomplete observation writes exactly one row in the
//! streak ledger and one in its child observation ledger, and that state
//! authorizes no provider action, no charge, no session, no worker, and no
//! board mutation. The single thing it can eventually cause is the *twelfth*
//! consecutive incomplete poll turning into one diagnose-only route — and even
//! that is written by the ledger's own transaction, atomically with the
//! escalation marker, so a crash between them is not a state this code can
//! reach.

use djinn_db::{
    CiEvidenceIdentity, CiHoldApply, CiHoldEscalationRoute, CiHoldIdentity,
    CiIncompleteHoldRepository, CiLane, CiOriginState, CiRouteAttemptRepository,
    CiRouteReservation, CiRouteSubject, CiTier2Reason,
};

use super::ci_routing::executor::CiTier2Handoff;
use super::ci_routing::{
    CiCapture, CiIncompleteReason, CiObservation, classify, head_budget_key, provider_action_key,
    retry_budget_key, tier2_lease_key,
};
use crate::actor::CoordinatorActor;

/// One logical poll of one lane identity, holding the sequence it reserved.
///
/// Deliberately not `Clone`: two copies would be two logical polls sharing one
/// sequence, which is the one thing the child ledger exists to make impossible.
#[derive(Debug)]
pub(crate) struct CiHoldPoll {
    identity: CiHoldIdentity,
    observation_id: String,
    sequence: i64,
}

impl CiHoldPoll {
    /// The monotonic sequence assigned before enumeration. Logged on every lane
    /// disposition, and read by the fixtures that assert a replayed logical
    /// poll keeps its own.
    pub(crate) fn sequence(&self) -> i64 {
        self.sequence
    }
}

/// What the hold ledger says the lane should do with this poll's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiHoldDisposition {
    /// The result is authoritative for this identity. Classify and route it.
    Proceed,
    /// The ledger absorbed the result. The lane performs no routing **and**
    /// withholds its legacy remediation path.
    ///
    /// Withholding legacy is not incidental. A superseded observation whose
    /// lane then fell through to `handle_ci_failure` would reopen the task on
    /// evidence a newer poll already answered — which is precisely the
    /// "no worker, no board mutation" clause the ordering contract is for.
    Absorbed(CiHoldAbsorption),
}

/// Why the ledger absorbed a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiHoldAbsorption {
    /// A newer logical poll already applied. Side-effect-free by construction.
    Superseded,
    /// The head, lane, or dequeue advanced between reservation and application.
    IdentityAdvanced,
    /// Recoverably incomplete and still inside the bound.
    Held { poll_count: i64 },
    /// This apply was the one that reached the bound. The diagnose-only
    /// run-absent route was inserted in the same transaction that set the
    /// escalation marker.
    ///
    /// `route_inserted` is false when a run-absent route for this identity
    /// already existed — an irrecoverable reason that routed directly earlier.
    /// That is correct and is the "a later reason adds evidence but opens no
    /// second lease or Lead session" clause.
    Escalated { route_inserted: bool },
    /// Already escalated. The high-watermark advances; the count does not, and
    /// nothing is dispatched.
    AlreadyEscalated,
    /// The apply transaction failed after the reservation committed.
    ///
    /// The lane must not fall through to legacy remediation here: the reserved
    /// sequence exists, a retry of the same logical poll will apply it, and
    /// reopening the task in the meantime would be the double-remedy the
    /// contract forbids. Holding is the fail-closed answer and costs one poll.
    LedgerUnavailable,
}

impl CoordinatorActor {
    pub(crate) fn ci_holds(&self) -> CiIncompleteHoldRepository {
        CiIncompleteHoldRepository::new(self.db.clone())
    }

    /// Build the hold identity for one lane observation.
    ///
    /// `repository_id` is `owner/repo`. That string is **mutable** — a
    /// repository rename changes it — and the sibling route keys deliberately
    /// exclude it for exactly that reason: a rename mid-flight would orphan a
    /// `calling` row and authorize a second provider call.
    ///
    /// It is safe *here* because a streak authorizes nothing. A rename starts a
    /// fresh streak at zero, which costs at most eleven extra polls of patience
    /// and cannot cause an action. The proposal names the repository in the hold
    /// identity, and the honest reading is that the hold needs to distinguish
    /// two projects' PR #41, which this does.
    pub(crate) fn ci_hold_identity(
        subject: &CiRouteSubject,
        owner: &str,
        repo: &str,
        pr_number: i64,
        pr_head_sha: &str,
        lane: CiLane,
        dequeue_id: Option<&str>,
    ) -> CiHoldIdentity {
        CiHoldIdentity {
            subject: subject.clone(),
            repository_id: format!("{owner}/{repo}"),
            pr_number,
            pr_head_sha: pr_head_sha.to_owned(),
            lane,
            dequeue_id: dequeue_id.map(str::to_owned),
        }
    }

    /// Step 1: reserve this logical poll's sequence, **before** enumeration.
    ///
    /// Returns `None` when the ledger is unreachable. Nothing has been written
    /// at that point, so the caller declines and the legacy path runs — the
    /// same answer a closed gate gives, and the only one that does not trade a
    /// database blip for a stalled merge queue.
    pub(crate) async fn reserve_ci_hold_poll(
        &self,
        identity: CiHoldIdentity,
    ) -> Option<CiHoldPoll> {
        let observation_id = uuid::Uuid::now_v7().to_string();
        match self
            .ci_holds()
            .reserve_poll(&identity, &observation_id)
            .await
        {
            Ok(reservation) => Some(CiHoldPoll {
                identity,
                observation_id,
                sequence: reservation.poll_sequence,
            }),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "ci hold: could not reserve a poll sequence; declining to the legacy path"
                );
                None
            }
        }
    }

    /// Step 3: apply this logical poll's result.
    ///
    /// `complete` is the enumeration's own verdict, narrowed to the question
    /// the ledger asks: *did this poll produce evidence that is not a
    /// recoverable incompleteness?* An irrecoverable reason counts as complete
    /// here — it is not waiting for anything, it takes its own diagnose-only
    /// route, and leaving a streak running underneath it would let a head that
    /// already has an adjudication escalate a second time.
    pub(crate) async fn apply_ci_hold_poll(
        &self,
        poll: &CiHoldPoll,
        observed_current: &CiHoldIdentity,
        complete: bool,
        origin_state: CiOriginState,
        hold_reason: CiIncompleteReason,
        task_id: &str,
    ) -> CiHoldDisposition {
        let escalation = escalation_route(&poll.identity, origin_state, hold_reason);
        match self
            .ci_holds()
            .apply_poll(
                &poll.identity,
                observed_current,
                &poll.observation_id,
                complete,
                &escalation,
            )
            .await
        {
            Ok(CiHoldApply::Reset { .. }) => CiHoldDisposition::Proceed,
            Ok(CiHoldApply::Superseded) => {
                CiHoldDisposition::Absorbed(CiHoldAbsorption::Superseded)
            }
            Ok(CiHoldApply::IdentityAdvanced) => {
                CiHoldDisposition::Absorbed(CiHoldAbsorption::IdentityAdvanced)
            }
            Ok(CiHoldApply::Held { poll_count }) => {
                CiHoldDisposition::Absorbed(CiHoldAbsorption::Held { poll_count })
            }
            Ok(CiHoldApply::Escalated {
                poll_count,
                route_inserted,
            }) => {
                tracing::warn!(
                    repository = %poll.identity.repository_id,
                    pr = poll.identity.pr_number,
                    head = %poll.identity.pr_head_sha,
                    lane = poll.identity.lane.as_str(),
                    poll_count,
                    route_inserted,
                    "ci hold: bounded hold spent; escalated to one diagnose-only run-absent route"
                );
                // The route and its lease are already durable. What is left is
                // the Lead session — and skipping it here is how an escalation
                // becomes a wedge with better paperwork: the lane withholds the
                // legacy path (correctly, there is an adjudication pending) and
                // then nothing adjudicates.
                self.dispatch_escalated_hold(task_id, &escalation).await;
                CiHoldDisposition::Absorbed(CiHoldAbsorption::Escalated { route_inserted })
            }
            Ok(CiHoldApply::AlreadyEscalated) => {
                // Re-drive the dispatch, not just the escalation.
                //
                // The route and its open lease are written by the ledger's
                // transaction; the Lead session is not, and cannot be — it is a
                // board write and a provider-independent side effect. So the
                // escalating poll can commit the lease and then fail to dispatch
                // (task row unreadable, arbitration store down), and every later
                // poll lands here. Without this the lease stays open forever
                // with nothing adjudicating it — and because the lease is
                // head-scoped, it would also block every OTHER Tier-2 route for
                // that head.
                //
                // Costs nothing when it already worked: `dispatch_ci_tier2_lead`
                // finds the unconsumed arbitration row and answers
                // `AlreadyInFlight` without creating a second session.
                self.dispatch_escalated_hold(task_id, &escalation).await;
                CiHoldDisposition::Absorbed(CiHoldAbsorption::AlreadyEscalated)
            }
            Ok(CiHoldApply::NotFound) => {
                // The streak row vanished between the two transactions. Nothing
                // in the repository deletes one while a reserved observation
                // remains, so this is a fault rather than a race; hold.
                tracing::warn!(
                    repository = %poll.identity.repository_id,
                    pr = poll.identity.pr_number,
                    "ci hold: streak row absent at apply time"
                );
                CiHoldDisposition::Absorbed(CiHoldAbsorption::LedgerUnavailable)
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "ci hold: could not apply a poll result; holding this lane for one poll"
                );
                CiHoldDisposition::Absorbed(CiHoldAbsorption::LedgerUnavailable)
            }
        }
    }

    /// Turn an escalated hold's durable route into one Lead session.
    ///
    /// The handoff is rebuilt from the escalation payload — which is what the
    /// ledger was asked to write — with exactly **one** value read back from
    /// the row: the Tier-2 lease id. That one is not a `SELECT` proving that it
    /// inverts an `INSERT`; it is the only field whose payload value can differ
    /// from the durable one, and both paths into this function can produce that
    /// difference:
    ///
    /// * [`escalation_route`] mints `tier2_lease_id` fresh on **every** call,
    ///   and `insert_escalation_route` writes it `ON CONFLICT DO NOTHING`. When
    ///   a run-absent route for this identity already existed
    ///   (`route_inserted == false`), the stored lease is the pre-existing
    ///   row's and this poll's minted id names no row at all.
    /// * The [`CiHoldApply::AlreadyEscalated`] re-drive — the recovery for "the
    ///   lease committed but the dispatch did not" — carries a payload minted
    ///   after the row was written, so its id is always the wrong one.
    ///
    /// Binding the payload's id in either case is not a cosmetic miss.
    /// `attach_lead_session` is fenced on the stored lease id, so the route
    /// keeps `lead_session_id` NULL — `unapplied_lead_results` then reads
    /// "quiescent" with an adjudication in flight — and the `tier2_lease_id`
    /// the directive carries is the id the supervisor hands
    /// `resolve_tier2_lease`, which is fenced on the same column. A Lead
    /// session dispatched under a lease id no row holds can therefore never
    /// have its result applied: the escalation would spend a session and stay
    /// wedged, which is the exact failure this dispatch exists to prevent.
    ///
    /// A row that is absent, or whose lease is no longer open, dispatches
    /// **nothing**. There is no lease to bind to and no lease to resolve under,
    /// so a session started here could only be discarded — and the absent case
    /// is real: `insert_escalation_route` also conflicts when a *different* row
    /// on this subject already holds the head's Tier-2 lease, and that row's
    /// adjudication is the one that owns the head.
    ///
    /// `evidence_references` deliberately omits a run id: there is none, and a
    /// placeholder would be a handle Lead could quote to look grounded while
    /// citing nothing.
    async fn dispatch_escalated_hold(&self, task_id: &str, escalation: &CiHoldEscalationRoute) {
        let task = match self.task_repo().get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    %error,
                    task_id,
                    "ci hold: could not load the task to adjudicate an escalated hold"
                );
                return;
            }
        };
        let subject = &escalation.reservation.subject;
        let key = &escalation.reservation.provider_action_key;
        let route = match CiRouteAttemptRepository::new(self.db.clone())
            .get(subject, key)
            .await
        {
            Ok(Some(route)) => route,
            Ok(None) => {
                tracing::warn!(
                    task_id,
                    key = %key,
                    "ci hold: the escalated route names no row on this subject, so the \
                     head's Tier-2 lease is held elsewhere; dispatching nothing"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    task_id,
                    key = %key,
                    "ci hold: could not read the escalated route's lease; the next poll \
                     lands on `AlreadyEscalated` and retries the dispatch"
                );
                return;
            }
        };
        let lease_id = match (route.holds_open_tier2_lease(), route.tier2_lease_id) {
            (true, Some(lease_id)) => lease_id,
            _ => {
                tracing::warn!(
                    task_id,
                    key = %key,
                    "ci hold: the escalated route holds no open Tier-2 lease, so a Lead \
                     session dispatched here could bind to nothing and resolve nothing"
                );
                return;
            }
        };
        let identity = &escalation.reservation.identity;
        let mut references = vec![identity.pr_head_sha.clone(), identity.run_head_sha.clone()];
        if let Some(dequeue) = &identity.dequeue_id {
            references.push(dequeue.clone());
        }
        references.retain(|reference| !reference.trim().is_empty());
        references.sort();
        references.dedup();
        let handoff = CiTier2Handoff {
            subject: escalation.reservation.subject.clone(),
            provider_action_key: escalation.reservation.provider_action_key.clone(),
            tier2_lease_id: lease_id,
            identity: identity.clone(),
            origin_state: escalation.reservation.origin_state,
            reason: escalation.tier2_reason,
            evidence_references: references,
            // No blocking-check set was ever enumerated — that is what the hold
            // was waiting for — so there is no command corpus, and every repair
            // on this route would degrade to a diagnosis even if the route did
            // not already forbid one outright.
            repository_commands: Vec::new(),
        };
        let _ = self
            .dispatch_ci_tier2_lead(&task, &handoff, task.pr_url.as_deref())
            .await;
    }
}

/// The route an escalating hold inserts, precomputed so the ledger can write it
/// inside the same transaction that sets `escalated_at`.
///
/// Its identity is the **run-absent** one: `run_id` is `None`, `run_head_sha` is
/// the observed head, and the dequeue id is the lane's real one. No sentinel is
/// constructible — `CiEvidenceIdentity::run_id` is an `Option` and the database
/// carries `CHECK (run_id IS NULL OR run_id > 0)` behind it.
///
/// The keys are derived by the same functions every other route uses, so an
/// escalated hold and a directly-routed irrecoverable reason on one head land
/// on **one** row rather than two: same subject, same lane, same PR, same head,
/// same absent run, same dequeue, same `ask_lead` action, therefore the same
/// `provider_action_key`.
///
/// # The class, action, and Tier-2 reason are CLASSIFIED, not asserted
///
/// They come from running the real classifier over
/// [`CiCapture::escalated_hold`], not from three literals written here. That is
/// the difference between the ledger persisting what the contract decided and
/// the ledger persisting a second, unenforced opinion that happens to agree
/// today. If `classify` ever stopped routing an escalated hold to a
/// diagnose-only run-absent Tier 2, this function would change with it rather
/// than silently disagreeing.
fn escalation_route(
    identity: &CiHoldIdentity,
    origin_state: CiOriginState,
    hold_reason: CiIncompleteReason,
) -> CiHoldEscalationRoute {
    let evidence = CiEvidenceIdentity {
        lane: identity.lane,
        pr_number: identity.pr_number,
        pr_head_sha: identity.pr_head_sha.clone(),
        run_id: None,
        run_head_sha: identity.pr_head_sha.clone(),
        dequeue_id: identity.dequeue_id.clone(),
    };
    // A hold has no blocking-check set — that is what "incomplete" means — so
    // the transient fingerprint is over the empty set. It is stable, it is
    // distinct from any real failure signature, and it charges nothing: the
    // action is `ask_lead`, which `CiAction::charges_budget` excludes.
    let fingerprint = super::ci_routing::transient_fingerprint(identity.lane, &[]);

    let decision = classify(&CiObservation {
        evidence: &evidence,
        observed_current: &evidence,
        capture: CiCapture::escalated_hold(hold_reason),
    });
    debug_assert!(
        decision.requires_run_absent_identity() && decision.is_diagnose_only(),
        "an escalated hold must classify to a diagnose-only run-absent Tier 2"
    );
    let action = decision.action();
    CiHoldEscalationRoute {
        reservation: CiRouteReservation {
            subject: identity.subject.clone(),
            provider_action_key: provider_action_key(&identity.subject, &evidence, action),
            identity: evidence,
            origin_state,
            class: decision.class(),
            action,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(
                &identity.subject,
                &CiEvidenceIdentity {
                    lane: identity.lane,
                    pr_number: identity.pr_number,
                    pr_head_sha: identity.pr_head_sha.clone(),
                    run_id: None,
                    run_head_sha: identity.pr_head_sha.clone(),
                    dequeue_id: identity.dequeue_id.clone(),
                },
                &fingerprint,
            ),
            head_budget_key: head_budget_key(
                &identity.subject,
                identity.pr_number,
                &identity.pr_head_sha,
            ),
        },
        // PROPOSED, not authoritative. `insert_escalation_route` writes this
        // `ON CONFLICT DO NOTHING`, so it becomes the row's lease only when this
        // poll is the one that inserts the row; every other call — a conflicting
        // insert, and every `AlreadyEscalated` re-drive, both of which mint a
        // fresh one here — leaves a durable lease that this value does not name.
        // That is why `dispatch_escalated_hold` binds the lease id it reads back
        // from the row rather than this one.
        tier2_lease_id: uuid::Uuid::now_v7().to_string(),
        tier2_lease_key: tier2_lease_key(
            &identity.subject,
            identity.pr_number,
            &identity.pr_head_sha,
        ),
        // `None` is unreachable — `tier2_diagnose_only` always names one — and a
        // fallback literal here would be exactly the shadow copy this function
        // exists to avoid, so it degrades to the same value the classifier
        // would have produced rather than inventing a different one.
        tier2_reason: decision
            .tier2_reason()
            .unwrap_or(CiTier2Reason::EvidenceUnknown),
    }
}
