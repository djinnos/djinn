//! In-flight projection for ledger-dispatched refinement intents.
//!
//! The durable intent ledger is the dispatch authority for a run that has a run
//! identity, but the *outcome* path — the only writer of the successor intent —
//! is reached through the coordinator's disposable `refinement_sessions` /
//! `active_refinements` projections. Only the legacy map-driven dispatcher ever
//! populated those, so a ledger-dispatched role was never observed finishing:
//! `process_refinement_outcome` was unreachable, `complete_refinement_intent`
//! was never called, and every durable run stalled at `materialized` with a
//! NULL stop tag (0 intents advanced past `materialized` fleet-wide in a week).
//!
//! This module rebuilds that projection from the durable ledger on every tick,
//! so the phase in flight is always named by the ledger and the coordinator's
//! existing monitoring — outcome application, task close, dispatch-retry cap,
//! and the execution watchdog — applies to durable runs exactly as it did to
//! legacy ones.

use djinn_core::models::Proposal;
use djinn_core::refinement_liveness::RefinementPhase as DurablePhase;
use djinn_db::SessionRepository;

use super::actor::CoordinatorActor;
use super::refinement::{RefinementLoopState, RefinementPhase};
use super::refinement_dispatch::RefinementSession;
use djinn_core::clock::{Clock, SystemClock};

/// The exact ledger work whose in-flight projection is being (re)registered.
pub(super) struct DurableInflightRegistration<'a> {
    pub run_id: &'a str,
    pub generation: i32,
    pub proposal: &'a Proposal,
    pub phase: DurablePhase,
    pub round: i32,
    pub task_id: &'a str,
}

fn local_phase(phase: DurablePhase) -> RefinementPhase {
    match phase {
        DurablePhase::AdversaryAttack => RefinementPhase::AdversaryAttack,
        DurablePhase::AdvocateRevision => RefinementPhase::AdvocateRevision,
        DurablePhase::JudgeAdjudication => RefinementPhase::JudgeAdjudication,
    }
}

impl CoordinatorActor {
    /// Ensure the disposable projections name this materialized intent as the
    /// run's in-flight work, and report whether its role agent already started.
    ///
    /// Idempotent and monotonic: an existing projection that already tracks this
    /// exact (run, generation, task, phase) is left untouched, so per-tick
    /// replay never resets the dispatch clock, the cached session-start instant,
    /// or a phase the outcome path has already advanced.
    ///
    /// Returns `true` when an agent session row exists for `task_id` — i.e. the
    /// role ran and what the run is owed is its outcome, not another dispatch.
    /// A DB read failure reports `true` (fail-safe): a transient blip must not
    /// re-enqueue a role that may be mid-flight.
    pub(super) async fn register_durable_refinement_inflight(
        &mut self,
        registration: DurableInflightRegistration<'_>,
    ) -> bool {
        let DurableInflightRegistration {
            run_id,
            generation,
            proposal,
            phase,
            round,
            task_id,
        } = registration;
        let phase = local_phase(phase);

        // A durable poll can race a newer in-memory outcome/wake projection.
        // The caller's run ID alone is insufficient authority to overwrite that
        // projection: all generation, phase, and round coordinates must agree.
        if self.active_refinements.get(run_id).is_some_and(|current| {
            current.run_id != run_id
                || current.generation != generation
                || current.phase != phase
                || current.current_round != round
        }) {
            tracing::debug!(
                run_id,
                generation,
                ?phase,
                round,
                "ignored stale durable in-flight projection registration"
            );
            return true;
        }

        let tracked = self.refinement_sessions.get(run_id).is_some_and(|session| {
            session.run_id == run_id
                && session.generation == generation
                && session.task_id == task_id
                && session.phase == phase
        });

        let (role_ran, model_id) = self.observe_refinement_role_session(task_id).await;

        if tracked {
            // Cache the first observed session start so the execution budget is
            // measured from when the agent actually began, not from dispatch.
            if role_ran
                && let Some(session) = self.refinement_sessions.get_mut(run_id)
                && session.session_started_at.is_none()
            {
                session.session_started_at = Some(SystemClock::new().now_instant());
            }
            return role_ran;
        }

        // The ledger is the authority for which phase/round is in flight; a
        // projection rebuilt alongside a fresh in-flight session must agree with
        // it, or the outcome correlation fence rejects every observation and the
        // run stalls exactly as it did before.
        let captured_snapshot_seq = djinn_db::ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .refinement_run_captured_snapshot_seq(run_id)
        .await
        .unwrap_or_default();
        let mut state = RefinementLoopState::new(&proposal.id, proposal.latest_revision_seq)
            .with_run_identity(run_id.to_owned(), generation)
            .with_captured_snapshot_seq(captured_snapshot_seq)
            .with_attributed_user(proposal.refinement_owner_user_id.clone());
        state.phase = phase;
        state.current_round = round;
        if let Some(existing) = self.active_refinements.get(run_id)
            && existing.generation == generation
            && existing.phase == phase
            && existing.current_round == round
        {
            // Preserve the live projection's accumulated loop state (dry-round
            // counters, objection signatures, spawn budget) rather than
            // resetting it from the ledger.
            state = existing.clone();
        }
        self.active_refinements.insert(run_id.to_owned(), state);

        let now = SystemClock::new().now_instant();
        self.refinement_sessions.insert(
            run_id.to_owned(),
            RefinementSession {
                run_id: run_id.to_owned(),
                generation,
                task_id: task_id.to_owned(),
                phase,
                dispatched_at: now,
                session_started_at: role_ran.then_some(now),
                model_id: model_id.unwrap_or_default(),
            },
        );
        tracing::debug!(
            run_id = %run_id,
            task_id = %task_id,
            ?phase,
            round,
            role_ran,
            "registered ledger-dispatched refinement role as in-flight"
        );
        role_ran
    }

    /// Whether an agent session row exists for a refinement role task, and the
    /// model the latest one ran on. Fails safe toward "started" so a DB blip
    /// never re-enqueues a possibly-running role.
    async fn observe_refinement_role_session(&self, task_id: &str) -> (bool, Option<String>) {
        let session_repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        match session_repo.list_for_task(task_id).await {
            Ok(sessions) => {
                let model = sessions.last().map(|session| session.model_id.clone());
                (!sessions.is_empty(), model)
            }
            Err(error) => {
                tracing::warn!(
                    task_id = %task_id,
                    %error,
                    "failed to read refinement role sessions; assuming the role started"
                );
                (true, None)
            }
        }
    }
}
