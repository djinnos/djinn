// Startup recovery for refinement loops interrupted by a coordinator restart.
//
// Split out of `refinement_outcome.rs` to keep that file under the
// size-guard byte threshold. Owns the restart reconciliation pass:
//
//   1. Dangling refinements that legitimately converged and parked awaiting the
//      human's accept/reject review are restored from durable lifecycle data.
//   2. Refinements still mid-tribunal are RESUMED in place — their in-memory
//      `RefinementLoopState` is rebuilt from durable data (lifecycle events,
//      debate trail, refinement task rows) and re-inserted into
//      `active_refinements` so the driver continues the same run.
//   3. Only genuinely ambiguous/contradictory refinements (no coherent
//      round/phase can be derived) fall back to the historical behavior:
//      stamped `refinement_stop`/`Interrupted` and left restartable.

use djinn_core::{
    models::ProposalDebateTrail,
    refinement_liveness::RefinementStopReason,
};
use djinn_db::{ProposalRepository, TaskRepository, TerminalRefinementRunRequest};

use super::actor::CoordinatorActor;
use super::refinement::{RefinementConfig, RefinementLoopState, RefinementPhase, StopReason};
use super::refinement_outcome::entry_in_current_run;

/// Outcome of deriving a resumable phase/round for a mid-flight refinement from
/// durable data. `Resume` carries the reconstructed phase and 1-based round the
/// driver should continue at; `Ambiguous` means the durable trail could not be
/// mapped to a coherent tribunal position and the caller must fall back to the
/// historical `Interrupted` stamp rather than guess into a corrupt run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    Resume { phase: RefinementPhase, round: i32 },
    Ambiguous(&'static str),
}

/// Derive the phase/round a mid-flight refinement should resume at, purely from
/// durable data. No I/O — every input is pre-resolved so this is unit-testable.
///
/// The derivation mirrors the tribunal state machine (`Adversary → Advocate →
/// Judge` per round):
///
///   - A linked evidence spike ⇒ the run is parked awaiting evidence; restore
///     `AwaitingEvidence` so the evidence-completion path can resume it.
///   - Within the latest current-run round `R`:
///       * a Judge `needs_evidence` demand ⇒ `AwaitingEvidence`;
///       * a Judge verdict already ruled ⇒ a blocking verdict below the round
///         cap advances to round `R+1` `AdversaryAttack`; a ready verdict, or a
///         blocking verdict at the cap, is AMBIGUOUS (those should have written
///         a durable awaiting-review park — restored by the sibling path — so
///         reaching here means the process died mid-transition and we cannot
///         reconstruct the snapshot revision);
///       * Adversary objections filed, no verdict yet ⇒ `AdvocateRevision` when
///         any is blocking, else `JudgeAdjudication` (dry round);
///       * round opened but the Adversary never filed ⇒ `AdversaryAttack`.
///   - No current-run debate entries at all ⇒ round 1 `AdversaryAttack` (the
///     run had just (re)started; the opener never filed).
///
/// Re-dispatching the derived phase is idempotent from the tribunal's
/// perspective: only the Judge advances the round, entry reads are set-based
/// (not append-counted), and `process_advocate_outcome` guards double-applying a
/// revision with `new_revision_seq > current_revision_seq`.
pub(crate) fn derive_resume_plan(
    entries: &[ProposalDebateTrail],
    run_start: Option<&str>,
    head_revision_seq: i32,
    linked_spike: bool,
    max_rounds: i32,
) -> ResumePlan {
    let max_current_run_round = entries
        .iter()
        .filter(|e| entry_in_current_run(e, run_start))
        .map(|e| e.round)
        .max();

    // A linked evidence spike parks the whole run awaiting evidence, regardless
    // of the per-round trail. Restore the park so the evidence-completion path
    // (which looks the run up in `active_refinements`) can resume it.
    if linked_spike {
        let round = max_current_run_round.unwrap_or(1).max(1);
        return ResumePlan::Resume {
            phase: RefinementPhase::AwaitingEvidence,
            round,
        };
    }

    let Some(round) = max_current_run_round else {
        return ResumePlan::Resume {
            phase: RefinementPhase::AdversaryAttack,
            round: 1,
        };
    };
    let round = round.max(1);

    // Current-run entries for the latest round.
    let round_entries: Vec<&ProposalDebateTrail> = entries
        .iter()
        .filter(|e| e.round == round && entry_in_current_run(e, run_start))
        .collect();

    // 1. Judge demanded evidence this round → parked awaiting evidence.
    if round_entries
        .iter()
        .any(|e| e.kind == "needs_evidence" && e.agent_role == "judge" && e.body_metadata.is_some())
    {
        return ResumePlan::Resume {
            phase: RefinementPhase::AwaitingEvidence,
            round,
        };
    }

    // 2. Judge already ruled this round. Prefer the latest verdict written
    //    against the current head revision, else the latest verdict overall
    //    (mirrors `select_current_run_verdict`).
    let is_verdict = |e: &&ProposalDebateTrail| e.kind == "verdict" && e.agent_role == "judge";
    let verdict = round_entries
        .iter()
        .copied()
        .filter(is_verdict)
        .filter(|e| e.against_revision_seq == head_revision_seq)
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
        .or_else(|| {
            round_entries
                .iter()
                .copied()
                .filter(is_verdict)
                .max_by(|a, b| a.created_at.cmp(&b.created_at))
        });

    if let Some(verdict) = verdict {
        if !verdict.blocking {
            // A ready verdict should have written a durable awaiting-review park
            // (restored by `try_restore_awaiting_review` before we get here).
            // Reaching this branch means the process died between the verdict
            // and the park write — we can't fabricate the snapshot revision.
            return ResumePlan::Ambiguous("ready judge verdict without awaiting-review park");
        }
        if round >= max_rounds {
            // A blocking verdict at the round cap escalates to a human-review
            // park; without that durable park we can't resume coherently.
            return ResumePlan::Ambiguous("blocking judge verdict at round cap without park");
        }
        // Blocking verdict below the cap → advance to the next Adversary round.
        return ResumePlan::Resume {
            phase: RefinementPhase::AdversaryAttack,
            round: round + 1,
        };
    }

    // 3. Adversary opened this round; Judge hasn't ruled yet.
    let adversary_objections: Vec<&ProposalDebateTrail> = round_entries
        .iter()
        .copied()
        .filter(|e| e.kind == "objection" && e.agent_role == "adversary")
        .collect();
    if !adversary_objections.is_empty() {
        let phase = if adversary_objections.iter().any(|e| e.blocking) {
            RefinementPhase::AdvocateRevision
        } else {
            RefinementPhase::JudgeAdjudication
        };
        return ResumePlan::Resume { phase, round };
    }

    // 4. Round started (some non-adversary entry exists) but the Adversary
    //    never filed → resume at the Adversary opener for this round.
    ResumePlan::Resume {
        phase: RefinementPhase::AdversaryAttack,
        round,
    }
}

impl CoordinatorActor {
    /// Startup reconciliation for refinements interrupted by a restart.
    ///
    /// Runs once before the message loop. Every DB-dangling refinement (more
    /// `refinement_start` than `refinement_stop` lifecycle rows) either:
    ///
    /// - is restored to its parked `AwaitingHumanReview` state — when the
    ///   tribunal had legitimately converged and written a durable
    ///   `refinement_awaiting_review` lifecycle row, and nobody has edited the
    ///   spec since (the head revision still equals the parked refined seq). A
    ///   converged park is a valid, human-actionable result; stamping it
    ///   `Interrupted` on every deploy would silently destroy the judge's work
    ///   and force a full re-run; or
    /// - is stamped `refinement_stop` with [`StopReason::Interrupted`] — for a
    ///   refinement genuinely lost mid-tribunal (no awaiting-review park after
    ///   the latest start), leaving the proposal restartable.
    pub(super) async fn recover_interrupted_refinements(&mut self) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let dangling = match proposal_repo.dangling_refinement_proposal_ids().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to query dangling refinements for startup recovery"
                );
                return;
            }
        };
        if dangling.is_empty() {
            return;
        }
        tracing::info!(
            count = dangling.len(),
            "Reconciling refinements interrupted by restart"
        );
        for proposal_id in dangling {
            if self.active_refinements.contains_key(&proposal_id) {
                continue;
            }
            // A legitimately-converged park is restored to AwaitingHumanReview.
            if self.try_restore_awaiting_review(&proposal_id).await {
                continue;
            }
            // A mid-tribunal run is resumed in place from durable data so it
            // keeps running across the restart.
            if self.try_resume_mid_flight(&proposal_id).await {
                continue;
            }
            // Only an exact active run may be terminalized. Legacy dangling
            // lifecycle rows deliberately receive no proposal-scoped stop write:
            // they cannot identify the generation that is safe to mutate.
            match proposal_repo.load_active_refinement_runs().await {
                Ok(runs) => {
                    if let Some(run) = runs.into_iter().find(|run| run.proposal_id == proposal_id) {
                        if let Err(error) = proposal_repo
                            .terminal_refinement_run(TerminalRefinementRunRequest {
                                run_id: run.run_id,
                                generation: run.generation,
                                reason: RefinementStopReason::Interrupted { detail: None },
                            })
                            .await
                        {
                            tracing::warn!(proposal_id = %proposal_id, %error,
                                "failed to terminalize ambiguous exact refinement run");
                        }
                    }
                }
                Err(error) => tracing::warn!(proposal_id = %proposal_id, %error,
                    "failed to load exact refinement run for recovery terminalization"),
            }
            tracing::info!(
                proposal_id = %proposal_id,
                "Stopped interrupted exact refinement run when reconstruction was ambiguous"
            );
        }
    }

    /// Attempt to resume a dangling refinement that was mid-tribunal when the
    /// process died. Rebuilds the in-memory [`RefinementLoopState`] from durable
    /// data (lifecycle events, debate trail, refinement task rows) and inserts
    /// it into `active_refinements` so `drive_active_refinements` continues the
    /// SAME run. Returns `true` when resumed, `false` when the state was
    /// ambiguous/contradictory or a required read failed (caller then falls back
    /// to the `Interrupted` stamp).
    ///
    /// Reconstruction sources:
    ///   - **phase / round**: [`derive_resume_plan`] over the current-run debate
    ///     trail (scoped by the latest `refinement_start` boundary), plus the
    ///     proposal's linked evidence spike.
    ///   - **current revision**: the proposal head (`latest_revision_seq`).
    ///   - **snapshot revision** (revert-on-reject baseline): the `seq` stamped
    ///     on the latest `refinement_start` lifecycle row.
    ///   - **attribution**: the proposal.s durable
    ///     `refinement_owner_user_id`, never tribunal task rows or the proposal
    ///     author.
    ///   - **spawn budget**: the count of this run's refinement task rows, so the
    ///     spawn cap still binds across the restart (conservative — includes a
    ///     re-dispatched orphan, which only tightens the cap).
    ///
    /// Any orphaned OPEN refinement task from this run (a role session that was
    /// running at kill time, whose slot the pool no longer tracks) is closed via
    /// the existing `close_refinement_task` machinery. The driver then
    /// re-dispatches the reconstructed phase on the next tick; re-running the
    /// phase is idempotent (see [`derive_resume_plan`]).
    async fn try_resume_mid_flight(&mut self, proposal_id: &str) -> bool {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to load proposal for mid-flight refinement resume; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        // One revisions read supplies the run boundary, the snapshot seq, and
        // the converged-park guard below.
        let revisions = match proposal_repo.revisions(proposal_id).await {
            Ok(revisions) => revisions,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read revisions for resume; falling back to interrupted stamp"
                );
                return false;
            }
        };

        // Current-run boundary = the latest `refinement_start`. A dangling
        // refinement always has one; `None` is treated as "all in-run".
        let latest_start = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "refinement_start");
        let run_start = latest_start.map(|r| r.created_at.clone());

        // Converged-park guard. A `refinement_awaiting_review` row in the current
        // run means the tribunal already converged/escalated and the park is
        // owned by `try_restore_awaiting_review`. Reaching here means that path
        // DECLINED to restore it (stale spec, or missing snapshot/refined seqs),
        // so we preserve the historical `Interrupted` stamp rather than
        // auto-starting a fresh tribunal over a spec the human may have edited.
        if let Some(start) = latest_start
            && revisions.iter().any(|r| {
                r.event_kind == "refinement_awaiting_review" && r.created_at >= start.created_at
            })
        {
            tracing::info!(
                proposal_id = %proposal_id,
                "Converged awaiting-review park present but not restorable; \
                 preserving interrupted stamp (no mid-flight resume)"
            );
            return false;
        }

        // Snapshot revision seq = the head at the moment refinement started,
        // recorded as the `seq` of the latest `refinement_start` lifecycle row.
        // Falls back to the current head (a reject-revert then no-ops).
        let snapshot_revision_seq = latest_start
            .map(|r| r.seq)
            .unwrap_or(proposal.latest_revision_seq);

        let entries = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read debate trail for resume; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        let config = RefinementConfig::default();
        let head_revision_seq = proposal.latest_revision_seq;
        let linked_spike = proposal.linked_spike_task_id.is_some();

        let (phase, round) = match derive_resume_plan(
            &entries,
            run_start.as_deref(),
            head_revision_seq,
            linked_spike,
            config.max_rounds,
        ) {
            ResumePlan::Resume { phase, round } => (phase, round),
            ResumePlan::Ambiguous(reason) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    reason,
                    "Mid-flight refinement reconstruction ambiguous; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        // Reconstruct spawn budget and orphaned open tasks from this run.s task
        // rows. Attribution itself comes only from durable proposal state.
        let (_task_attributed_user, run_task_count, orphaned_open_task_ids) = self
            .reconstruct_run_refinement_tasks(proposal_id, run_start.as_deref())
            .await;
        let attributed_user_id = proposal.refinement_owner_user_id.clone();

        let mut state = RefinementLoopState::with_config(proposal_id, head_revision_seq, config)
            .with_attributed_user(attributed_user_id);
        state.phase = phase;
        state.current_round = round.max(1);
        state.snapshot_revision_seq = snapshot_revision_seq;
        // Conservative: pre-restart dispatched tasks count against the spawn cap
        // so it still binds. record_spawn on re-dispatch only tightens it.
        state.total_spawns = run_task_count;

        self.active_refinements
            .insert(proposal_id.to_string(), state);

        // Reconcile orphaned in-flight work: close any open refinement task from
        // this run so it does not linger on the board; the reconstructed phase
        // is re-dispatched by the driver on the next tick (idempotent).
        for task_id in &orphaned_open_task_ids {
            self.close_refinement_task(
                task_id,
                "refinement role session orphaned by coordinator restart; phase re-dispatched",
            )
            .await;
        }

        tracing::info!(
            proposal_id = %proposal_id,
            phase = ?phase,
            round,
            snapshot_revision_seq,
            total_spawns = run_task_count,
            orphaned_open_tasks = orphaned_open_task_ids.len(),
            "Resumed mid-flight refinement across restart (in-place, same run)"
        );
        true
    }

    /// Scan this run's refinement task rows to reconstruct: the attributed user
    /// (most recent task's `created_by_user_id`), the spawn count (all this-run
    /// refinement tasks), and the ids of any still-OPEN (orphaned) tasks.
    ///
    /// Refinement tasks carry no structured proposal-id column; they are matched
    /// by the durable `for proposal {id},` marker their description always
    /// carries (see `create_refinement_task_with_context`) and scoped to the
    /// current run by `created_at > run_start`.
    async fn reconstruct_run_refinement_tasks(
        &self,
        proposal_id: &str,
        run_start: Option<&str>,
    ) -> (Option<String>, i32, Vec<String>) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let targets = proposal_repo.targets(proposal_id).await.unwrap_or_default();

        let event_bus2 = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus2);

        let marker = format!("for proposal {proposal_id},");
        let mut count = 0i32;
        let mut open_ids: Vec<String> = Vec::new();
        // (created_at, user_id) of the most recent this-run task with a user.
        let mut latest_user: Option<(String, String)> = None;

        for target in &targets {
            let tasks = match task_repo.list_by_project(&target.project_id).await {
                Ok(tasks) => tasks,
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        project_id = %target.project_id,
                        error = %e,
                        "Failed to list tasks for refinement resume reconstruction; skipping project"
                    );
                    continue;
                }
            };
            for task in tasks {
                if task.issue_type != "refinement" || !task.description.contains(&marker) {
                    continue;
                }
                if let Some(start) = run_start
                    && task.created_at.as_str() <= start
                {
                    continue;
                }
                count += 1;
                if task.status != "closed" {
                    open_ids.push(task.id.clone());
                }
                {
                    let uid = task.created_by_user_id.clone();
                    let is_newer = latest_user
                        .as_ref()
                        .map(|(at, _)| task.created_at > *at)
                        .unwrap_or(true);
                    if is_newer {
                        latest_user = Some((task.created_at.clone(), uid));
                    }
                }
            }
        }

        (latest_user.map(|(_, uid)| uid), count, open_ids)
    }

    /// Attempt to restore a dangling refinement that was parked awaiting the
    /// human's accept/reject review from durable lifecycle data. Returns `true`
    /// when the parked state was rebuilt into `self.active_refinements` (so the
    /// caller must NOT stamp it interrupted), `false` otherwise.
    ///
    /// Falls back to `false` (interrupted stamp) when the proposal is not parked
    /// awaiting review, or when the live spec has moved on since the park (head
    /// `latest_revision_seq` no longer equals the parked `refined_revision_seq`),
    /// because a human/agent edit invalidates the converged result.
    async fn try_restore_awaiting_review(&mut self, proposal_id: &str) -> bool {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let park = match proposal_repo.parked_awaiting_review(proposal_id).await {
            Ok(Some(park)) => park,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read awaiting-review park during startup recovery; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        let Some(refined_revision_seq) = park.refined_revision_seq else {
            tracing::warn!(
                proposal_id = %proposal_id,
                "Awaiting-review park missing refined_revision_seq; \
                 falling back to interrupted stamp"
            );
            return false;
        };

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to load proposal during awaiting-review restore; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        // If the spec moved on since the park, the converged result no longer
        // matches the live head — treat as interrupted rather than restoring a
        // stale review.
        if proposal.latest_revision_seq != refined_revision_seq {
            tracing::info!(
                proposal_id = %proposal_id,
                parked_seq = refined_revision_seq,
                head_seq = proposal.latest_revision_seq,
                "Awaiting-review park is stale (spec advanced); stamping interrupted"
            );
            return false;
        }

        // The snapshot seq is the pre-refinement baseline (revert target on
        // reject). Fall back to the refined seq if the park predates snapshot
        // recording — a reject then reverts to the refined head, a no-op.
        let snapshot_revision_seq = park.snapshot_revision_seq.unwrap_or(refined_revision_seq);

        // Restore the round counter from the debate trail so a reject-with-
        // feedback re-loop resumes at the correct round. Scoped to the current
        // run (see `entry_in_current_run`) so a prior interrupted run's
        // round-numbered entries don't inflate the restored round. Best-effort:
        // defaults to round 1 when the trail is empty or unreadable.
        let run_start = self.latest_refinement_run_start(proposal_id).await;
        let current_round = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => entries
                .iter()
                .filter(|e| entry_in_current_run(e, run_start.as_deref()))
                .map(|e| e.round)
                .max()
                .unwrap_or(1),
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read debate trail for round restore; defaulting to round 1"
                );
                1
            }
        };

        let stop_reason = park.stop_reason.as_deref().and_then(StopReason::from_tag);

        let state = RefinementLoopState::restored_awaiting_review(
            proposal_id,
            refined_revision_seq,
            snapshot_revision_seq,
            current_round,
            proposal.author_user_id.clone(),
            stop_reason,
        );
        self.active_refinements
            .insert(proposal_id.to_string(), state);

        tracing::info!(
            proposal_id = %proposal_id,
            refined_revision_seq,
            snapshot_revision_seq,
            current_round,
            "Restored refinement parked awaiting human review across restart"
        );
        true
    }
}
